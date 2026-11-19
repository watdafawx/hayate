//! Self-install from blink.exe (no separate installer) and the toast click
//! handler it registers.
//!
//! * A copy running anywhere but `%LOCALAPPDATA%\Programs\Blink` (or an
//!   install.ps1 -Machine dir) is portable: it works as always and shows a
//!   dismissible bar inside its window offering "Install Blink". Nothing is
//!   written before the user clicks Install in the panel.
//! * Install copies the running exe (+ version.txt), writes the Start-menu
//!   shortcuts (app ID, toast activator CLSID, icon), optionally the Startup
//!   shortcut (`--resident`, never elevated) and update.ini, and the one
//!   registry key: `HKCU\Software\Classes\CLSID\{…}\LocalServer32`. Then this
//!   window closes and `finish` starts the installed copy (through UAC when
//!   "Run as administrator by default" was ticked; on cancel, unelevated).
//! * Uninstall (Settings) removes the same items, all in this process: a
//!   running exe cannot be deleted but can be renamed, so blink.exe first
//!   moves to `%TEMP%\Blink-uninstalled\blink-<pid>.exe` (deleted by the next
//!   Blink start, or Storage Sense); then the install dir goes, and the data
//!   dir (when ticked) once the window has closed. No helper process, no
//!   cmd.exe, no reboot-pending key: see the Defender review notes, Self-install.
//! * Installs also write an Installed-apps entry (Settings > Apps): `HKCU` or
//!   (install.ps1 -Machine) `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Hayate`,
//!   `UninstallString` = `"<exe>" --uninstall`. That start (`uninstall_cli`, also
//!   Settings > Uninstall for an all-users install) asks with two native boxes and
//!   runs the same `uninstall`; an all-users install is first handed to UAC.
//!   An older install gets the entry on its next normal start (`refresh_arp`).
//! * `-ToastActivated` (COM, a click in the notification centre): see
//!   `activator`; the event goes to the running Blink or Blink starts on Watch.
//!
//! install.ps1 writes the same footprint (see the Defender review notes, Self-install).

pub mod logic;
mod activator;

use logic::*;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::osutil::{self, log, pcw, wide};
use crate::update::logic::{command_line, join_args, relaunch_args, Restore};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct Status {
    pub at: Where,
    pub exe: String,
    pub exe_dir: String,
    /// `%LOCALAPPDATA%\Programs\Blink`
    pub install_dir: String,
    /// auto-update source offered by default (a publish folder), if any
    pub update_source: Option<String>,
    /// an installed blink.exe (per-user or per-machine) exists: no offer bar
    pub installed: Option<String>,
}

static STATUS: OnceLock<Status> = OnceLock::new();
static PREFS: Mutex<Option<Prefs>> = Mutex::new(None);
/// `-ToastActivated`: the Watch event to open (0: just Watch)
static START_WATCH: OnceLock<Option<u64>> = OnceLock::new();
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

enum Pending {
    Launch { exe: PathBuf, restore: Restore, admin: bool },
    /// uninstall done: delete this data dir (when asked) after the window, then say so
    /// (data dir to delete, install folder that survived the removal)
    Removed(Option<String>, Option<String>),
}

/// `<LocalAppData>\Programs\Blink` from the known folder (never `%LOCALAPPDATA%`).
pub fn user_dir() -> String {
    known_folder(&FOLDERID_LocalAppData).map(|p| p.join(r"Programs\Blink").to_string_lossy().into_owned()).unwrap_or_default()
}

fn machine_dir() -> String {
    known_folder(&FOLDERID_ProgramFiles).map(|p| p.join("Blink").to_string_lossy().into_owned()).unwrap_or_default()
}

/// This is the per-user installed copy (`%LOCALAPPDATA%\Programs\Blink\blink.exe`): the only copy that
/// writes or changes Blink's Startup shortcut from the Startup tab.
pub fn per_user_installed() -> bool {
    status().at == Where::User
}

/// `%ProgramFiles%` from the shell's known-folder API (not the environment,
/// which a user can set for a process before it is started elevated).
pub fn local_app_data() -> Option<String> {
    known_folder(&FOLDERID_LocalAppData).map(|p| p.to_string_lossy().into_owned())
}

pub fn program_files() -> Option<String> {
    known_folder(&FOLDERID_ProgramFiles).map(|p| p.to_string_lossy().into_owned())
}

pub fn status() -> &'static Status {
    STATUS.get_or_init(|| {
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let exe_dir = osutil::parent_dir(&exe).to_string();
        let install_dir = user_dir();
        let at = classify(&exe_dir, &install_dir, &machine_dir());
        let has = |n: &str| Path::new(&exe_dir).join(n).is_file();
        let update_source = update_default(&exe_dir, has("blink.exe.sha256"), has("version.txt"), &install_dir);
        let installed = [install_dir.clone(), machine_dir()].into_iter().filter(|d| !d.is_empty()).find(|d| Path::new(d).join("blink.exe").is_file());
        Status { at, exe, exe_dir, install_dir, update_source, installed }
    })
}

fn prefs_file() -> PathBuf {
    osutil::data_dir().join("setup.ini")
}

fn prefs<R>(f: impl FnOnce(&mut Prefs) -> R) -> R {
    let mut g = PREFS.lock().unwrap_or_else(|e| e.into_inner());
    let p = g.get_or_insert_with(|| std::fs::read_to_string(prefs_file()).map(|t| parse_prefs(&t)).unwrap_or_default());
    f(p)
}

fn save_prefs() {
    let text = prefs(|p| write_prefs(p));
    crate::bgio::write_file(prefs_file(), text.into_bytes());
}

/// The install bar is up (portable copy, not answered for this exe path).
pub fn bar_visible() -> bool {
    let s = status();
    prefs(|p| offer(&s.at, s.installed.is_some(), p, &s.exe))
}

/// "Not now": hide the bar for this exe path.
pub fn not_now() {
    let exe = status().exe.clone();
    prefs(|p| p.dismiss(&exe));
    save_prefs();
    log(&format!("setup: install offer hidden for {exe} (not now)"));
}

/// "Don't ask again".
pub fn never_ask() {
    prefs(|p| p.never = true);
    save_prefs();
    log("setup: install offer off (don't ask again)");
}

// ---- toast activation ------------------------------------------------------

/// From main, before the window: a `-ToastActivated` start (COM) waits for
/// Windows' Activate call and remembers which Watch event to open.
pub fn early() {
    sweep_parked();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !is_toast_launch(&args) {
        refresh_arp();
        migrate_menu_links();
        return;
    }
    let got = activator::wait_activation(std::time::Duration::from_secs(10));
    let id = got.as_deref().and_then(crate::notify::parse_launch).unwrap_or(0);
    log(&format!("setup: started by a notification-centre click, launch {got:?} -> Watch event {id}"));
    let _ = START_WATCH.set(Some(id));
}

/// Watch event a toast launch asked for (Some(0): just Watch).
pub fn start_watch() -> Option<u64> {
    START_WATCH.get().copied().flatten()
}

/// The running Blink to hand a start to: one running from this exe first (a
/// BLINK_MULTI test copy next to others), else the user's normal one (never
/// for a test instance). Not this process.
pub fn find_running() -> Option<HWND> {
    let me = norm(&status().exe);
    let mut any = None;
    let mut mine = None;
    for (class, normal) in [(w!("BlinkMain"), true), (w!("BlinkMainMulti"), false)] {
        let mut prev: Option<HWND> = None;
        loop {
            let Ok(h) = (unsafe { FindWindowExW(None, prev, class, None) }) else { break };
            if h.is_invalid() {
                break;
            }
            prev = Some(h);
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
            if pid == std::process::id() {
                continue;
            }
            if osutil::image_path(pid).is_some_and(|p| norm(&p) == me) {
                mine.get_or_insert(h);
            } else if normal && !osutil::multi() {
                any.get_or_insert(h);
            }
        }
    }
    mine.or(any)
}

/// Toast launch with Blink already running: post `msg` (wp 1, lp event id) to
/// that window and return true (this process then exits).
pub fn forward(msg: u32) -> bool {
    let Some(id) = start_watch() else { return false };
    let Some(h) = find_running() else { return false };
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        let _ = PostMessageW(Some(h), msg, WPARAM(1), LPARAM(id as isize));
    }
    log(&format!("setup: toast click forwarded to the running Blink (event {id})"));
    true
}

fn norm(p: &str) -> String {
    crate::update::logic::norm_path(p)
}

// ---- install -----------------------------------------------------------------

/// Private args that carry a test environment (BLINK_DATA_DIR / BLINK_MULTI)
/// into starts that do not inherit ours (COM, UAC).
fn env_args() -> Vec<String> {
    relaunch_args(&[], &env_restore())
}

/// BLINK_DATA_DIR / BLINK_MULTI of this process as a `Restore` (UAC and
/// Explorer starts lose the environment; `update::env_from_args` puts it back).
pub fn env_restore() -> Restore {
    Restore { data_dir: std::env::var("BLINK_DATA_DIR").ok().filter(|d| !d.is_empty()), multi: osutil::multi(), ..Default::default() }
}

/// Run `f` on a fresh STA thread (shell links are apartment COM objects).
fn on_sta<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Option<R> {
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let r = f();
        CoUninitialize();
        r
    })
    .join()
    .ok()
}

/// Install with the user's choices. Ok: close the window; `finish` starts
/// the installed copy with `restore` (tab, window place).
pub fn install(c: Choice, restore: Restore) -> std::result::Result<(), String> {
    let s = status();
    if s.at != Where::Portable {
        return Err("Hayate is already installed".into());
    }
    let (exe, dir, src) = (s.exe.clone(), s.install_dir.clone(), s.update_source.clone());
    if dir.is_empty() {
        return Err("%LOCALAPPDATA% is not set".into());
    }
    log(&format!("setup: install clicked: admin_default={} autostart={} update={} -> {dir}", c.admin_default, c.autostart, c.update && src.is_some()));
    let dest = on_sta(move || do_install(&exe, &dir, c, src.as_deref(), false)).unwrap_or_else(|| Err("install thread failed".into()))?;
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(Pending::Launch { exe: dest, restore, admin: c.admin_default });
    Ok(())
}

/// `machine`: the all-users install (elevated process): `%ProgramFiles%\Blink`, the common Start menu,
/// the HKLM Installed-apps entry, the all-users Startup folder. Same layout and values as install.ps1 -Machine.
fn do_install(exe: &str, dir: &str, c: Choice, src: Option<&str>, machine: bool) -> std::result::Result<PathBuf, String> {
    let d = Path::new(dir);
    let dest = d.join("blink.exe");
    if machine {
        if dir.is_empty() || dir != machine_dir() {
            return Err("Unusual install folder: nothing was installed.".into());
        }
        // the source is the running exe: a regular file we can read, never a link
        let m = std::fs::symlink_metadata(exe).map_err(|e| format!("Cannot read {exe}: {e}"))?;
        if !m.is_file() || std::os::windows::fs::MetadataExt::file_attributes(&m) & 0x400 != 0 {
            return Err("This copy of blink.exe is not a plain file: nothing was installed.".into());
        }
        std::fs::File::open(exe).map_err(|e| format!("Cannot read {exe}: {e}"))?;
        // an existing folder must already be admin-only (not a link, no user write)
        if d.exists() {
            crate::taskmgr::verify_admin_only(dir, false).map_err(|e| format!("{dir} is not protected: {e}"))?;
        }
    }
    std::fs::create_dir_all(d).map_err(|e| format!("Cannot create {dir}: {e}"))?;
    copy_exe_atomic(exe, d, &dest, dir)?;
    log(&format!("setup: copied {exe} -> {}", dest.display()));
    // version.txt (the publish step) travels with the exe; leftovers of a self-update go
    let ver = Path::new(osutil::parent_dir(exe)).join("version.txt");
    if ver.is_file() {
        let _ = std::fs::copy(&ver, d.join("version.txt"));
    } else {
        let _ = std::fs::remove_file(d.join("version.txt"));
    }
    for f in ["blink.old.exe", "blink.new.exe", "blink.bad.exe", "blink.new.version.txt", "version.old.txt"] {
        let _ = std::fs::remove_file(d.join(f));
    }
    // shortcuts
    let menu = known_folder(if machine { &FOLDERID_CommonPrograms } else { &FOLDERID_Programs }).ok_or("Start-menu folder not found")?;
    let (links, drop) = menu_links(c.admin_default, crate::notify::AUMID);
    for n in drop {
        let _ = std::fs::remove_file(menu.join(n));
    }
    for l in &links {
        write_link(&menu.join(l.name), &dest, l).map_err(|e| format!("Cannot write the shortcut {}: {e}", l.name))?;
        log(&format!("setup: shortcut {} (runas={}, app id {:?}, toast={})", menu.join(l.name).display(), l.runas, l.app_id, l.toast));
    }
    // an unresolvable Startup folder aborts the install either way (as it always did)
    let startup = startup_folder(machine).ok_or("Startup folder not found")?.join(LINK_AUTO);
    if c.autostart {
        write_autostart(&dest, machine)?;
    } else {
        let _ = std::fs::remove_file(&startup);
    }
    // auto-update
    let ini = osutil::data_dir().join("update.ini");
    match src.filter(|_| c.update) {
        Some(s) => {
            osutil::write_atomic(&ini, update_ini(s, dir).as_bytes()).map_err(|e| format!("Cannot write update.ini: {e}"))?;
            log(&format!("setup: auto-update from {s}"));
        }
        None => {
            let _ = std::fs::remove_file(&ini);
        }
    }
    // toast click handler: the one registry key
    let server = local_server(&dest.to_string_lossy(), &env_args());
    reg_set(&clsid_key(), "Hayate toast activator")?;
    reg_set(&format!(r"{}\LocalServer32", clsid_key()), &server)?;
    log(&format!(r"setup: HKCU\{}\LocalServer32 = {server}", clsid_key()));
    // Settings > Apps > Installed apps: not worth failing an install over
    let hive = if machine { Hive::Machine } else { Hive::User };
    match RealArp.write(hive, ArpKey::Current, &arp_vals(dir)) {
        Ok(()) => {
            log(&format!(r"setup: {hive:?} {ARP_KEY} written (Installed apps)"));
            // the pre-rename `Blink` key of this install goes now (only when it is ours)
            match arp_drop_legacy(&mut RealArp, hive, dir) {
                Ok(true) => {}
                Ok(false) => log("setup: a foreign Uninstall Blink key was left alone"),
                Err(e) => log(&format!("setup: legacy Installed-apps entry not removed: {e}")),
            }
        }
        Err(e) if machine => return Err(format!("Cannot write the Installed-apps entry: {e}")),
        Err(e) => log(&format!("setup: Installed-apps entry not written: {e}")),
    }
    if machine {
        crate::taskmgr::verify_admin_only(&dest.to_string_lossy(), true).map_err(|e| format!("The installed blink.exe is not protected: {e}"))?;
    }
    Ok(dest)
}

/// 64-bit content hash of a file (streamed).
fn file_hash(f: &mut std::fs::File) -> std::io::Result<u64> {
    use std::hash::Hasher;
    use std::io::Read;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.finish());
        }
        h.write(&buf[..n]);
    }
}

/// Copy the source exe through ONE open handle to `blink.new.exe` beside the target, check the
/// copy has the same content as that handle, then rename it over `blink.exe`. A live install is
/// never truncated in place: a failed copy leaves the old exe as it was.
fn copy_exe_atomic(exe: &str, d: &Path, dest: &Path, dir: &str) -> std::result::Result<(), String> {
    use std::io::Seek;
    let tmp = d.join("blink.new.exe");
    let _ = std::fs::remove_file(&tmp);
    let fail = |e: String| format!("Cannot copy blink.exe to {dir}: {e}. If an installed Hayate is running, exit it from its tray menu first.");
    let mut src = std::fs::File::open(exe).map_err(|e| fail(e.to_string()))?;
    let want = {
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| fail(e.to_string()))?;
        let r = std::io::copy(&mut src, &mut out).and_then(|_| out.sync_all());
        if let Err(e) = r {
            drop(out);
            let _ = std::fs::remove_file(&tmp);
            return Err(fail(e.to_string()));
        }
        src.rewind().and_then(|_| file_hash(&mut src))
    };
    let got = std::fs::File::open(&tmp).and_then(|mut t| file_hash(&mut t));
    if want.is_err() || got.is_err() || want.as_ref().ok() != got.as_ref().ok() {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail("the copy did not match the source".into()));
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(fail(e.to_string()));
    }
    log(&format!("setup: copied {exe} -> {} (verified)", dest.display()));
    Ok(())
}

/// "Start with Windows": the Startup shortcut for `exe` (`--resident`, plain,
/// never elevated; no scheduled task, no Run value). The one write for it: the
/// installer's option above and the Startup tab's offer both call this. Must run
/// on an STA thread.
fn write_autostart(exe: &Path, common: bool) -> std::result::Result<PathBuf, String> {
    let startup = startup_folder(common).ok_or("Startup folder not found")?.join(LINK_AUTO);
    write_link(&startup, exe, &autostart_link()).map_err(|e| format!("Cannot write the Startup shortcut: {e}"))?;
    log(&format!("setup: autostart shortcut {} (--resident, not elevated)", startup.display()));
    Ok(startup)
}

/// The Startup tab's "Start Blink with Windows": the same shortcut write as the
/// installer's option, for the per-user installed copy that is running. Any other
/// copy is refused (a portable one would point the shortcut at wherever it happens to
/// sit; a per-machine one is set up by install.ps1), and so is an elevated Blink or
/// one that cannot tell: the Startup folder is user-writable and an elevated process
/// does not write there on its own. Call from the I/O worker, after the user's click.
pub fn enable_autostart() -> std::result::Result<(), String> {
    let s = status();
    if s.at != Where::User {
        return Err("This copy of Hayate is not the per-user installed one. Install Hayate from Settings first.".into());
    }
    if osutil::elevation() != Some(false) {
        return Err("Hayate is running as administrator (or could not tell), so it will not write its Start with Windows shortcut. Do it from a normal Hayate.".into());
    }
    let exe = PathBuf::from(&s.exe);
    on_sta(move || write_autostart(&exe, false).map(|_| ())).unwrap_or_else(|| Err("The shortcut thread failed.".into()))
}

// ---- Installed apps entry ------------------------------------------------------

struct RealArp;

fn arp_root(h: Hive) -> HKEY {
    match h {
        Hive::User => HKEY_CURRENT_USER,
        Hive::Machine => HKEY_LOCAL_MACHINE,
    }
}

impl ArpStore for RealArp {
    fn read_str(&self, h: Hive, key: ArpKey, name: &str) -> Option<String> {
        let (k, v) = (wide(key.path()), wide(name));
        let mut buf = [0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe { RegGetValueW(arp_root(h), pcw(&k), pcw(&v), RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len)) };
        if r.is_err() {
            return None;
        }
        let n = (len as usize / 2).min(buf.len());
        Some(String::from_utf16_lossy(buf[..n].split(|c| *c == 0).next().unwrap_or(&[])))
    }

    fn read_dword(&self, h: Hive, key: ArpKey, name: &str) -> Option<u32> {
        let (k, v) = (wide(key.path()), wide(name));
        let mut d = 0u32;
        let mut len = 4u32;
        let r = unsafe { RegGetValueW(arp_root(h), pcw(&k), pcw(&v), RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY, None, Some(&mut d as *mut u32 as *mut _), Some(&mut len)) };
        r.is_ok().then_some(d)
    }

    fn write(&mut self, h: Hive, ak: ArpKey, vals: &[ArpValue]) -> std::result::Result<(), String> {
        let hive = if h == Hive::User { "HKCU" } else { "HKLM" };
        let k = wide(ak.path());
        let mut key = HKEY::default();
        // the 64-bit view, like install.ps1 (Settings > Apps reads that one)
        let r = unsafe { RegCreateKeyExW(arp_root(h), pcw(&k), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE | KEY_WOW64_64KEY, None, &mut key, None) };
        if r.is_err() {
            return Err(format!("{hive}\\{}: {}", ak.path(), r.to_hresult().message()));
        }
        let mut res = Ok(());
        for v in vals {
            let r = match v {
                ArpValue::Str(n, s) => {
                    let (n, w) = (wide(n), wide(s));
                    let bytes = unsafe { std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2) };
                    unsafe { RegSetValueExW(key, pcw(&n), None, REG_SZ, Some(bytes)) }
                }
                ArpValue::Dword(n, d) => {
                    let n = wide(n);
                    let bytes = d.to_le_bytes();
                    unsafe { RegSetValueExW(key, pcw(&n), None, REG_DWORD, Some(&bytes)) }
                }
            };
            if r.is_err() {
                res = Err(format!("{hive}\\{}: {}", ak.path(), r.to_hresult().message()));
                break;
            }
        }
        unsafe {
            let _ = RegCloseKey(key);
        }
        res
    }

    fn delete(&mut self, h: Hive, key: ArpKey) -> std::result::Result<(), String> {
        // the 64-bit view (where `write` puts it and Settings > Apps reads it)
        let k = wide(key.path());
        let mut key = HKEY::default();
        let r = unsafe { RegOpenKeyExW(arp_root(h), pcw(&k), None, KEY_ALL_ACCESS | KEY_WOW64_64KEY, &mut key) };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if r.is_err() {
            return Err(r.to_hresult().message());
        }
        let t = unsafe { RegDeleteTreeW(key, PCWSTR::null()) };
        unsafe {
            let _ = RegCloseKey(key);
        }
        if t.is_err() {
            return Err(t.to_hresult().message());
        }
        let r = unsafe { RegDeleteKeyExW(arp_root(h), pcw(&k), KEY_WOW64_64KEY.0, None) };
        if r.is_ok() || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(r.to_hresult().message())
        }
    }
}

/// The entry's values for the install in `dir`.
fn arp_vals(dir: &str) -> Vec<ArpValue> {
    let dir = dir.trim_end_matches(['\\', '/']);
    let exe = format!("{dir}\\blink.exe");
    let kb = std::fs::metadata(&exe).map(|m| (m.len() / 1024).min(u32::MAX as u64) as u32).unwrap_or(0);
    arp_values(env!("CARGO_PKG_VERSION"), &exe, dir, kb)
}

/// A normal start of an installed blink.exe: write the Installed-apps entry
/// when it is missing or stale and this process may write it (HKCU from a
/// normal one, HKLM only when already elevated; no UAC from here, ever).
/// Test copies (BLINK_MULTI) and blink-tm.exe never write it.
fn refresh_arp() {
    let s = status();
    if osutil::multi() || !is_blink_exe(&s.exe) || arp_hive(&s.at).is_none() {
        return;
    }
    let (at, dir, elevated) = (s.at.clone(), s.exe_dir.clone(), osutil::is_elevated());
    let _ = std::thread::Builder::new().name("setup-arp".into()).spawn(move || match arp_refresh(&mut RealArp, &at, elevated, &arp_vals(&dir)) {
        Ok(ArpOutcome::Written(h)) => log(&format!("setup: Installed-apps entry refreshed ({h:?})")),
        Ok(_) => {}
        Err(e) => log(&format!("setup: Installed-apps entry not refreshed: {e}")),
    });
}

/// A normal start of an installed copy: shortcuts written before the Hayate
/// rename are renamed (or dropped when the new one exists). Best effort, no UAC:
/// the install's own menu (the all-users menu only from an elevated start) and,
/// for an all-users install, this account's own menu too (legacy `Blink*.lnk`
/// that start our exe; that menu is the user's to write). Failures are only
/// logged. Runs off the start path (shell links need an STA thread).
fn migrate_menu_links() {
    let s = status();
    if osutil::multi() || !is_blink_exe(&s.exe) {
        return;
    }
    let exe = s.exe.clone();
    let folders: Vec<(&'static GUID, bool)> = match arp_hive(&s.at) {
        Some(Hive::User) => vec![(&FOLDERID_Programs, false)],
        Some(Hive::Machine) => vec![(&FOLDERID_CommonPrograms, false), (&FOLDERID_Programs, true)],
        None => return,
    };
    let _ = std::thread::Builder::new().name("setup-menu".into()).spawn(move || {
        let _ = on_sta(move || {
            for (folder, check_target) in folders {
                let Some(menu) = known_folder(folder) else { continue };
                // a legacy link in a menu this install does not own is moved only when it starts our exe
                let have = |n: &str| menu.join(n).exists() && (!check_target || !LEGACY_MENU_LINKS.contains(&n) || link_points_at(&menu.join(n), &exe));
                for m in menu_migration(have) {
                    match m {
                        MenuMove::Rename(a, b) => match std::fs::rename(menu.join(a), menu.join(b)) {
                            Ok(()) => log(&format!("setup: Start-menu shortcut {a} renamed to {b}")),
                            Err(e) => log(&format!("setup: Start-menu shortcut {a} not renamed: {e}")),
                        },
                        MenuMove::DropLegacy(a) => {
                            let _ = std::fs::remove_file(menu.join(a));
                        }
                    }
                }
            }
        });
    });
}

/// The target of a .lnk is `exe` (case-insensitive). Needs an STA thread.
fn link_points_at(link: &Path, exe: &str) -> bool {
    if !link.exists() || norm(exe).is_empty() {
        return false;
    }
    unsafe {
        let Ok(sl) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) else { return false };
        let Ok(pf) = sl.cast::<IPersistFile>() else { return false };
        if pf.Load(&HSTRING::from(link.as_os_str()), STGM_READ).is_err() {
            return false;
        }
        let mut buf = [0u16; 1024];
        if sl.GetPath(&mut buf, std::ptr::null_mut(), 0).is_err() {
            return false;
        }
        let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        norm(&String::from_utf16_lossy(&buf[..n])).eq_ignore_ascii_case(&norm(exe))
    }
}

/// Default value of `HKCU\<key>`.
fn reg_get(key: &str) -> Option<String> {
    let k = wide(key);
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, pcw(&k), PCWSTR::null(), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len)) };
    if r.is_err() {
        return None;
    }
    let n = (len as usize / 2).min(buf.len());
    Some(String::from_utf16_lossy(buf[..n].split(|c| *c == 0).next().unwrap_or(&[])))
}

fn reg_set(key: &str, value: &str) -> std::result::Result<(), String> {
    let (k, v) = (wide(key), wide(value));
    let r = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, pcw(&k), PCWSTR::null(), REG_SZ.0, Some(v.as_ptr() as *const _), (v.len() * 2) as u32) };
    if r.is_ok() {
        Ok(())
    } else {
        Err(format!(r"Cannot write HKCU\{key}: {}", r.to_hresult().message()))
    }
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

/// `(LocalAppData, RoamingAppData, Profile)` of the token's user from the Known Folder API,
/// never from the environment (the Task Manager launcher builds a cold-started Blink's
/// environment from these).
pub fn user_folders() -> Option<(String, String, String)> {
    let g = |id: &GUID| known_folder(id).map(|p| p.to_string_lossy().into_owned());
    Some((g(&FOLDERID_LocalAppData)?, g(&FOLDERID_RoamingAppData)?, g(&FOLDERID_Profile)?))
}

/// `%ProgramFiles(x86)%` / `%ProgramData%` from the Known Folder API (the cold-started
/// Blink's environment is built from these, never from the caller's).
pub fn program_files_x86() -> Option<String> {
    known_folder(&FOLDERID_ProgramFilesX86).map(|p| p.to_string_lossy().into_owned())
}
pub fn program_data() -> Option<String> {
    known_folder(&FOLDERID_ProgramData).map(|p| p.to_string_lossy().into_owned())
}

/// The Startup folder from the Known Folder API: the user's own, or with
/// `common` the all-users one. Never built from %APPDATA% / %ProgramData%.
pub fn startup_folder(common: bool) -> Option<PathBuf> {
    known_folder(if common { &FOLDERID_CommonStartup } else { &FOLDERID_Startup })
}

/// PROPVARIANT as laid out in memory (the property store copies the value).
#[repr(C)]
struct RawPv {
    vt: u16,
    r: [u16; 3],
    p: *const core::ffi::c_void,
    _pad: *const core::ffi::c_void,
}

/// System.AppUserModel.* (fmtid 9F4C2855-…): pid 5 = ID, pid 26 = ToastActivatorCLSID
fn pkey(pid: u32) -> PROPERTYKEY {
    PROPERTYKEY { fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3), pid }
}

/// Set a string (VT_LPWSTR) property on a property store; `w` is NUL-terminated UTF-16.
pub(crate) fn set_string_prop(ps: &windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore, key: &PROPERTYKEY, w: &[u16]) -> Result<()> {
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    let v = RawPv { vt: 31 /* VT_LPWSTR */, r: [0; 3], p: w.as_ptr() as *const _, _pad: std::ptr::null() };
    unsafe { ps.SetValue(key, &v as *const RawPv as *const PROPVARIANT) }
}

/// Settings > Taskbar: is the Jump List on (it is, until switched off)?
pub fn jumplist_enabled() -> bool {
    prefs(|p| !p.jumplist_off)
}

pub fn set_jumplist(on: bool) {
    prefs(|p| p.jumplist_off = !on);
    save_prefs();
    log(&format!("setup: taskbar Jump List switched {}", if on { "on" } else { "off" }));
}

/// A fresh .lnk (like install.ps1's New-Link): target, args, working dir,
/// icon = the exe's first icon, app ID, optional runas flag and activator.
fn write_link(path: &Path, target: &Path, l: &Link) -> Result<()> {
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    let _ = std::fs::remove_file(path);
    let t = HSTRING::from(target.as_os_str());
    let dir = HSTRING::from(target.parent().map(|p| p.as_os_str()).unwrap_or_default());
    unsafe {
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        sl.SetPath(&t)?;
        sl.SetArguments(&HSTRING::from(l.args))?;
        sl.SetWorkingDirectory(&dir)?;
        sl.SetDescription(&HSTRING::from(l.description))?;
        sl.SetIconLocation(&t, 0)?;
        if l.runas {
            let dl: IShellLinkDataList = sl.cast()?;
            let f = dl.GetFlags()?;
            dl.SetFlags(f | SLDF_RUNAS_USER.0 as u32)?;
        }
        let ps: IPropertyStore = sl.cast()?;
        if let Some(id) = &l.app_id {
            let w = wide(id);
            let v = RawPv { vt: 31 /* VT_LPWSTR */, r: [0; 3], p: w.as_ptr() as *const _, _pad: std::ptr::null() };
            ps.SetValue(&pkey(5), &v as *const RawPv as *const PROPVARIANT)?;
        }
        if l.toast {
            let g = GUID::from_u128(CLSID);
            let v = RawPv { vt: 72 /* VT_CLSID */, r: [0; 3], p: &g as *const GUID as *const _, _pad: std::ptr::null() };
            ps.SetValue(&pkey(26), &v as *const RawPv as *const PROPVARIANT)?;
        }
        ps.Commit()?;
        let pf: IPersistFile = sl.cast()?;
        let p = HSTRING::from(path.as_os_str());
        pf.Save(&p, true)?;
        // tell the shell now, so the AUMID shows in shell:AppsFolder (toasts
        // work) within moments instead of whenever Explorer rescans. NOWAIT:
        // SHCNF_FLUSH would block until a slow or hung Explorer handled it.
        SHChangeNotify(SHCNE_CREATE, SHCNF_PATHW | SHCNF_FLUSHNOWAIT, Some(p.as_ptr() as *const _), None);
    }
    Ok(())
}

// ---- uninstall ---------------------------------------------------------------

/// Settings > Uninstall Blink (per-user install only), all in this process:
/// the running exe moves out to %TEMP% (`logic::parked_exe`), then the
/// shortcuts, the activator key, update.ini and the install dir go. The data
/// dir (when `remove_data`) goes in `finish`, after the window has saved its
/// state. Ok: close the window. Err: nothing was removed.
pub fn uninstall(remove_data: bool) -> std::result::Result<(), String> {
    let s = status();
    // which install this exe belongs to (never a path from anywhere else), and what that means here
    let (dir, menu_id, hive) = match s.at {
        Where::User => (s.install_dir.clone(), FOLDERID_Programs, Hive::User),
        Where::Machine => {
            if !osutil::is_elevated() {
                return Err("Removing Hayate for all users needs administrator rights.".into());
            }
            (machine_dir(), FOLDERID_CommonPrograms, Hive::Machine)
        }
        Where::Portable => return Err("Only an installed Hayate can uninstall itself".into()),
    };
    if !is_blink_exe(&s.exe) {
        return Err("Only the installed blink.exe can uninstall Hayate".into());
    }
    if !safe_to_remove(&dir) {
        return Err(format!("Unusual install path {dir}: delete it by hand"));
    }
    let data = osutil::data_dir().to_string_lossy().into_owned();
    let data_ok = remove_data && data_dir_removable(&data, &dir);
    log(&format!("setup: uninstall clicked (remove data: {remove_data}{})", if remove_data && !data_ok { ", but the data dir is not Blink's own: kept" } else { "" }));
    // BEFORE anything is removed: give every Windows startup entry Blink switched off back. Blink's copy of
    // the original is the only record of what to restore; once the exe/data are gone the user would be left
    // with entries disabled by a program that no longer exists. Any failure = remove nothing.
    match crate::startup::run::release_all_owned() {
        Ok(r) => {
            log(&format!("setup: startup entries: {}", r.summary()));
            // an all-users record without its administrator-protected copy is never acted on, and never stops the uninstall
            for w in &r.skipped {
                log(&format!("setup: WARNING startup entry {w}"));
            }
        }
        Err(e) => {
            log(&format!("setup: uninstall stopped, startup entries could not be given back: {e}"));
            return Err(e);
        }
    }
    // a Blink of this install that sits in the tray holds the folder too
    quit_running_copies_of(&s.exe);
    // then the one step that can fail: a running exe cannot be deleted, only renamed
    let temp = park_root().ok_or("The Local AppData folder was not found. Nothing was removed.")?;
    let parked = parked_exe(&temp, std::process::id());
    let _ = std::fs::create_dir_all(osutil::parent_dir(&parked));
    if osutil::reparse_in_chain(Path::new(osutil::parent_dir(&parked))) {
        return Err("The folder for the removed copy is a link, so it is not used. Nothing was removed.".into());
    }
    std::fs::rename(&s.exe, &parked).map_err(|e| format!("Cannot move the running blink.exe out of {dir}: {e}. Nothing was removed."))?;
    log(&format!("setup: running exe moved to {parked} (deleted at the next Blink start)"));
    let machine = s.at == Where::Machine;
    let own_exe = s.exe.clone();
    let removed = on_sta(move || {
        let mut out = Vec::new();
        if let Some(menu) = known_folder(&menu_id) {
            for n in all_menu_links() {
                if std::fs::remove_file(menu.join(n)).is_ok() {
                    out.push(menu.join(n).display().to_string());
                }
            }
        }
        // the OTHER menu too (legacy per-user links beside an all-users install, or the reverse), but only links that start this exe
        let other = if machine { &FOLDERID_Programs } else { &FOLDERID_CommonPrograms };
        if let Some(menu) = known_folder(other) {
            for n in all_menu_links() {
                let f = menu.join(n);
                if link_points_at(&f, &own_exe) && std::fs::remove_file(&f).is_ok() {
                    out.push(f.display().to_string());
                }
            }
        }
        // this account's Startup link, and (all-users install) the all-users one; other users' are theirs
        for common in [false, true] {
            if common && !machine {
                continue;
            }
            if let Some(st) = startup_folder(common) {
                if std::fs::remove_file(st.join(LINK_AUTO)).is_ok() {
                    out.push(st.join(LINK_AUTO).display().to_string());
                }
            }
        }
        out
    })
    .unwrap_or_default();
    // the Jump List points at the exe that is going: take it out (waits at most 3 s)
    crate::taskbar::remove_for_uninstall(std::time::Duration::from_secs(3));
    // the toast key of this account, only when it starts the exe being removed (another user's, or another install's, stays)
    let ls = reg_get(&format!(r"{}\LocalServer32", clsid_key()));
    if clsid_ours(ls.as_deref(), &dir) {
        let k = wide(&clsid_key());
        let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, pcw(&k)) };
        log(&format!(r"setup: removed {removed:?}; HKCU\{} {}", clsid_key(), if r.is_ok() { "deleted" } else { "not there" }));
    } else {
        log(&format!(r"setup: removed {removed:?}; HKCU\{} kept (it does not start the exe being removed)", clsid_key()));
    }
    match arp_remove_all(&mut RealArp, hive, &dir) {
        Ok(()) => log(&format!("setup: {hive:?} Installed-apps entries removed (both key names)")),
        Err(e) => log(&format!("setup: Installed-apps entry not removed: {e}")),
    }
    let dd = osutil::data_dir();
    for f in ["update.ini", "update-state.ini", "update-uac.txt", "jumplist.ini"] {
        let _ = std::fs::remove_file(dd.join(f));
    }
    // our working dir is the install dir (`launch`, the shortcuts): that open
    // handle alone makes the folder undeletable (os error 32)
    let _ = std::env::set_current_dir(&temp);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => log(&format!("setup: removed {dir}")),
        Err(e) => log(&format!("setup: could not remove {dir}: {e}")),
    }
    let left = Path::new(&dir).exists().then(|| dir.clone());
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(Pending::Removed(data_ok.then_some(data), left));
    Ok(())
}

/// `<LocalAppData>\Temp` from the known folder (not `%TEMP%`): where the
/// running exe is parked and where the sweep looks.
fn park_root() -> Option<String> {
    known_folder(&FOLDERID_LocalAppData).map(|p| park_base(&p.to_string_lossy()))
}

/// Ask a Blink of this very exe (tray, hidden) to quit, wait a few seconds.
/// Only a process of the same or lower integrity gets the message; an
/// elevated one stays (the exe can still be renamed under it).
fn quit_running_copies_of(exe: &str) {
    let me = norm(exe);
    // every Blink window (normal and test class) running exactly this image, not this process
    let same = || {
        let mut found: Vec<HWND> = Vec::new();
        for class in [w!("BlinkMain"), w!("BlinkMainMulti")] {
            let mut prev: Option<HWND> = None;
            loop {
                let Ok(h) = (unsafe { FindWindowExW(None, prev, class, None) }) else { break };
                if h.is_invalid() {
                    break;
                }
                prev = Some(h);
                let mut pid = 0u32;
                unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
                if pid != std::process::id() && osutil::image_path(pid).is_some_and(|p| norm(&p) == me) {
                    found.push(h);
                }
            }
        }
        found
    };
    let first = same();
    if first.is_empty() {
        return;
    }
    for h in &first {
        unsafe {
            let _ = PostMessageW(Some(*h), crate::win::quit_msg(), WPARAM(0), LPARAM(0));
        }
    }
    log(&format!("setup: asked {} running Blink(s) of this install to quit", first.len()));
    for _ in 0..12 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if same().is_empty() {
            return;
        }
    }
    log("setup: that Blink is still running; continuing (the exe is only renamed)");
}

/// Delete exes parked by an earlier uninstall (`<LocalAppData>\Temp\Blink-uninstalled`).
/// Off the UI thread; only `blink-<pid>.exe` names; a still-running one
/// just stays for next time.
fn sweep_parked() {
    let Some(root) = park_root() else { return };
    let dir = PathBuf::from(root).join(PARK_DIR);
    if !dir.is_dir() {
        return;
    }
    if osutil::reparse_in_chain(&dir) {
        return;
    }
    let _ = std::thread::Builder::new().name("setup-sweep".into()).spawn(move || {
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        for e in rd.flatten() {
            if is_parked_name(&e.file_name().to_string_lossy()) && std::fs::remove_file(e.path()).is_ok() {
                log(&format!("setup: deleted {} (left by an uninstall)", e.path().display()));
            }
        }
        let _ = std::fs::remove_dir(&dir); // only when empty
    });
}

// ---- the confirm flow (Installed apps, `--uninstall`, Settings) ------------------

pub enum Flow {
    /// the user said No (or cancelled UAC): nothing was removed
    Cancelled,
    /// all-users install, not elevated: an elevated copy was started through UAC and asks there
    Elevated,
    /// removed (the flag: settings and history went too)
    Done(bool),
    Failed(String),
}

fn message_box(owner: Option<HWND>, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    message_box_titled(owner, text, UNINSTALL_TITLE, style)
}

fn message_box_titled(owner: Option<HWND>, text: &str, title: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let (t, c) = (wide(text), wide(title));
    unsafe { MessageBoxW(owner, pcw(&t), pcw(&c), style | MB_SETFOREGROUND) }
}

/// "Uninstall Blink" with native boxes: confirm, then "also delete settings and
/// history?", then the Task Manager redirect gate and `uninstall`. An all-users
/// install without admin rights is handed to UAC first (same exe, `--uninstall`
/// only), and the questions are asked there. Only ever acts on the install this
/// exe is running from (`uninstall_plan`).
pub fn uninstall_flow(owner: Option<HWND>) -> Flow {
    let s = status();
    match uninstall_plan(&s.exe, &user_dir(), &machine_dir(), osutil::is_elevated()) {
        Err(e) => return Flow::Failed(e),
        Ok(UninstallPlan::Elevate) => {
            // never the caller's data dir / multi flag: the elevated copy uses its own known-folder data dir
            return match runas(Path::new(&s.exe), &uninstall_args(&[], osutil::user_sid_hex().as_deref())) {
                Ok(()) => {
                    log("setup: uninstall: all-users install, UAC accepted, the elevated copy asks");
                    Flow::Elevated
                }
                Err(e) if e == "cancelled" => {
                    log("setup: uninstall: UAC cancelled, nothing removed");
                    Flow::Cancelled
                }
                Err(e) => Flow::Failed(format!("Could not ask for administrator rights: {e}")),
            };
        }
        Ok(UninstallPlan::Run) => {}
    }
    if message_box(owner, &confirm_text(&s.at), MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2) != IDYES {
        log("setup: uninstall: not confirmed");
        return Flow::Cancelled;
    }
    let dir = if s.at == Where::Machine { machine_dir() } else { s.install_dir.clone() };
    let data = osutil::data_dir().to_string_lossy().into_owned();
    // the settings folder belongs to the elevated account: when UAC was answered with another admin's
    // credentials it is not the requester's, so it is neither offered nor touched
    let cli: Vec<String> = std::env::args().skip(1).collect();
    let same = same_account(requester_of(&cli).as_deref(), osutil::user_sid_hex().as_deref());
    if !same {
        log("setup: uninstall: the elevated account is not the one that asked; settings and history are kept");
    }
    let remove_data = same && data_dir_removable(&data, &dir) && message_box(owner, &data_question(&s.at, &data), MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2) == IDYES;
    // a Task Manager redirect that is on goes off first (UAC); declined: nothing is removed
    match crate::taskmgr::uninstall_gate().and_then(|()| uninstall(remove_data)) {
        // the "removed" box comes from `finish`, after the settings folder is dealt with
        Ok(()) => Flow::Done(remove_data),
        Err(e) => Flow::Failed(e),
    }
}

/// `blink.exe --uninstall` (the Installed-apps entry): no window, no tray, no
/// single-instance check; the boxes above, then exit. True when this process
/// was started that way.
pub fn uninstall_cli() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !is_uninstall_launch(&args) {
        return false;
    }
    log(&format!("setup: --uninstall (elevated: {})", osutil::is_elevated()));
    match uninstall_flow(None) {
        Flow::Done(_) => {
            crate::taskbar::drain(std::time::Duration::from_secs(3));
            // the data dir (if asked) goes now and the process ends there
            finish();
        }
        Flow::Failed(e) => {
            log(&format!("setup: --uninstall failed: {e}"));
            message_box(None, &e, MB_OK | MB_ICONERROR);
        }
        Flow::Cancelled | Flow::Elevated => {}
    }
    true
}

// ---- all-users install from the app ("Install for everyone on this PC") ------------

/// A copy installed just for this user exists (the panel then offers to replace it).
pub fn user_install_exists() -> bool {
    let d = user_dir();
    !d.is_empty() && Path::new(&d).join("blink.exe").is_file()
}

/// The window's click: ask UAC for this very exe with the fixed `--install-machine` flags (no
/// path arguments: the elevated copy installs itself), wait for it. `Ok(true)`: installed, and
/// the elevated copy has started the installed Blink; `Ok(false)`: UAC was cancelled.
fn install_all(c: Choice) -> std::result::Result<bool, String> {
    let s = status();
    machine_install_plan(&s.at, osutil::is_elevated())?;
    log(&format!("setup: install for everyone clicked: admin_default={} autostart={} update={} replace_user={}", c.admin_default, c.autostart, c.update, c.replace_user));
    let mut args = install_machine_args(&c);
    // the installed copy waits for this window to be gone (it would hand its start to us otherwise)
    args.extend(["--wait-pid".into(), std::process::id().to_string()]);
    if let Some(r) = osutil::user_sid_hex() {
        args.extend([ARG_REQUESTER.to_string(), r]);
    }
    match runas_wait(Path::new(&s.exe), &args) {
        Ok(MI_OK) => Ok(true),
        Ok(MI_DECLINED) => {
            log("setup: install for everyone: the newer all-users copy was kept");
            Ok(false)
        }
        Ok(code) => Err(machine_exit_text(code).into()),
        Err(e) if e == "timeout" => Err("The installer is still running (not finished after 3 minutes). Wait a minute, then look in Settings > Apps > Installed apps before trying again.".into()),
        Err(e) if e == "cancelled" => {
            log("setup: install for everyone: UAC cancelled");
            Ok(false)
        }
        Err(e) => Err(format!("Could not ask for administrator rights: {e}")),
    }
}

static IA_RESULT: Mutex<Option<std::result::Result<bool, String>>> = Mutex::new(None);
static IA_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The window's click: UAC, the elevated copy and the wait run on a worker thread (the window
/// stays alive); `done` runs there when the result is ready and posts to the UI thread.
pub fn start_install_all(c: Choice, done: impl FnOnce() + Send + 'static) {
    if IA_BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("install-all".into()).spawn(move || {
        let r = install_all(c);
        *IA_RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        IA_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
        done();
    });
}

/// The finished run's result, once: Ok(true) installed and started, Ok(false) cancelled/declined.
pub fn take_install_all_result() -> Option<std::result::Result<bool, String>> {
    IA_RESULT.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// `blink.exe --install-machine ...`: the elevated half. No window, no tray; copies itself to
/// `%ProgramFiles%\Blink`, writes the all-users entries, starts the installed copy unelevated and
/// exits with `MI_*`. True when this process was started that way (it never returns then).
pub fn install_machine_cli() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(c) = parse_install_machine(&args) else { return false };
    let s = status();
    log(&format!("setup: --install-machine (elevated: {})", osutil::is_elevated()));
    let code = match machine_install_plan(&s.at, osutil::is_elevated()) {
        Err(e) => {
            log(&format!("setup: install for everyone refused: {e}"));
            MI_REFUSED
        }
        Ok(MachinePlan::Elevate) => runas_wait(Path::new(&s.exe), &args).unwrap_or(MI_FAILED),
        Ok(MachinePlan::Run) => {
            let wait = args.iter().position(|a| a == "--wait-pid").and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<u32>().ok());
            run_machine_install(&c, wait, requester_of(&args))
        }
    };
    log(&format!("setup: --install-machine finished with {code}"));
    std::process::exit(code);
}

/// What was there before an all-users install, so a failure undoes only what the run created.
struct Pre {
    dir: bool,
    exe: bool,
    arp: bool,
    links: Vec<&'static str>,
    startup: bool,
}

fn pre_state(dir: &str) -> Pre {
    let menu = known_folder(&FOLDERID_CommonPrograms);
    Pre {
        dir: Path::new(dir).exists(),
        exe: Path::new(dir).join("blink.exe").exists(),
        arp: RealArp.read_str(Hive::Machine, ArpKey::Current, "InstallLocation").is_some(),
        links: all_menu_links().filter(|n| menu.as_ref().is_some_and(|m| m.join(n).exists())).collect(),
        startup: startup_folder(true).is_some_and(|s| s.join(LINK_AUTO).exists()),
    }
}

fn run_machine_install(c: &Choice, wait_pid: Option<u32>, requester: Option<String>) -> i32 {
    let s = status();
    let dir = machine_dir();
    if dir.is_empty() {
        log("setup: install for everyone: Program Files folder not found");
        return MI_FAILED;
    }
    let pre = pre_state(&dir);
    let dest_exe = Path::new(&dir).join("blink.exe");
    if pre.exe {
        // an all-users copy is there: replacing it with one that is not newer needs an explicit Yes
        let same = std::fs::File::open(&s.exe).and_then(|mut a| file_hash(&mut a)).ok().zip(std::fs::File::open(&dest_exe).and_then(|mut b| file_hash(&mut b)).ok()).is_some_and(|(a, b)| a == b);
        let ver = |p: &str| std::fs::read_to_string(Path::new(p).join("version.txt")).ok();
        let (ev, sv) = (ver(&dir), ver(osutil::parent_dir(&s.exe)));
        let mt = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let not_older = mt(&dest_exe).zip(mt(Path::new(&s.exe))).is_some_and(|(e, n)| e >= n);
        if overwrite_needs_confirm(ev.as_deref(), sv.as_deref(), same, not_older) && message_box_titled(None, OVERWRITE_QUESTION, "Install Hayate", MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2) != IDYES {
            log("setup: install for everyone: not replacing the installed copy");
            return MI_DECLINED;
        }
        quit_running_copies_of(&dest_exe.to_string_lossy());
    }
    let (exe, d2, src, ch) = (s.exe.clone(), dir.clone(), s.update_source.clone(), *c);
    let done = on_sta(move || do_install(&exe, &d2, ch, src.as_deref(), true)).unwrap_or_else(|| Err("install thread failed".into()));
    let dest = match done {
        Ok(p) => p,
        Err(e) => {
            log(&format!("setup: install for everyone failed: {e}; undoing"));
            rollback_machine(&dir, &pre);
            return MI_FAILED;
        }
    };
    if c.replace_user && user_install_exists() {
        if same_account(requester.as_deref(), osutil::user_sid_hex().as_deref()) {
            remove_user_install();
        } else {
            log("setup: the per-user copy belongs to another account than the one that asked: left alone");
        }
    }
    // the installed copy starts as the normal user, never with this admin token
    let mut args = relaunch_args(&[], &Restore { show: Some(crate::update::logic::Show::Front), ..Default::default() });
    args.extend(["--wait-pid".into(), wait_pid.unwrap_or_else(std::process::id).to_string()]);
    let line = command_line(&dest.to_string_lossy(), &args);
    match crate::rules::actions::launch_unelevated(&line, Some(&dir)) {
        Ok(_) => {
            log("setup: installed copy started unelevated (through Explorer)");
            MI_OK
        }
        Err(e) => {
            // installed fine; only the start failed: say so, keep the window
            log(&format!("setup: start of the installed copy failed: {e}"));
            MI_LAUNCH_FAILED
        }
    }
}

/// Undo a failed all-users install: what this run wrote, nothing more.
fn rollback_machine(dir: &str, pre: &Pre) {
    if let Some(menu) = known_folder(&FOLDERID_CommonPrograms) {
        for n in all_menu_links() {
            if !pre.links.contains(&n) {
                let _ = std::fs::remove_file(menu.join(n));
            }
        }
    }
    if !pre.startup {
        if let Some(st) = startup_folder(true) {
            let _ = std::fs::remove_file(st.join(LINK_AUTO));
        }
    }
    if !pre.arp {
        let _ = RealArp.delete(Hive::Machine, ArpKey::Current);
    }
    // an earlier install is left fully intact; only a folder this run created goes
    if !pre.dir && safe_to_remove(dir) && !osutil::reparse_in_chain(Path::new(dir)) {
        let _ = std::fs::remove_dir_all(dir);
    } else if !pre.exe {
        let _ = std::fs::remove_file(Path::new(dir).join("blink.exe"));
        let _ = std::fs::remove_file(Path::new(dir).join("blink.new.exe"));
    }
}

/// The copy installed just for this account goes (after the all-users one is in place): its
/// shortcuts, Installed-apps entry and folder. Settings and history stay.
fn remove_user_install() {
    let dir = user_dir();
    let exe = format!(r"{dir}\blink.exe");
    if dir.is_empty() || !safe_to_remove(&dir) || !is_blink_exe(&exe) || osutil::reparse_in_chain(Path::new(&dir)) {
        log(&format!("setup: the per-user copy in {dir} was left alone (unusual path)"));
        return;
    }
    quit_running_copies_of(&exe);
    let menu = on_sta(|| {
        if let Some(menu) = known_folder(&FOLDERID_Programs) {
            for n in all_menu_links() {
                let _ = std::fs::remove_file(menu.join(n));
            }
        }
    });
    let _ = menu;
    if RealArp.read_str(Hive::User, ArpKey::Current, "InstallLocation").is_some_and(|l| norm(&l) == norm(&dir)) {
        let _ = RealArp.delete(Hive::User, ArpKey::Current);
    }
    let _ = arp_drop_legacy(&mut RealArp, Hive::User, &dir);
    // this account's own Startup link and toast key, when they belong to the copy that goes
    if let Some(st) = startup_folder(false) {
        let _ = std::fs::remove_file(st.join(LINK_AUTO));
    }
    if clsid_ours(reg_get(&format!(r"{}\LocalServer32", clsid_key())).as_deref(), &dir) {
        let k = wide(&clsid_key());
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, pcw(&k));
        }
    }
    // the quit above is asynchronous: give the old copy a moment to let go of its exe
    let mut last = None;
    for _ in 0..25 {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                log(&format!("setup: removed the per-user copy {dir}"));
                return;
            }
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if let Some(e) = last {
        log(&format!("setup: could not remove the per-user copy {dir}: {e}"));
    }
}

/// UAC start of `exe` that is waited for: its exit code.
fn runas_wait(exe: &Path, args: &[String]) -> std::result::Result<i32, String> {
    let (file, params) = (wide(&exe.to_string_lossy()), wide(&join_args(args)));
    let dir = wide(&exe.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default());
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI | SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: pcw(&file),
        lpParameters: pcw(&params),
        lpDirectory: pcw(&dir),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        ShellExecuteExW(&mut sei).map_err(|e| if e.code() == ERROR_CANCELLED.to_hresult() { "cancelled".to_string() } else { e.message() })?;
        let h = sei.hProcess;
        if h.is_invalid() {
            return Err("the administrator copy did not start".into());
        }
        if WaitForSingleObject(h, 180_000) == WAIT_TIMEOUT {
            let _ = CloseHandle(h);
            return Err("timeout".into());
        }
        let mut code = 0u32;
        let r = GetExitCodeProcess(h, &mut code);
        let _ = CloseHandle(h);
        r.map_err(|e| e.message())?;
        Ok(code as i32)
    }
}

// ---- after the window closed -------------------------------------------------

/// From main after win::run returned: start the installed copy, or the
/// uninstall's folder cleanup.
pub fn finish() {
    let Some(p) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    match p {
        Pending::Launch { exe, restore, admin } => launch(&exe, restore, admin),
        Pending::Removed(data, left) => {
            // the window has saved its state; let queued writes land, then
            // delete and exit at once so nothing recreates the folder
            let outcome = match data {
                None => DataOutcome::Kept,
                Some(dir) => {
                    log(&format!("setup: removing the data dir {dir}"));
                    crate::bgio::flush(std::time::Duration::from_millis(1500));
                    for _ in 0..10 {
                        if std::fs::remove_dir_all(&dir).is_ok() || !Path::new(&dir).exists() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    if Path::new(&dir).exists() {
                        log(&format!("setup: the data dir {dir} could not be deleted"));
                        DataOutcome::Failed(dir)
                    } else {
                        DataOutcome::Deleted
                    }
                }
            };
            message_box(None, &done_text(&outcome, left.as_deref()), MB_OK | MB_ICONINFORMATION);
            std::process::exit(0);
        }
    }
}

/// Start the installed copy. Admin by default: a UAC prompt (the user's
/// consent); cancelled or failed: the same build unelevated.
fn launch(exe: &Path, mut restore: Restore, admin: bool) {
    let pid = std::process::id().to_string();
    let elevated = osutil::is_elevated();
    if admin && !elevated {
        // runas goes through the AppInfo service: our environment does not carry over
        restore.data_dir = std::env::var("BLINK_DATA_DIR").ok().filter(|d| !d.is_empty());
        restore.multi = osutil::multi();
        let mut args = relaunch_args(&[], &restore);
        args.extend(["--wait-pid".into(), pid.clone()]);
        match runas(exe, &args) {
            Ok(()) => {
                log("setup: installed copy started elevated (user consented)");
                return;
            }
            Err(e) => log(&format!("setup: UAC start of the installed copy: {e}; starting it unelevated")),
        }
        restore.data_dir = None;
        restore.multi = false;
    }
    let mut args = relaunch_args(&[], &restore);
    args.extend(["--wait-pid".into(), pid]);
    let line = command_line(&exe.to_string_lossy(), &args);
    let dir = exe.parent().map(|d| d.to_string_lossy().into_owned());
    if elevated && !admin {
        // installed as a normal app: do not hand it our admin token
        match crate::rules::actions::launch_unelevated(&line, dir.as_deref()) {
            Ok(_) => log("setup: installed copy started unelevated (through Explorer)"),
            Err(e) => log(&format!("setup: start of the installed copy failed: {e}")),
        }
        return;
    }
    let mut cmd = wide(&line);
    let (app, cwd) = (wide(&exe.to_string_lossy()), wide(dir.as_deref().unwrap_or("")));
    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        match CreateProcessW(pcw(&app), Some(PWSTR(cmd.as_mut_ptr())), None, None, false, PROCESS_CREATION_FLAGS(0), None, pcw(&cwd), &si, &mut pi) {
            Ok(()) => {
                log(&format!("setup: installed copy started, pid {}", pi.dwProcessId));
                let _ = CloseHandle(pi.hThread);
                let _ = CloseHandle(pi.hProcess);
            }
            Err(e) => log(&format!("setup: start of the installed copy failed: {e}")),
        }
    }
}

pub(crate) fn runas(exe: &Path, args: &[String]) -> std::result::Result<(), String> {
    let (file, params) = (wide(&exe.to_string_lossy()), wide(&join_args(args)));
    let dir = wide(&exe.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default());
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: w!("runas"),
        lpFile: pcw(&file),
        lpParameters: pcw(&params),
        lpDirectory: pcw(&dir),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        ShellExecuteExW(&mut sei).map_err(|e| if e.code() == ERROR_CANCELLED.to_hresult() { "cancelled".to_string() } else { e.message() })
    }
}

/// Window state for the installed copy's first start: in front, same tab,
/// same place (the update relaunch args).
pub fn restore_of(hwnd: HWND, tab: usize) -> Restore {
    let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    let place = unsafe { GetWindowPlacement(hwnd, &mut wp) }.ok().map(|_| {
        let r = wp.rcNormalPosition;
        (r.left, r.top, r.right, r.bottom)
    });
    let maximized = unsafe { IsZoomed(hwnd).as_bool() };
    Restore { show: Some(crate::update::logic::Show::Front), tab: Some(tab), place, maximized, ..Default::default() }
}
