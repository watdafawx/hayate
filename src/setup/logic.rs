//! Pure parts of self-install: where this exe runs from, the install offer's
//! prefs file, what an install writes (shortcuts, update.ini, the toast
//! activator key) and the uninstall's cleanup command. No I/O here, so all of
//! it is unit-tested. install.ps1 writes the same footprint; the tests below
//! read install.ps1 and fail when the two drift apart.

use crate::update::logic::{inside_or_same, norm_path, same_dir, source_allowed};

/// COM class Windows starts when a toast in the notification centre is
/// clicked. Keep in sync with install.ps1 (`$toastClsid`).
pub const CLSID_STR: &str = "{8F49C037-2A7A-47A4-A7A0-715C9CF629B6}";
pub const CLSID: u128 = 0x8f49c037_2a7a_47a4_a7a0_715c9cf629b6;
/// HKCU-relative key of the activator (the only key an install writes).
pub fn clsid_key() -> String {
    format!(r"Software\Classes\CLSID\{CLSID_STR}")
}
/// First argument of a COM toast launch (`LocalServer32` value); COM may
/// append `-Embedding`.
pub const TOAST_ARG: &str = "-ToastActivated";

/// Start-menu shortcut names an install may write (all removed on uninstall).
pub const LINK_MAIN: &str = "Hayate.lnk";
pub const LINK_ADMIN: &str = "Hayate (admin).lnk";
pub const LINK_PLAIN: &str = "Hayate (no admin).lnk";
/// Start-menu names before the Hayate rename: swept on every install and
/// uninstall, and migrated to the new names on an update.
pub const LEGACY_MENU_LINKS: [&str; 3] = ["Blink.lnk", "Blink (admin).lnk", "Blink (no admin).lnk"];
/// `LEGACY_MENU_LINKS[i]` becomes `MENU_LINKS[i]`.
pub const MENU_LINKS: [&str; 3] = [LINK_MAIN, LINK_ADMIN, LINK_PLAIN];
/// Every Start-menu shortcut name ever written (new and legacy): what uninstall removes.
pub fn all_menu_links() -> impl Iterator<Item = &'static str> {
    MENU_LINKS.into_iter().chain(LEGACY_MENU_LINKS)
}
/// in the user's Startup folder. Windows lists the file by this name; the
/// pre-rename `Blink.lnk` and its StartupApproved byte are migrated in v0.2.0.

pub const LINK_AUTO: &str = "Hayate.lnk";

#[derive(Clone, Debug, PartialEq)]
pub enum Where {
    /// `%LOCALAPPDATA%\Programs\Hayate\hayate.exe`: no offer; Settings can uninstall
    User,
    /// `%ProgramFiles%\Hayate` (install.ps1 -Machine): Settings or Installed apps uninstall it (UAC)
    Machine,
    /// anywhere else: the offer bar
    Portable,
}

pub fn classify(exe_dir: &str, user_dir: &str, machine_dir: &str) -> Where {
    if same_dir(&norm_path(exe_dir), &norm_path(user_dir)) {
        Where::User
    } else if !machine_dir.trim().is_empty() && same_dir(&norm_path(exe_dir), &norm_path(machine_dir)) {
        Where::Machine
    } else {
        Where::Portable
    }
}

/// [`classify`], and the pre-rename install folders count as installed too: a
/// bridged update runs from `Programs\Blink` / `ProgramFiles\Blink` until the
/// migration moves it (docs/rename-full.md).
pub fn classify_any(exe_dir: &str, user_dir: &str, machine_dir: &str, legacy_user: &str, legacy_machine: &str) -> Where {
    let w = classify(exe_dir, user_dir, machine_dir);
    if w != Where::Portable {
        return w;
    }
    if !legacy_user.trim().is_empty() && same_dir(&norm_path(exe_dir), &norm_path(legacy_user)) {
        Where::User
    } else if !legacy_machine.trim().is_empty() && same_dir(&norm_path(exe_dir), &norm_path(legacy_machine)) {
        Where::Machine
    } else {
        Where::Portable
    }
}

/// `%LOCALAPPDATA%\Hayate\setup.ini`: the install offer's answers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prefs {
    /// "Don't ask again"
    pub never: bool,
    /// "Not now": exe paths (normalised) the bar stays hidden for
    pub dismissed: Vec<String>,
    /// Settings > Taskbar: the Jump List is switched off (on by default)
    pub jumplist_off: bool,
}

pub fn parse_prefs(text: &str) -> Prefs {
    let mut p = Prefs::default();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "never" => p.never = matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
            "jumplist" => p.jumplist_off = matches!(v.to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off"),
            "dismissed" if !v.is_empty() => {
                let n = norm_path(v);
                if !p.dismissed.contains(&n) {
                    p.dismissed.push(n);
                }
            }
            _ => {}
        }
    }
    p
}

pub fn write_prefs(p: &Prefs) -> String {
    let mut s = String::from("; Hayate install offer (written by Hayate; safe to delete)\n");
    s.push_str(&format!("never = {}\n", p.never));
    s.push_str(&format!("jumplist = {}\n", !p.jumplist_off));
    // newest 16 only: the file never grows without bound
    for d in p.dismissed.iter().rev().take(16).rev() {
        s.push_str(&format!("dismissed = {d}\n"));
    }
    s
}

impl Prefs {
    pub fn dismiss(&mut self, exe: &str) {
        let n = norm_path(exe);
        self.dismissed.retain(|d| d != &n);
        self.dismissed.push(n);
    }
}

/// The in-window "Install Hayate" bar shows for a portable copy the user has
/// not answered for this exe path.
pub fn show_bar(w: &Where, prefs: &Prefs, exe: &str) -> bool {
    *w == Where::Portable && !prefs.never && !prefs.dismissed.contains(&norm_path(exe))
}

/// The bar, and also not while Hayate is already installed (per-user or
/// per-machine hayate.exe exists): a dev or downloaded copy then stays quiet;
/// Settings still offers "Install this copy".
pub fn offer(w: &Where, installed: bool, prefs: &Prefs, exe: &str) -> bool {
    !installed && show_bar(w, prefs, exe)
}

/// Auto-update default: the folder this exe runs from, only when it is a
/// publish.ps1 drop (hayate.exe + hayate.exe.sha256 + version.txt) outside
/// the install dir. Otherwise off.
pub fn update_default(exe_dir: &str, has_sha: bool, has_version: bool, install_dir: &str) -> Option<String> {
    (has_sha && has_version && source_allowed(exe_dir, install_dir).is_ok()).then(|| exe_dir.trim_end_matches(['\\', '/']).to_string())
}

/// What the user picked in the install panel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Choice {
    pub admin_default: bool,
    pub autostart: bool,
    pub update: bool,
    /// all-users install only: remove the copy installed just for this user
    pub replace_user: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    /// file name inside the Start-menu Programs (or Startup) folder
    pub name: &'static str,
    pub args: &'static str,
    pub description: &'static str,
    /// System.AppUserModel.ID; None: no ID (the Startup link, like install.ps1)
    pub app_id: Option<String>,
    /// SLDF_RUNAS_USER: the shell asks UAC on every start
    pub runas: bool,
    /// stamps System.AppUserModel.ToastActivatorCLSID (Hayate.lnk only)
    pub toast: bool,
}

/// Start-menu links for a choice, plus the names to delete (the other variant).
/// Only Hayate.lnk carries Hayate's own app ID and the toast activator; the
/// second link gets an ID of its own, else Windows names toasts after it.
pub fn menu_links(admin_default: bool, aumid: &str) -> (Vec<Link>, Vec<&'static str>) {
    let main = Link {
        name: LINK_MAIN,
        args: "",
        description: if admin_default { "Hayate task manager (asks for UAC)" } else { "Hayate task manager" },
        app_id: Some(aumid.to_string()),
        runas: admin_default,
        toast: true,
    };
    if admin_default {
        let plain = Link { name: LINK_PLAIN, args: "", description: "Hayate task manager (no admin)", app_id: Some(format!("{aumid}.User")), runas: false, toast: false };
        (vec![main, plain], with_legacy(LINK_ADMIN))
    } else {
        let admin = Link { name: LINK_ADMIN, args: "", description: "Hayate task manager (asks for UAC)", app_id: Some(format!("{aumid}.Admin")), runas: true, toast: false };
        (vec![main, admin], with_legacy(LINK_PLAIN))
    }
}

/// What an update does with the pre-rename Start-menu shortcuts: rename a
/// legacy one to its new name (keeps its app ID and toast activator), or just
/// drop it when the new name already exists.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuMove {
    Rename(&'static str, &'static str),
    DropLegacy(&'static str),
}
pub fn menu_migration(exists: impl Fn(&str) -> bool) -> Vec<MenuMove> {
    LEGACY_MENU_LINKS
        .iter()
        .zip(MENU_LINKS)
        .filter(|(old, _)| exists(old))
        .map(|(old, new)| if exists(new) { MenuMove::DropLegacy(old) } else { MenuMove::Rename(old, new) })
        .collect()
}

fn with_legacy(other: &'static str) -> Vec<&'static str> {
    std::iter::once(other).chain(LEGACY_MENU_LINKS).collect()
}

/// The Startup-folder link: the plain exe, `--resident`, never elevated.
pub fn autostart_link() -> Link {
    Link { name: LINK_AUTO, args: "--resident", description: "Hayate task manager (starts hidden at sign-in)", app_id: None, runas: false, toast: false }
}

/// update.ini as install.ps1 writes it.
pub fn update_ini(source: &str, install_dir: &str) -> String {
    format!(
        "; Hayate auto-update (written by Hayate's installer). Hayate reads it when it starts.\n; Turn off: set enabled = false (or delete this file), then restart Hayate.\n[update]\nenabled = true\nsource = {source}\ninstall_dir = {install_dir}\n"
    )
}

/// `LocalServer32` value: `"<exe>" -ToastActivated`, plus the private
/// test-environment args (HAYATE_DATA_DIR / HAYATE_MULTI) when the installer
/// itself ran with them, so a rig test instance gets its own toasts back.
pub fn local_server(exe: &str, extra: &[String]) -> String {
    let mut a = vec![TOAST_ARG.to_string()];
    a.extend(extra.iter().cloned());
    crate::update::logic::command_line(exe, &a)
}

pub fn is_toast_launch(args: &[String]) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case(TOAST_ARG))
}

/// A folder an uninstall may delete as a whole: absolute (drive letter, no
/// UNC), at least three levels deep (never `C:\` or `C:\Users`).
pub fn safe_to_remove(p: &str) -> bool {
    let n = norm_path(p);
    let b = n.as_bytes();
    let abs = b.len() > 3 && b[1] == b':' && b[2] == b'\\';
    abs && n.trim_end_matches('\\').split('\\').count() >= 3
}

/// The data dir may go only when it is Hayate's own (leaf `Hayate…`), never a
/// HAYATE_DATA_DIR override pointing at some shared folder.
pub fn data_dir_removable(data_dir: &str, install_dir: &str) -> bool {
    let n = norm_path(data_dir);
    let leaf = n.trim_end_matches('\\').rsplit('\\').next().unwrap_or("");
    // not a parent of the install dir, nor inside it (that goes anyway)
    leaf.starts_with("hayate") && safe_to_remove(data_dir) && !inside_or_same(install_dir, data_dir) && !inside_or_same(data_dir, install_dir)
}

/// The same rule for the pre-rename data dir (`Blink`), which an uninstall removes
/// only when the user asked for the settings to go.
pub fn legacy_data_dir_removable(data_dir: &str, install_dir: &str) -> bool {
    let n = norm_path(data_dir);
    let leaf = n.trim_end_matches('\\').rsplit('\\').next().unwrap_or("");
    leaf.eq_ignore_ascii_case("blink") && safe_to_remove(data_dir) && !inside_or_same(install_dir, data_dir) && !inside_or_same(data_dir, install_dir)
}

/// Uninstall cannot delete the running hayate.exe, but Windows lets a running
/// exe be renamed (the self-updater's hayate.old.exe swap does the same). It
/// moves to `%TEMP%\Hayate-uninstalled\hayate-<pid>.exe`, so the install dir
/// goes at once, in-process: no helper process, no cmd.exe, no reboot key.
/// The next Hayate start (any copy) or Storage Sense deletes the parked file.
pub const PARK_DIR: &str = "Hayate-uninstalled";

pub fn parked_exe(temp: &str, pid: u32) -> String {
    format!("{}\\{PARK_DIR}\\hayate-{pid}.exe", temp.trim_end_matches(['\\', '/']))
}

/// Files in the park dir a sweep may delete: only `hayate-<digits>.exe`.
pub fn is_parked_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.strip_prefix("hayate-").and_then(|r| r.strip_suffix(".exe")).is_some_and(|d| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit()))
}

// ---- Installed apps (Add/Remove Programs) and `hayate.exe --uninstall` ----------

/// `hayate.exe --uninstall`: what Windows' Installed apps list runs. No window
/// of Hayate's own: two native boxes ask, then the normal uninstall runs.
pub const ARG_UNINSTALL: &str = "--uninstall";

/// A start with the uninstall argument (an exact flag: it carries no path, so
/// nothing in the command line can name what gets deleted).
pub fn is_uninstall_launch(args: &[String]) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case(ARG_UNINSTALL))
}

/// Args of the elevated copy a non-admin all-users uninstall asks UAC for:
/// the flag plus the private test-environment args, nothing else.
pub fn uninstall_args(env: &[String], requester: Option<&str>) -> Vec<String> {
    let mut a = vec![ARG_UNINSTALL.to_string()];
    a.extend(env.iter().cloned());
    if let Some(r) = requester {
        a.extend([ARG_REQUESTER.to_string(), r.to_string()]);
    }
    a
}

/// Who asked (hex of the requesting account's SID, not a path): the elevated copy compares it
/// with its own account before it touches anything that belongs to one account.
pub const ARG_REQUESTER: &str = "--requester";

pub fn requester_of(args: &[String]) -> Option<String> {
    let i = args.iter().position(|a| a.eq_ignore_ascii_case(ARG_REQUESTER))?;
    let v = args.get(i + 1)?;
    (!v.is_empty() && v.len() <= 140 && v.chars().all(|c| c.is_ascii_hexdigit())).then(|| v.to_ascii_lowercase())
}

/// No requester (started from Installed apps) counts as the same account; otherwise the SIDs must match.
pub fn same_account(requester: Option<&str>, own: Option<&str>) -> bool {
    match requester {
        None => true,
        Some(r) => own.is_some_and(|o| o.eq_ignore_ascii_case(r)),
    }
}

/// Under HKCU (per-user install) or HKLM (all users).
pub const ARP_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Hayate";
/// The key name before the Hayate rename. winget keys on the key NAME, so an
/// entry called `Blink` was matched to the unrelated package prayag17.Blink.Alpha
/// whatever DisplayName/Publisher said. Migrated to `ARP_KEY` (only when it is ours), removed on uninstall.
pub const ARP_KEY_LEGACY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Blink";

/// Which of the two key names an `ArpStore` call is about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ArpKey {
    Current,
    Legacy,
}
impl ArpKey {
    pub fn path(self) -> &'static str {
        match self {
            ArpKey::Current => ARP_KEY,
            ArpKey::Legacy => ARP_KEY_LEGACY,
        }
    }
}
/// Not the app name: winget matched an ARP entry published as "Hayate" to an unrelated package.
pub const ARP_PUBLISHER: &str = "Hayate Project";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hive {
    User,
    Machine,
}

/// Which hive an install at `w` keeps its Installed-apps entry in.
pub fn arp_hive(w: &Where) -> Option<Hive> {
    match w {
        Where::User => Some(Hive::User),
        Where::Machine => Some(Hive::Machine),
        Where::Portable => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArpValue {
    Str(&'static str, String),
    Dword(&'static str, u32),
}

/// `"<exe>" --uninstall`
pub fn arp_uninstall_string(exe: &str) -> String {
    format!("\"{exe}\" {ARG_UNINSTALL}")
}

/// The values of the entry (install.ps1 writes the same). `dir` is the install
/// dir, `exe` its hayate.exe, `size_kb` the exe size (EstimatedSize).
pub fn arp_values(version: &str, exe: &str, dir: &str, size_kb: u32) -> Vec<ArpValue> {
    vec![
        ArpValue::Str("DisplayName", crate::osutil::APP_NAME.into()),
        ArpValue::Str("DisplayVersion", version.into()),
        ArpValue::Str("Publisher", ARP_PUBLISHER.into()),
        ArpValue::Str("DisplayIcon", exe.into()),
        ArpValue::Str("InstallLocation", dir.into()),
        ArpValue::Str("UninstallString", arp_uninstall_string(exe)),
        ArpValue::Dword("NoModify", 1),
        ArpValue::Dword("NoRepair", 1),
        ArpValue::Dword("EstimatedSize", size_kb),
    ]
}

/// Where the Installed-apps entry lives: the real registry in `setup/mod.rs`,
/// a map in the tests.
pub trait ArpStore {
    fn read_str(&self, h: Hive, k: ArpKey, name: &str) -> Option<String>;
    fn read_dword(&self, h: Hive, k: ArpKey, name: &str) -> Option<u32>;
    fn write(&mut self, h: Hive, k: ArpKey, vals: &[ArpValue]) -> Result<(), String>;
    /// Removes the whole key; a missing key is Ok.
    fn delete(&mut self, h: Hive, k: ArpKey) -> Result<(), String>;
}

fn same_path(a: &str, b: &str) -> bool {
    let n = |p: &str| p.trim().trim_end_matches(['\\', '/']).replace('/', "\\").to_ascii_lowercase();
    !a.trim().is_empty() && n(a) == n(b)
}

/// The legacy `Blink` key is ours only when it was written for THIS install:
/// DisplayName Blink/Hayate, InstallLocation = `dir`, and an UninstallString
/// that starts `"<dir>\blink.exe"`. Anything else (another program's entry
/// that happens to be called Blink) is never touched.
pub fn legacy_arp_is_ours(store: &impl ArpStore, h: Hive, dir: &str) -> bool {
    let k = ArpKey::Legacy;
    let dir = dir.trim_end_matches(['\\', '/']);
    let name_ok = store.read_str(h, k, "DisplayName").is_some_and(|n| n.eq_ignore_ascii_case("Blink") || n.eq_ignore_ascii_case("Hayate"));
    let loc_ok = store.read_str(h, k, "InstallLocation").is_some_and(|l| same_path(&l, dir));
    let want = format!("\"{dir}\\blink.exe\"").replace('/', "\\").to_ascii_lowercase();
    let un_ok = store.read_str(h, k, "UninstallString").is_some_and(|u| u.trim().replace('/', "\\").to_ascii_lowercase().starts_with(&want));
    name_ok && loc_ok && un_ok
}

/// Delete the legacy key when it is ours. Ok(true): gone afterwards (or never there); Ok(false): foreign, kept.
pub fn arp_drop_legacy(store: &mut impl ArpStore, h: Hive, dir: &str) -> Result<bool, String> {
    if store.read_str(h, ArpKey::Legacy, "InstallLocation").is_none() && store.read_str(h, ArpKey::Legacy, "DisplayName").is_none() {
        return Ok(true);
    }
    if !legacy_arp_is_ours(store, h, dir) {
        return Ok(false);
    }
    store.delete(h, ArpKey::Legacy)?;
    Ok(true)
}

/// Uninstall: both key names go (the legacy one only when it is ours).
pub fn arp_remove_all(store: &mut impl ArpStore, h: Hive, dir: &str) -> Result<(), String> {
    let a = store.delete(h, ArpKey::Current);
    let b = arp_drop_legacy(store, h, dir).map(|_| ());
    a.and(b)
}

#[derive(Debug, PartialEq)]
pub enum ArpOutcome {
    /// not this process's job (and why)
    Skipped(&'static str),
    Current,
    Written(Hive),
}

/// The installed exe's start: write the entry when it is missing or stale
/// (older version, moved install). Only where this process may write it:
/// HKCU from a normal process, HKLM from an already elevated one. Never asks
/// for UAC. A per-user install started elevated is skipped: that token's HKCU
/// may belong to another administrator.
pub fn arp_refresh(store: &mut impl ArpStore, at: &Where, elevated: bool, vals: &[ArpValue]) -> Result<ArpOutcome, String> {
    let Some(h) = arp_hive(at) else { return Ok(ArpOutcome::Skipped("not an installed copy")) };
    match (h, elevated) {
        (Hive::Machine, false) => return Ok(ArpOutcome::Skipped("the all-users entry needs an elevated start")),
        (Hive::User, true) => return Ok(ArpOutcome::Skipped("elevated start of a per-user install")),
        _ => {}
    }
    let same = vals.iter().all(|v| match v {
        ArpValue::Str(n, want) => store.read_str(h, ArpKey::Current, n).as_deref() == Some(want.as_str()),
        ArpValue::Dword(n, want) => store.read_dword(h, ArpKey::Current, n) == Some(*want),
    });
    // the pre-rename `Hayate` key: new key first, then the old one goes, so a crash in
    // between leaves both, and both present breaks nothing
    let dir = vals.iter().find_map(|v| match v {
        ArpValue::Str("InstallLocation", d) => Some(d.as_str()),
        _ => None,
    });
    let migrate = dir.is_some_and(|d| legacy_arp_is_ours(store, h, d));
    if !same {
        store.write(h, ArpKey::Current, vals)?;
    }
    if migrate {
        if let Some(d) = dir {
            arp_drop_legacy(store, h, d)?;
        }
    }
    Ok(if same { ArpOutcome::Current } else { ArpOutcome::Written(h) })
}

/// The one exe of an install: `<dir>\hayate.exe`.
pub fn is_hayate_exe(exe: &str) -> bool {
    exe.rsplit(['\\', '/']).next().is_some_and(|n| n.eq_ignore_ascii_case("hayate.exe"))
}

/// An exe of an install of either generation: `hayate.exe`, or the pre-rename
/// `blink.exe` (recognised by `--uninstall` until the migration has moved it).
pub fn is_installed_exe(exe: &str) -> bool {
    exe.rsplit(['\\', '/']).next().is_some_and(|n| n.eq_ignore_ascii_case("hayate.exe") || n.eq_ignore_ascii_case("blink.exe"))
}

#[derive(Debug, PartialEq)]
pub enum UninstallPlan {
    /// ask the two questions and remove it in this process
    Run,
    /// all-users install, not elevated: ask UAC for this exe with `--uninstall`
    Elevate,
}

/// `--uninstall` acts on the running exe's own install and nothing else: the
/// exe must be `hayate.exe` (or the pre-rename `blink.exe`) directly inside the
/// per-user or the all-users install dir, current or pre-rename (both from
/// known folders, given by the caller). A portable copy, a copy in a sub folder
/// or hayate-tm.exe is refused.
pub fn uninstall_plan(exe: &str, user_dir: &str, machine_dir: &str, legacy_user: &str, legacy_machine: &str, elevated: bool) -> Result<UninstallPlan, String> {
    if !is_installed_exe(exe) {
        return Err("This is not an installed copy of Hayate.".into());
    }
    let dir = exe.rsplit_once(['\\', '/']).map(|(d, _)| d).unwrap_or("");
    match classify_any(dir, user_dir, machine_dir, legacy_user, legacy_machine) {
        Where::User => Ok(UninstallPlan::Run),
        Where::Machine if elevated => Ok(UninstallPlan::Run),
        Where::Machine => Ok(UninstallPlan::Elevate),
        Where::Portable => Err("This copy of Hayate is not installed (it runs from an ordinary folder), so there is nothing to uninstall. Delete the file to remove it.".into()),
    }
}

pub const UNINSTALL_TITLE: &str = "Uninstall Hayate";

/// First box (Yes / No).
pub fn confirm_text(w: &Where) -> String {
    match w {
        Where::Machine => "Remove Hayate from this PC?\n\nIt is installed for everyone, so it goes for every user. Its Start menu shortcuts are removed too.".into(),
        _ => "Remove Hayate from this PC?\n\nIts Start menu shortcuts are removed too.".into(),
    }
}

/// Second box (Yes / No), asked only when the data folder may be deleted
/// (`data_dir_removable`); otherwise settings and history are kept. For an
/// all-users install it names whose folder this is: only the account that runs
/// the uninstall is touched, other users keep theirs. `legacy` names the
/// pre-rename settings folder when it is still there and removable too.
pub fn data_question(w: &Where, data_dir: &str, legacy: Option<&str>) -> String {
    let who = if *w == Where::Machine { " That is the folder of the account you are using now; the settings of other users stay." } else { "" };
    let old = legacy.map(|l| format!(" The settings of the version before the rename are in {l}; they go too.")).unwrap_or_default();
    format!("Also delete your Hayate settings and history?\n\nThey are in {data_dir}.{who}{old} Choose No to keep them in case you install Hayate again.")
}

/// What happened to the settings folder at the end of an uninstall.
#[derive(Clone, Debug, PartialEq)]
pub enum DataOutcome {
    Kept,
    Deleted,
    /// asked for, but the folder is still there
    Failed(String),
}

/// `left`: the install folder that is still there after the removal (files in use).
pub fn done_text(d: &DataOutcome, left: Option<&str>) -> String {
    let mut t = match d {
        DataOutcome::Deleted => "Hayate was removed, with its settings and history.".to_string(),
        DataOutcome::Kept => "Hayate was removed. Its settings and history were kept.".to_string(),
        DataOutcome::Failed(dir) => format!("Hayate was removed, but its settings and history could not be deleted. They are still in {dir}; you can delete that folder yourself."),
    };
    if let Some(dir) = left {
        t = t.replacen("Hayate was removed", "Hayate was mostly removed", 1);
        t.push_str(&format!("\n\nSome files are still in {dir} (something was using them). Restart Windows, then delete that folder."));
    }
    t
}

/// An all-users copy is already installed: ask before replacing it with one that is not newer.
pub fn overwrite_needs_confirm(existing_ver: Option<&str>, source_ver: Option<&str>, same_bytes: bool, existing_not_older: bool) -> bool {
    if same_bytes {
        return true;
    }
    match (existing_ver, source_ver) {
        (Some(e), Some(s)) => e.trim() >= s.trim(),
        _ => existing_not_older,
    }
}

pub const OVERWRITE_QUESTION: &str = "Hayate is already installed for everyone on this PC, and it is the same as or newer than this copy.\n\nReplace it with this copy anyway?";

/// The toast activator key is another user's (or another install's) unless its
/// `LocalServer32` command starts the exe inside the dir being removed.
pub fn clsid_ours(local_server: Option<&str>, dir: &str) -> bool {
    let Some(v) = local_server else { return false };
    let v = v.trim();
    let exe = match v.strip_prefix('"') {
        Some(r) => r.split('"').next().unwrap_or(""),
        None => v.split(' ').next().unwrap_or(""),
    };
    crate::update::logic::inside_or_same(exe, dir)
}

// ---- all-users install from the app (`--install-machine`) -----------------------------

/// First argument of the elevated copy an "Install for everyone" click starts.
/// Fixed flags only: the source is the running exe itself, never a path from the command line.
pub const ARG_INSTALL_MACHINE: &str = "--install-machine";
const MF_ADMIN: &str = "--admin-default";
const MF_AUTO: &str = "--autostart";
const MF_UPDATE: &str = "--update";
const MF_REPLACE: &str = "--replace-user";

pub fn install_machine_args(c: &Choice) -> Vec<String> {
    let mut a = vec![ARG_INSTALL_MACHINE.to_string()];
    for (on, f) in [(c.admin_default, MF_ADMIN), (c.autostart, MF_AUTO), (c.update, MF_UPDATE), (c.replace_user, MF_REPLACE)] {
        if on {
            a.push(f.to_string());
        }
    }
    a
}

/// `Some(choice)` when started with `--install-machine`; every other argument is ignored.
pub fn parse_install_machine(args: &[String]) -> Option<Choice> {
    let has = |f: &str| args.iter().any(|a| a.eq_ignore_ascii_case(f));
    has(ARG_INSTALL_MACHINE).then(|| Choice { admin_default: has(MF_ADMIN), autostart: has(MF_AUTO), update: has(MF_UPDATE), replace_user: has(MF_REPLACE) })
}

#[derive(Debug, PartialEq)]
pub enum MachinePlan {
    Run,
    /// not elevated: ask UAC for this exe with the fixed flags
    Elevate,
}

/// Only a portable copy installs itself for everyone. A copy running from the
/// all-users folder is refused (it is installed), and so is one running from the
/// per-user folder.
pub fn machine_install_plan(at: &Where, elevated: bool) -> Result<MachinePlan, String> {
    match at {
        Where::Machine => Err("Hayate is already installed for everyone on this PC.".into()),
        Where::User => Err("This copy is the one installed just for you. Run a downloaded hayate.exe to install it for everyone.".into()),
        Where::Portable if elevated => Ok(MachinePlan::Run),
        Where::Portable => Ok(MachinePlan::Elevate),
    }
}

/// Exit codes of the elevated installer (the window reads them).
pub const MI_OK: i32 = 0;
pub const MI_FAILED: i32 = 1;
pub const MI_REFUSED: i32 = 2;
/// installed, but the installed copy could not be started
pub const MI_LAUNCH_FAILED: i32 = 3;
/// the user said No to replacing a newer all-users copy
pub const MI_DECLINED: i32 = 4;

pub fn machine_exit_text(code: i32) -> &'static str {
    match code {
        MI_REFUSED => "Hayate could not be installed for everyone: this copy is not allowed to be (see hayate.log).",
        MI_LAUNCH_FAILED => "Hayate was installed for everyone, but it could not be started. Open it from the Start menu.",
        _ => "Installing for everyone did not finish. Nothing was left half installed. The reason is in hayate.log.",
    }
}

pub const INSTALL_ALL_LABEL: &str = "Install for everyone on this PC";
pub const INSTALL_ALL_HINT: &str = "Needs administrator permission: Windows asks once. Every user gets Hayate.";
pub const INSTALL_USER_LABEL: &str = "Install just for me";
pub const REPLACE_USER_TITLE: &str = "Replace my own copy";
pub const REPLACE_USER_SUB: &str = "Hayate is already installed just for you. Installing for everyone removes that copy.";

/// Folder below the user's LocalAppData the parked exe goes under (the
/// per-user Temp folder, from the known folder, never `%TEMP%`).
pub fn park_base(local_app_data: &str) -> String {
    format!("{}\\Temp", local_app_data.trim_end_matches(['\\', '/']))
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = r"C:\Users\a\AppData\Local\Programs\Hayate";
    const M: &str = r"C:\Program Files\Hayate";

    #[test]
    fn legacy_data_dir_removable_only_for_the_legacy_leaf() {
        let i = r"C:\Users\a\AppData\Local\Programs\Hayate";
        assert!(legacy_data_dir_removable(r"C:\Users\a\AppData\Local\Blink", i));
        assert!(!legacy_data_dir_removable(r"C:\Users\a\AppData\Local\Hayate", i), "only the pre-rename leaf");
        assert!(!legacy_data_dir_removable(r"C:\Users\a\AppData\Local\Blink\sub", i), "not a sub folder");
        assert!(!legacy_data_dir_removable(r"C:\", i), "never a drive root");
        assert!(!legacy_data_dir_removable(r"C:\Users\a\AppData\Local\Blink", r"C:\Users\a\AppData\Local\Blink\Programs"), "never a parent of the install");
    }

    #[test]
    fn where_it_runs() {
        assert_eq!(classify(U, U, M), Where::User);
        assert_eq!(classify(&format!("{}\\", U.to_uppercase()), U, M), Where::User);
        assert_eq!(classify(r"c:/program files/hayate", U, M), Where::Machine);
        assert_eq!(classify(r"C:\Users\a\Downloads", U, M), Where::Portable);
        assert_eq!(classify(r"C:\src\hayate\target\release", U, ""), Where::Portable);
        // a folder inside the install dir is not the install dir
        assert_eq!(classify(&format!(r"{U}\sub"), U, M), Where::Portable);
    }

    #[test]
    fn prefs_round_trip_and_bar() {
        let mut p = Prefs::default();
        let exe = r"C:\Users\a\Downloads\hayate.exe";
        assert!(show_bar(&Where::Portable, &p, exe));
        assert!(!show_bar(&Where::User, &p, exe));
        assert!(!show_bar(&Where::Machine, &p, exe));
        p.dismiss(exe);
        p.dismiss(&exe.to_uppercase());
        assert_eq!(p.dismissed.len(), 1);
        let back = parse_prefs(&write_prefs(&p));
        assert_eq!(back, p);
        assert!(!show_bar(&Where::Portable, &back, exe));
        assert!(show_bar(&Where::Portable, &back, r"D:\other\hayate.exe"));
        // the taskbar Jump List is on unless switched off, and the answer survives a rewrite
        assert!(!parse_prefs("").jumplist_off);
        let off = parse_prefs("jumplist = false\n");
        assert!(off.jumplist_off);
        assert!(parse_prefs(&write_prefs(&off)).jumplist_off);
        assert!(!parse_prefs(&write_prefs(&Prefs::default())).jumplist_off);
        let never = parse_prefs("never = true\n");
        assert!(!show_bar(&Where::Portable, &never, r"D:\other\hayate.exe"));
        // bounded
        let mut q = Prefs::default();
        for i in 0..40 {
            q.dismiss(&format!(r"C:\d{i}\hayate.exe"));
        }
        let q2 = parse_prefs(&write_prefs(&q));
        assert_eq!(q2.dismissed.len(), 16);
        assert_eq!(q2.dismissed.last().unwrap(), r"c:\d39\hayate.exe");
    }

    #[test]
    fn update_source_only_from_a_publish_folder() {
        let d = r"C:\src\hayate\dist";
        assert_eq!(update_default(d, true, true, U).as_deref(), Some(d));
        assert_eq!(update_default(&format!("{d}\\"), true, true, U).as_deref(), Some(d));
        assert_eq!(update_default(d, false, true, U), None);
        assert_eq!(update_default(d, true, false, U), None);
        assert_eq!(update_default(U, true, true, U), None);
    }

    #[test]
    fn links_for_each_choice() {
        let (l, rm) = menu_links(false, "Hayate.TaskManager");
        assert_eq!(l.len(), 2);
        assert!(l[0].name == LINK_MAIN && !l[0].runas && l[0].toast && l[0].app_id.as_deref() == Some("Hayate.TaskManager"));
        assert!(l[1].name == LINK_ADMIN && l[1].runas && !l[1].toast && l[1].app_id.as_deref() == Some("Hayate.TaskManager.Admin"));
        assert_eq!(rm, vec![LINK_PLAIN, "Blink.lnk", "Blink (admin).lnk", "Blink (no admin).lnk"]);
        let (l, rm) = menu_links(true, "Hayate.TaskManager");
        assert!(l[0].name == LINK_MAIN && l[0].runas && l[0].toast);
        assert!(l[1].name == LINK_PLAIN && !l[1].runas && l[1].app_id.as_deref() == Some("Hayate.TaskManager.User"));
        assert_eq!(rm, vec![LINK_ADMIN, "Blink.lnk", "Blink (admin).lnk", "Blink (no admin).lnk"]);
        let a = autostart_link();
        assert!(a.args == "--resident" && !a.runas && a.app_id.is_none());
    }

    #[test]
    fn toast_server_command() {
        assert_eq!(local_server(r"C:\a b\hayate.exe", &[]), r#""C:\a b\hayate.exe" -ToastActivated"#);
        let x = local_server(r"C:\b\hayate.exe", &["--update-data-dir".into(), r"C:\t d".into(), "--update-multi".into()]);
        assert_eq!(x, r#""C:\b\hayate.exe" -ToastActivated --update-data-dir "C:\t d" --update-multi"#);
        assert!(is_toast_launch(&["-ToastActivated".into(), "-Embedding".into()]));
        assert!(is_toast_launch(&["-toastactivated".into()]));
        assert!(!is_toast_launch(&["--resident".into()]));
        assert_eq!(clsid_key(), r"Software\Classes\CLSID\{8F49C037-2A7A-47A4-A7A0-715C9CF629B6}");
        assert_eq!(format!("{{{:?}}}", windows::core::GUID::from_u128(CLSID)).to_uppercase(), CLSID_STR);
    }

    #[test]
    fn uninstall_paths_are_guarded() {
        assert!(safe_to_remove(U));
        assert!(!safe_to_remove(r"C:\"));
        assert!(!safe_to_remove(r"C:\Users"));
        assert!(!safe_to_remove(r"C:\Users\"));
        assert!(!safe_to_remove(r"\\server\share\Hayate"));
        assert!(!safe_to_remove("Hayate"));
        assert!(data_dir_removable(r"C:\Users\a\AppData\Local\Hayate", U));
        assert!(data_dir_removable(r"C:\Users\a\AppData\Local\Hayate-multi\", U));
        assert!(!data_dir_removable(r"C:\Users\a\AppData\Local", U));
        assert!(!data_dir_removable(r"C:\Users\a\Documents", U));
        assert!(!data_dir_removable(r"C:\Hayate", U));
        assert!(!data_dir_removable(&format!(r"{U}\hayate-data"), U));
        assert_eq!(parked_exe(r"C:\Users\a\AppData\Local\Temp\", 42), r"C:\Users\a\AppData\Local\Temp\Hayate-uninstalled\hayate-42.exe");
        assert!(is_parked_name("hayate-42.exe"));
        assert!(is_parked_name("HAYATE-7.EXE"));
        assert!(!is_parked_name("hayate-.exe"));
        assert!(!is_parked_name("hayate-4x.exe"));
        assert!(!is_parked_name("other.exe"));
        assert!(!is_parked_name("hayate-42.exe.txt"));
    }

    #[test]
    fn offer_hidden_when_installed() {
        let p = Prefs::default();
        assert!(!offer(&Where::Portable, true, &p, r"D:\x\hayate.exe"));
        assert!(offer(&Where::Portable, false, &p, r"D:\x\hayate.exe"));
    }

    fn sec_pos(ps: &str, needle: &str) -> usize {
        ps.find(needle).unwrap_or_else(|| panic!("install.ps1 lacks {needle}"))
    }

    #[derive(Default)]
    struct MemArp {
        user: Vec<ArpValue>,
        machine: Vec<ArpValue>,
        user_legacy: Vec<ArpValue>,
        machine_legacy: Vec<ArpValue>,
        writes: u32,
        fail: bool,
    }
    impl MemArp {
        fn lslot(&mut self, h: Hive, k: ArpKey) -> &mut Vec<ArpValue> {
            match (h, k) {
                (Hive::User, ArpKey::Current) => &mut self.user,
                (Hive::Machine, ArpKey::Current) => &mut self.machine,
                (Hive::User, ArpKey::Legacy) => &mut self.user_legacy,
                (Hive::Machine, ArpKey::Legacy) => &mut self.machine_legacy,
            }
        }
        fn get(&self, h: Hive, k: ArpKey) -> &Vec<ArpValue> {
            match (h, k) {
                (Hive::User, ArpKey::Current) => &self.user,
                (Hive::Machine, ArpKey::Current) => &self.machine,
                (Hive::User, ArpKey::Legacy) => &self.user_legacy,
                (Hive::Machine, ArpKey::Legacy) => &self.machine_legacy,
            }
        }
    }
    impl ArpStore for MemArp {
        fn read_str(&self, h: Hive, k: ArpKey, name: &str) -> Option<String> {
            let v = self.get(h, k);
            v.iter().find_map(|x| match x {
                ArpValue::Str(n, s) if *n == name => Some(s.clone()),
                _ => None,
            })
        }
        fn read_dword(&self, h: Hive, k: ArpKey, name: &str) -> Option<u32> {
            let v = self.get(h, k);
            v.iter().find_map(|x| match x {
                ArpValue::Dword(n, d) if *n == name => Some(*d),
                _ => None,
            })
        }
        fn write(&mut self, h: Hive, k: ArpKey, vals: &[ArpValue]) -> Result<(), String> {
            if self.fail {
                return Err("denied".into());
            }
            self.writes += 1;
            *self.lslot(h, k) = vals.to_vec();
            Ok(())
        }
        fn delete(&mut self, h: Hive, k: ArpKey) -> Result<(), String> {
            self.lslot(h, k).clear();
            Ok(())
        }
    }

    fn vals() -> Vec<ArpValue> {
        arp_values("1.2.3", r"C:\Program Files\Hayate\hayate.exe", r"C:\Program Files\Hayate", 2300)
    }

    #[test]
    fn installed_apps_entry_values() {
        let v = vals();
        let s = |n: &str| v.iter().find_map(|x| match x { ArpValue::Str(k, s) if *k == n => Some(s.as_str()), _ => None });
        let d = |n: &str| v.iter().find_map(|x| match x { ArpValue::Dword(k, d) if *k == n => Some(*d), _ => None });
        assert_eq!(s("DisplayName"), Some("Hayate"));
        assert_eq!(s("DisplayVersion"), Some("1.2.3"));
        assert_eq!(s("Publisher"), Some(ARP_PUBLISHER));
        assert_eq!(s("DisplayIcon"), Some(r"C:\Program Files\Hayate\hayate.exe"));
        assert_eq!(s("InstallLocation"), Some(r"C:\Program Files\Hayate"));
        assert_eq!(s("UninstallString"), Some(r#""C:\Program Files\Hayate\hayate.exe" --uninstall"#));
        assert_eq!((d("NoModify"), d("NoRepair"), d("EstimatedSize")), (Some(1), Some(1), Some(2300)));
        assert_eq!(ARP_KEY, r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Hayate");
        assert_eq!(ARP_KEY_LEGACY, r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Blink");
        assert_eq!(arp_hive(&Where::User), Some(Hive::User));
        assert_eq!(arp_hive(&Where::Machine), Some(Hive::Machine));
        assert_eq!(arp_hive(&Where::Portable), None);
    }

    #[test]
    fn existing_installs_get_the_entry_only_where_this_process_may_write_it() {
        let v = vals();
        // portable: never
        let mut m = MemArp::default();
        assert!(matches!(arp_refresh(&mut m, &Where::Portable, false, &v), Ok(ArpOutcome::Skipped(_))));
        // per-user, normal start: written once, then current
        assert_eq!(arp_refresh(&mut m, &Where::User, false, &v), Ok(ArpOutcome::Written(Hive::User)));
        assert_eq!(arp_refresh(&mut m, &Where::User, false, &v), Ok(ArpOutcome::Current));
        assert_eq!(m.writes, 1);
        assert!(m.machine.is_empty());
        // a new version (auto-update) refreshes it
        let newer = arp_values("1.2.4", r"C:\Program Files\Hayate\hayate.exe", r"C:\Program Files\Hayate", 2400);
        assert_eq!(arp_refresh(&mut m, &Where::User, false, &newer), Ok(ArpOutcome::Written(Hive::User)));
        // per-user install started elevated: HKCU may be another account's, skip
        let mut e = MemArp::default();
        assert!(matches!(arp_refresh(&mut e, &Where::User, true, &v), Ok(ArpOutcome::Skipped(_))));
        assert_eq!(e.writes, 0);
        // all-users: not elevated = no write (and no prompt: there is no elevation path in this fn)
        assert!(matches!(arp_refresh(&mut e, &Where::Machine, false, &v), Ok(ArpOutcome::Skipped(_))));
        assert_eq!(e.writes, 0);
        // all-users, already elevated: HKLM only
        assert_eq!(arp_refresh(&mut e, &Where::Machine, true, &v), Ok(ArpOutcome::Written(Hive::Machine)));
        assert!(e.user.is_empty() && !e.machine.is_empty());
        // a changed size alone makes the entry stale
        let mut g = MemArp::default();
        arp_refresh(&mut g, &Where::User, false, &v).unwrap();
        let bigger = arp_values("1.2.3", r"C:\Program Files\Hayate\hayate.exe", r"C:\Program Files\Hayate", 9999);
        assert_eq!(arp_refresh(&mut g, &Where::User, false, &bigger), Ok(ArpOutcome::Written(Hive::User)));
        // a write failure is an error, not a panic or a retry loop
        let mut f = MemArp { fail: true, ..Default::default() };
        assert_eq!(arp_refresh(&mut f, &Where::User, false, &v), Err("denied".into()));
        // removal clears only the named hive
        e.user = v.clone();
        e.delete(Hive::Machine, ArpKey::Current).unwrap();
        assert!(e.machine.is_empty() && !e.user.is_empty());
    }

    fn legacy_vals(dir: &str) -> Vec<ArpValue> {
        let exe = format!("{dir}\\blink.exe");
        vec![
            ArpValue::Str("DisplayName", "Blink".into()),
            ArpValue::Str("Publisher", "Blink".into()),
            ArpValue::Str("InstallLocation", dir.into()),
            ArpValue::Str("UninstallString", arp_uninstall_string(&exe)),
        ]
    }
    const PF: &str = r"C:\Program Files\Hayate";

    #[test]
    fn legacy_blink_key_is_migrated_to_hayate() {
        let v = vals();
        for (at, h, elevated) in [(Where::User, Hive::User, false), (Where::Machine, Hive::Machine, true)] {
            let mut m = MemArp::default();
            *m.lslot(h, ArpKey::Legacy) = legacy_vals(PF);
            assert_eq!(arp_refresh(&mut m, &at, elevated, &v), Ok(ArpOutcome::Written(h)));
            assert!(!m.get(h, ArpKey::Current).is_empty() && m.get(h, ArpKey::Legacy).is_empty(), "{h:?}");
            let o = if h == Hive::User { Hive::Machine } else { Hive::User };
            assert!(m.get(o, ArpKey::Current).is_empty() && m.get(o, ArpKey::Legacy).is_empty(), "other hive untouched");
            assert_eq!(arp_refresh(&mut m, &at, elevated, &v), Ok(ArpOutcome::Current));
        }
    }

    #[test]
    fn both_keys_present_is_cleaned_without_a_rewrite() {
        let v = vals();
        let mut m = MemArp::default();
        arp_refresh(&mut m, &Where::User, false, &v).unwrap();
        *m.lslot(Hive::User, ArpKey::Legacy) = legacy_vals(PF);
        let w = m.writes;
        assert_eq!(arp_refresh(&mut m, &Where::User, false, &v), Ok(ArpOutcome::Current));
        assert_eq!(m.writes, w, "current key already right: not rewritten");
        assert!(m.user_legacy.is_empty());
        // both present and the new one stale (e.g. a crash, then an update): refreshed, legacy gone
        let newer = arp_values("9.9.9", r"C:\Program Files\Hayate\hayate.exe", PF, 1);
        *m.lslot(Hive::User, ArpKey::Legacy) = legacy_vals(PF);
        assert_eq!(arp_refresh(&mut m, &Where::User, false, &newer), Ok(ArpOutcome::Written(Hive::User)));
        assert!(m.user_legacy.is_empty());
    }

    #[test]
    fn foreign_blink_key_is_never_touched() {
        let v = vals();
        let un = |s: &str| ArpValue::Str("UninstallString", s.into());
        let loc = |s: &str| ArpValue::Str("InstallLocation", s.into());
        let name = |s: &str| ArpValue::Str("DisplayName", s.into());
        for foreign in [
            legacy_vals(r"C:\Users\a\AppData\Local\Blink-Alpha"),
            vec![name("Blink Alpha"), loc(PF), un(r#""C:\Program Files\Hayate\blink.exe" --uninstall"#)],
            vec![name("Blink"), loc(PF), un(r#""C:\Other\unins000.exe""#)],
            vec![name("Blink")],
        ] {
            let mut m = MemArp::default();
            *m.lslot(Hive::Machine, ArpKey::Legacy) = foreign.clone();
            assert_eq!(arp_refresh(&mut m, &Where::Machine, true, &v), Ok(ArpOutcome::Written(Hive::Machine)));
            assert_eq!(m.machine_legacy, foreign, "foreign entry kept");
            assert!(!m.machine.is_empty());
            arp_remove_all(&mut m, Hive::Machine, PF).unwrap();
            assert!(m.machine.is_empty());
            assert_eq!(m.machine_legacy, foreign, "uninstall leaves it too");
        }
    }

    #[test]
    fn uninstall_removes_both_key_names_of_its_own_hive_only() {
        let v = vals();
        let mut m = MemArp::default();
        m.user = v.clone();
        m.machine = v.clone();
        *m.lslot(Hive::User, ArpKey::Legacy) = legacy_vals(PF);
        *m.lslot(Hive::Machine, ArpKey::Legacy) = legacy_vals(PF);
        arp_remove_all(&mut m, Hive::Machine, r"C:\Program Files\Hayate\").unwrap();
        assert!(m.machine.is_empty() && m.machine_legacy.is_empty());
        assert!(!m.user.is_empty() && !m.user_legacy.is_empty(), "per-user entries stay");
        arp_remove_all(&mut m, Hive::User, PF).unwrap();
        assert!(m.user.is_empty() && m.user_legacy.is_empty());
    }

    #[test]
    fn legacy_migration_needs_the_right_hive_and_elevation() {
        let v = vals();
        // all-users legacy entry, not elevated: nothing to do (an elevated start or a reinstall migrates it)
        let mut m = MemArp::default();
        *m.lslot(Hive::Machine, ArpKey::Legacy) = legacy_vals(PF);
        assert!(matches!(arp_refresh(&mut m, &Where::Machine, false, &v), Ok(ArpOutcome::Skipped(_))));
        assert!(!m.machine_legacy.is_empty() && m.machine.is_empty());
        assert!(matches!(arp_refresh(&mut m, &Where::Portable, true, &v), Ok(ArpOutcome::Skipped(_))));
        assert!(!m.machine_legacy.is_empty());
    }

    #[test]
    fn uninstall_acts_only_on_the_installed_exe() {
        let mx = r"C:\Program Files\Hayate\hayate.exe";
        let ux = format!(r"{U}\hayate.exe");
        let (lu, lm) = (r"C:\Users\a\AppData\Local\Programs\Blink", r"C:\Program Files\Blink");
        let lx = format!(r"{lu}\blink.exe");
        let lmx = format!(r"{lm}\blink.exe");
        assert_eq!(uninstall_plan(&ux, U, M, lu, lm, false), Ok(UninstallPlan::Run));
        assert_eq!(uninstall_plan(&ux, U, M, lu, lm, true), Ok(UninstallPlan::Run));
        assert_eq!(uninstall_plan(mx, U, M, lu, lm, false), Ok(UninstallPlan::Elevate));
        assert_eq!(uninstall_plan(mx, U, M, lu, lm, true), Ok(UninstallPlan::Run));
        assert_eq!(uninstall_plan(&mx.to_uppercase(), U, M, lu, lm, true), Ok(UninstallPlan::Run));
        // the pre-rename install of either kind is uninstallable too (docs/rename-full.md)
        assert_eq!(uninstall_plan(&lx, U, M, lu, lm, false), Ok(UninstallPlan::Run));
        assert_eq!(uninstall_plan(&lmx, U, M, lu, lm, false), Ok(UninstallPlan::Elevate));
        assert_eq!(uninstall_plan(&lmx, U, M, lu, lm, true), Ok(UninstallPlan::Run));
        // refused: portable folders, sub folders, other exe names, empty known folders
        assert!(uninstall_plan(r"C:\Users\a\Downloads\hayate.exe", U, M, lu, lm, false).is_err());
        assert!(uninstall_plan(r"C:\Users\a\Downloads\blink.exe", U, M, lu, lm, false).is_err());
        assert!(uninstall_plan(r"C:\src\hayate\target\release\hayate.exe", U, M, lu, lm, true).is_err());
        assert!(uninstall_plan(&format!(r"{U}\sub\hayate.exe"), U, M, lu, lm, false).is_err());
        assert!(uninstall_plan(r"C:\Program Files\Hayate\hayate-tm.exe", U, M, lu, lm, true).is_err());
        assert!(uninstall_plan(r"C:\Program Files\Blink\blink-tm.exe", U, M, lu, lm, true).is_err());
        assert!(uninstall_plan(r"C:\Program Files\Hayate\hayate.old.exe", U, M, lu, lm, true).is_err());
        assert!(uninstall_plan(mx, "", "", "", "", true).is_err());
        assert!(uninstall_plan(r"C:\Program Files\Hayate\..\Evil\hayate.exe", U, M, lu, lm, true).is_err());
        assert!(uninstall_plan("hayate.exe", U, M, lu, lm, true).is_err());
    }

    #[test]
    fn uninstall_args_carry_no_path() {
        assert!(is_uninstall_launch(&["--uninstall".into()]));
        assert!(is_uninstall_launch(&["--UNINSTALL".into(), "--update-multi".into()]));
        assert!(!is_uninstall_launch(&["--uninstall-now".into()]));
        assert!(!is_uninstall_launch(&["--resident".into()]));
        assert_eq!(uninstall_args(&[], None), vec!["--uninstall".to_string()]);
        assert_eq!(uninstall_args(&["--update-multi".into()], None), vec!["--uninstall".to_string(), "--update-multi".to_string()]);
        let a = uninstall_args(&[], Some("0105ab"));
        assert_eq!(requester_of(&a).as_deref(), Some("0105ab"));
        assert_eq!(requester_of(&["--requester".into(), r"C:\evil".into()]), None);
        assert!(same_account(None, Some("01")) && same_account(Some("0105AB"), Some("0105ab")));
        assert!(!same_account(Some("01"), Some("02")) && !same_account(Some("01"), None));
        assert!(overwrite_needs_confirm(None, None, true, false));
        assert!(overwrite_needs_confirm(Some("2026-11-18"), Some("2026-11-18"), false, false));
        assert!(!overwrite_needs_confirm(Some("2026-11-17"), Some("2026-11-18"), false, true));
        assert!(overwrite_needs_confirm(None, None, false, true) && !overwrite_needs_confirm(None, None, false, false));
        assert!(done_text(&DataOutcome::Kept, Some(r"C:\Program Files\Hayate")).contains(r"C:\Program Files\Hayate"));
        assert!(machine_exit_text(MI_LAUNCH_FAILED).contains("installed for everyone"));
        assert!(confirm_text(&Where::Machine).contains("everyone"));
        assert!(!confirm_text(&Where::User).contains("everyone"));
        assert!(data_question(&Where::User, r"C:\Users\a\AppData\Local\Hayate", None).contains(r"C:\Users\a\AppData\Local\Hayate"));
        assert!(data_question(&Where::Machine, r"C:\x\Hayate", None).contains("other users"));
        assert!(!data_question(&Where::User, r"C:\x\Hayate", None).contains("other users"));
        assert_ne!(done_text(&DataOutcome::Deleted, None), done_text(&DataOutcome::Kept, None));
        assert!(done_text(&DataOutcome::Failed(r"C:\x\Hayate".into()), None).contains(r"C:\x\Hayate"));
        assert_eq!(park_base(r"C:\Users\a\AppData\Local\"), r"C:\Users\a\AppData\Local\Temp");
        assert_eq!(parked_exe(&park_base(r"C:\Users\a\AppData\Local"), 7), r"C:\Users\a\AppData\Local\Temp\Hayate-uninstalled\hayate-7.exe");
    }

    #[test]
    fn user_text_has_no_script_hint() {
        // the Installed-apps entry and Settings never send a user to a terminal
        for t in [confirm_text(&Where::Machine), confirm_text(&Where::User), data_question(&Where::Machine, "C:\\x\\Hayate", None), done_text(&DataOutcome::Deleted, None), done_text(&DataOutcome::Kept, Some("d")), done_text(&DataOutcome::Failed("d".into()), None), OVERWRITE_QUESTION.into(), INSTALL_ALL_LABEL.into(), INSTALL_ALL_HINT.into(), REPLACE_USER_SUB.into(), machine_exit_text(MI_FAILED).into()] {
            assert!(!t.contains("install.ps1") && !t.contains("terminal") && !t.contains("HKLM") && !t.contains("UAC"));
        }
    }

    #[test]
    fn all_users_install_flags_and_plan() {
        let c = Choice { admin_default: true, autostart: false, update: true, replace_user: true };
        let a = install_machine_args(&c);
        assert_eq!(a[0], "--install-machine");
        assert_eq!(parse_install_machine(&a), Some(c));
        assert_eq!(parse_install_machine(&install_machine_args(&Choice::default())), Some(Choice::default()));
        assert_eq!(parse_install_machine(&["--uninstall".into()]), None);
        // only fixed flags exist: a path-looking argument changes nothing
        let mut b = install_machine_args(&Choice::default());
        b.push(r"C:\evil\hayate.exe".into());
        assert_eq!(parse_install_machine(&b), Some(Choice::default()));
        assert!(a.iter().all(|x| x.starts_with("--")));
        assert_eq!(machine_install_plan(&Where::Portable, false), Ok(MachinePlan::Elevate));
        assert_eq!(machine_install_plan(&Where::Portable, true), Ok(MachinePlan::Run));
        assert!(machine_install_plan(&Where::Machine, true).is_err());
        assert!(machine_install_plan(&Where::User, true).is_err());
    }

    #[test]
    fn toast_key_is_removed_only_when_it_points_into_the_removed_dir() {
        let d = r"C:\Program Files\Hayate";
        assert!(clsid_ours(Some(r#""C:\Program Files\Hayate\hayate.exe" -ToastActivated"#), d));
        assert!(!clsid_ours(Some(r#""C:\Users\b\AppData\Local\Programs\Hayate\hayate.exe" -ToastActivated"#), d));
        assert!(!clsid_ours(Some(r#""C:\Program Files\Hayate2\hayate.exe" -ToastActivated"#), d));
        assert!(!clsid_ours(None, d));
        // an unquoted command with a space in the path is never treated as ours
        assert!(!clsid_ours(Some(r"c:\program files\hayate\hayate.exe -ToastActivated"), d));
        assert!(clsid_ours(Some(r"C:\HAYATE\hayate.exe -ToastActivated"), r"c:\hayate"));
    }

    /// install.ps1 writes the same footprint as the in-app installer.
    #[test]
    fn menu_migration_renames_or_drops_legacy_links() {
        use std::collections::HashSet;
        let have: HashSet<&str> = ["Blink.lnk", "Blink (admin).lnk", "Hayate (admin).lnk"].into_iter().collect();
        let m = menu_migration(|n| have.contains(n));
        assert_eq!(m, vec![MenuMove::Rename("Blink.lnk", "Hayate.lnk"), MenuMove::DropLegacy("Blink (admin).lnk")]);
        assert!(menu_migration(|_| false).is_empty());
    }

    #[test]
    fn no_user_visible_name_still_says_blink() {
        let names = [crate::osutil::APP_NAME, ARP_PUBLISHER, LINK_MAIN, LINK_ADMIN, LINK_PLAIN];
        for n in names {
            assert!(!n.to_lowercase().contains("blink"), "{n}");
        }
        let (a, _) = menu_links(true, "x");
        let (b, _) = menu_links(false, "x");
        for l in a.iter().chain(b.iter()).chain([autostart_link()].iter()) {
            assert!(!l.description.to_lowercase().contains("blink"), "{}", l.description);
        }
        let v = arp_values("1.0", "C:/x/hayate.exe", "C:/x", 1);
        assert!(v.iter().any(|x| matches!(x, ArpValue::Str("DisplayName", s) if s == "Hayate")));
        assert_eq!(ARP_KEY.rsplit('\\').next(), Some("Hayate"), "winget keys on the key name");
    }

    #[test]
    fn install_ps1_matches() {
        let ps = include_str!("../../install.ps1");
        assert!(ps.contains(&format!("$toastClsid = '{CLSID_STR}'")), "install.ps1 CLSID");
        assert!(ps.contains(&format!("$appId = '{}'", crate::notify::AUMID)), "install.ps1 AUMID");
        for n in all_menu_links() {
            assert!(ps.contains(&format!("'{n}'")), "install.ps1 link {n}");
        }
        assert!(ps.contains(TOAST_ARG));
        assert!(ps.contains("'--resident'"));
        // the Installed-apps entry: same key, same values, same version
        assert!(ps.contains(&format!("$arpPath    = '{ARP_KEY}'")), "install.ps1 ARP key");
        assert!(ps.contains(&format!("$arpLegacy  = '{ARP_KEY_LEGACY}'")), "install.ps1 legacy ARP key");
        assert!(ps.contains(&format!("$arpVersion = '{}'", env!("CARGO_PKG_VERSION"))), "install.ps1 ARP version (Cargo.toml)");
        for v in arp_values("v", "e", "d", 0) {
            let n = match v {
                ArpValue::Str(n, _) | ArpValue::Dword(n, _) => n,
            };
            assert!(ps.contains(&format!("@('{n}', ")), "install.ps1 ARP value {n}");
        }
        assert!(ps.contains(&format!("\"`\"$dest`\" {ARG_UNINSTALL}\"")), "install.ps1 UninstallString");
        assert!(ps.contains("Open-ArpHive ([bool]$Machine)") && ps.contains("RegistryView]::Registry64"), "install.ps1 ARP hive by -Machine, 64-bit view");
        // an all-users install drops the per-user entry and uses the all-users Startup folder, like the in-app one
        assert!(ps.replace("\r\n", "\n").contains("if ($Machine -and -not (Test-Path (Join-Path $userDir 'hayate.exe'))) {") && ps.contains("'CommonStartup'") && ps.contains("$autoDst = if ($Machine) { $autoAll }"), "install.ps1 -Machine parity");
        // and the uninstall removes it, for both generations of the install
        assert!(
            ps.contains("Remove-Arp $false $userDir")
                && ps.contains("if ($isAdmin) {")
                && ps.contains("Remove-Arp $true $machineDir")
                && ps.contains("Remove-ArpLegacy ([bool]$Machine) $dir")
                && ps.contains("Remove-ArpLegacy $false $legacyUserDir")
                && ps.contains("Remove-ArpLegacy $true $legacyMachineDir")
                && ps.contains("$legacyMachineDir")
                && ps.contains("$legacyUserDir")
        );
        // uninstall clears the Jump List the same way the in-app uninstall does
        assert!(ps.contains(crate::taskbar::logic::ARG_REMOVE), "install.ps1 jump list removal");
        // SECURITY: an elevated uninstall must never run the per-user exe (user-writable, swappable). The exe list is chosen by $isAdmin.
        let sec = ps.lines().position(|l| l.contains("$jlExes = if ($isAdmin)")).expect("jump list exe selection");
        let l = ps.lines().nth(sec).unwrap();
        let (admin_branch, user_branch) = l.split_once("} else {").expect("if/else");
        assert!(admin_branch.contains("$machineDir") && !admin_branch.contains("$userDir"), "elevated: per-machine exe only: {l}");
        assert!(user_branch.contains("$userDir") && !user_branch.contains("$machineDir"), "normal: per-user exe only: {l}");
        // and nothing else in the script launches hayate.exe with the removal arg
        assert_eq!(ps.matches("--remove-jumplist").count(), 1, "single launch site of --remove-jumplist");
        assert_eq!(ps.matches("Start-Process -FilePath $exe").count(), 1);
        // the startup manager's switched-off Windows entries are given back BEFORE anything is removed, by that same
        // single launch site (so the elevated run still never executes the per-user exe), and a failure stops the uninstall
        let steps = ps.find(&format!("$steps = @('{}', '{}')", crate::startup::ARG_RELEASE, crate::taskbar::logic::ARG_REMOVE)).expect("release step list");
        let first_removal = ps.find("Remove-Item $auto").expect("first removal");
        assert!(steps < first_removal, "release comes before the first removal");
        assert!(steps > sec_pos(ps, "$jlExes = if ($isAdmin)"), "release uses the exe chosen by $isAdmin");
        assert!(ps.contains("-Wait -PassThru") && ps.contains("$code -ne 0"), "the exit code is checked");
        let fail = ps.find("$code -ne 0").unwrap();
        assert!(ps[fail..first_removal].contains("exit 1"), "a failed release exits before removing anything");
        assert!(ps.contains("if (-not $released)"), "no exe allowed to release = refuse, not silently continue");
        assert!(ps.contains("$machineDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles'))"));
        assert!(!ps.contains("$machineDir = Join-Path $env:"));
        assert!(ps.contains("ToastActivatorCLSID") || ps.contains("pid = 26"));
    }
}
