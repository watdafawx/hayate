//! Enumerate every startup entry the current user's session can start:
//! Run values (this user, all users, all users 32-bit) and the two Startup
//! folders, each with its StartupApproved state. Read-only.
//!
//! Runs on the I/O worker: it reads the registry, lists folders and opens
//! every `.lnk` (never `Resolve`: no search for a moved target, nothing that
//! could hang on a dead share).

use windows::Win32::System::Registry::*;

use super::approved::{self, Live, Locations};
use super::id::{EntryId, Kind, Scope, View, MAX_KEY};
use super::reg;
use crate::osutil::{from_wide, pcw, wide};

/// Entries kept per source (a runaway key or folder cannot flood the list).
pub const MAX_PER_SOURCE: usize = 512;

/// What a `.lnk` points at, as stored in the file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Target {
    pub path: String,
    pub args: String,
    pub dir: String,
    /// the file the shortcut takes its icon from
    pub icon: String,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: EntryId,
    /// the Run value as stored (REG_EXPAND_SZ left unexpanded), or the full file path
    pub raw: String,
    /// `raw` with %VARS% expanded: what actually runs (a Run value) or the file path
    pub command: String,
    /// resolved shortcut target (folder `.lnk` only)
    pub target: Option<Target>,
    /// image whose icon the row shows
    pub icon: String,
    pub live: Live,
    /// this entry starts Blink itself
    pub is_blink: bool,
    /// what the command / shortcut looked like when it was read (None = it
    /// could not be read, so taking it over is refused)
    pub sig: Option<SourceSig>,
}

/// The exact content a startup entry runs from: the registry value as stored
/// (type and raw bytes) or the shortcut file (length and a FNV-1a hash of its
/// bytes). Compared again before and after Blink takes the entry over, so an
/// installer that replaced the program in between is noticed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSig {
    Run(u32, Vec<u8>),
    File(u64, u64),
}

/// Shortcut files are tiny; anything bigger is not hashed (None = unreadable).
const MAX_SIG_FILE: u64 = 1024 * 1024;

pub fn file_sig(path: &std::path::Path) -> Option<SourceSig> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > MAX_SIG_FILE {
        return None;
    }
    let b = std::fs::read(path).ok()?;
    if b.len() as u64 != md.len() {
        return None;
    }
    let mut h = 0xcbf29ce484222325u64;
    for x in &b {
        h ^= *x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    Some(SourceSig::File(b.len() as u64, h))
}

/// The entry's source as it is right now (None = missing or unreadable).
pub fn source_sig(loc: &Locations, id: &EntryId) -> Option<SourceSig> {
    match id.kind {
        Kind::Run => {
            let k = reg::open(loc.root(id.scope), Locations::run_path(id), KEY_READ | KEY_WOW64_64KEY).ok()?;
            reg::get(&k, &id.key).ok().flatten().map(|(ty, b)| SourceSig::Run(ty, b))
        }
        Kind::Folder => {
            let d = loc.dir(id.scope);
            if d.as_os_str().is_empty() {
                return None;
            }
            file_sig(&d.join(&id.key))
        }
    }
}

impl Entry {
    /// Name shown in lists: the key, without a shortcut extension.
    pub fn display(&self) -> String {
        let k = &self.id.key;
        let low = k.to_ascii_lowercase();
        if self.id.kind == Kind::Folder && (low.ends_with(".lnk") || low.ends_with(".url")) {
            k[..k.len() - 4].to_string()
        } else {
            k.clone()
        }
    }

    /// The command line as the user should read it: for a shortcut its target and arguments.
    pub fn shown_command(&self) -> String {
        match &self.target {
            Some(t) if !t.path.is_empty() => {
                if t.args.is_empty() {
                    format!("\"{}\"", t.path)
                } else {
                    format!("\"{}\" {}", t.path, t.args)
                }
            }
            _ => self.command.clone(),
        }
    }

    pub fn state(&self) -> approved::State {
        self.live.effective()
    }
}

pub struct Scan {
    pub entries: Vec<Entry>,
    /// sources that could not be read, in words
    pub problems: Vec<String>,
}

/// `%NAME%` expansion from the process environment (display and launch of a Run
/// value only; Known Folders are never taken from the environment).
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('%') {
        out.push_str(&rest[..p]);
        let after = &rest[p + 1..];
        match after.find('%') {
            Some(q) if q > 0 => {
                let var = &after[..q];
                match std::env::var(var) {
                    Ok(v) if !var.contains(['=', '\0']) => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(var);
                        out.push('%');
                    }
                }
                rest = &after[q + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn scan(loc: &Locations) -> Scan {
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    let runs = [(Scope::User, View::Native, "Run (this user)"), (Scope::AllUsers, View::Native, "Run (all users)"), (Scope::AllUsers, View::Wow32, "Run (all users, 32-bit)")];
    for (scope, view, label) in runs {
        let probe = EntryId::new(Kind::Run, scope, view, "x");
        let key = match reg::open(loc.root(scope), Locations::run_path(&probe), KEY_READ | KEY_WOW64_64KEY) {
            Ok(k) => k,
            Err(e) if e.not_found() => continue,
            Err(e) => {
                problems.push(format!("{label}: {}", e.text()));
                continue;
            }
        };
        let vals = match reg::values(&key) {
            Ok(v) => v,
            Err(e) => {
                problems.push(format!("{label}: {}", e.text()));
                continue;
            }
        };
        for (name, ty, data) in vals.into_iter().take(MAX_PER_SOURCE) {
            let data_copy = data.clone();
            if name.is_empty() || name.chars().count() > MAX_KEY || name.chars().any(|c| c.is_control()) {
                continue; // the default value or an unaddressable name is not an entry
            }
            let Some(raw) = reg::text(&(ty, data)).filter(|s| !s.is_empty()) else { continue };
            let command = if ty == REG_EXPAND_SZ.0 { expand_env(&raw) } else { raw.clone() };
            let id = EntryId::new(Kind::Run, scope, view, name);
            let live = match approved::read(loc, &id) {
                Ok(l) => l,
                Err(e) => {
                    problems.push(format!("{}: state unreadable ({})", id.key, e.text()));
                    Live::unreadable()
                }
            };
            let icon = crate::osutil::clean_image_path(&command);
            let is_blink = is_blink_image(&icon);
            entries.push(Entry { id, raw, command, target: None, icon, live, is_blink, sig: Some(SourceSig::Run(ty, data_copy)) });
        }
    }
    for scope in [Scope::User, Scope::AllUsers] {
        let dir = loc.dir(scope);
        if dir.as_os_str().is_empty() {
            problems.push(format!("{} Startup folder: path unknown", if scope == Scope::User { "User" } else { "Common" }));
            continue;
        }
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                problems.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        let mut n = 0;
        // shortcuts are parsed by the shell's COM code: only from a Blink that is
        // provably NOT elevated (the folder is user-writable; a high process must
        // not feed it to a parser). Unknown elevation counts as elevated.
        let parse_links = crate::osutil::elevation() == Some(false);
        let mut noted_links = false;
        for e in rd.flatten() {
            if n >= MAX_PER_SOURCE {
                break;
            }
            // not following anything: the entry's own attributes (DirEntry::metadata does not traverse a link)
            let Ok(md) = e.metadata() else { continue };
            let reparse = std::os::windows::fs::MetadataExt::file_attributes(&md) & 0x400 != 0;
            if !reparse && !md.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case("desktop.ini") || name.is_empty() || name.chars().count() > MAX_KEY || name.chars().any(|c| c.is_control()) {
                continue;
            }
            n += 1;
            let file = e.path().to_string_lossy().into_owned();
            let id = EntryId::new(Kind::Folder, scope, View::Native, name.clone());
            let live = if reparse {
                // a symlink / junction-like file: listed by name so it is not invisible, never read, resolved or taken over
                problems.push(format!("{name}: is a link, not a regular file. It is listed by name only and Hayate will not change it."));
                Live::unreadable()
            } else {
                match approved::read(loc, &id) {
                    Ok(l) => l,
                    Err(e) => {
                        problems.push(format!("{name}: state unreadable ({})", e.text()));
                        Live::unreadable()
                    }
                }
            };
            let is_lnk = name.to_ascii_lowercase().ends_with(".lnk");
            if is_lnk && !parse_links && !reparse && !noted_links {
                noted_links = true;
                problems.push("Shortcut targets are not read while Hayate runs as administrator (or when that cannot be told): shortcuts are listed by name only.".into());
            }
            let target = if is_lnk && parse_links && !reparse { resolve_link(&file) } else { None };
            let icon = match &target {
                Some(t) if !t.icon.is_empty() => t.icon.clone(),
                Some(t) => t.path.clone(),
                None => file.clone(),
            };
            let is_blink = target.as_ref().is_some_and(|t| is_blink_image(&t.path)) || (!is_lnk && is_blink_image(&file));
            let sig = if reparse { None } else { file_sig(&e.path()) };
            entries.push(Entry { id, raw: file.clone(), command: file.clone(), target, icon, live, is_blink, sig });
        }
    }
    entries.sort_by(|a, b| a.display().to_lowercase().cmp(&b.display().to_lowercase()).then_with(|| a.id.encode().cmp(&b.id.encode())));
    Scan { entries, problems }
}

/// Is this image Blink itself (its own exe, or any `blink.exe`)?
pub fn is_blink_image(path: &str) -> bool {
    let p = path.trim().trim_matches('"');
    if p.is_empty() {
        return false;
    }
    if let Ok(me) = std::env::current_exe() {
        if crate::update::logic::norm_path(&me.to_string_lossy()) == crate::update::logic::norm_path(p) {
            return true;
        }
    }
    crate::osutil::file_name(p).eq_ignore_ascii_case("blink.exe")
}

/// Read a `.lnk` file's stored target (no search, no UI). `None` when the file
/// is not a shortcut Windows can read.
pub fn resolve_link(lnk: &str) -> Option<Target> {
    use windows::Win32::System::Com::*;
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok();
        let r = (|| {
            let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
            let pf: IPersistFile = windows::core::Interface::cast(&sl).ok()?;
            let w = wide(lnk);
            pf.Load(pcw(&w), STGM_READ).ok()?;
            let mut path = [0u16; 520];
            let mut args = [0u16; 2048];
            let mut dir = [0u16; 520];
            let mut icon = [0u16; 520];
            let mut idx = 0i32;
            // SLGP_RAWPATH (4): the path as stored, with %VARS% unexpanded
            let _ = sl.GetPath(&mut path, std::ptr::null_mut(), 4);
            let _ = sl.GetArguments(&mut args);
            let _ = sl.GetWorkingDirectory(&mut dir);
            let _ = sl.GetIconLocation(&mut icon, &mut idx);
            let icon = from_wide(&icon);
            Some(Target { path: expand_env(&from_wide(&path)), args: from_wide(&args), dir: expand_env(&from_wide(&dir)), icon: if idx == 0 { expand_env(&icon) } else { String::new() } })
        })();
        if init {
            CoUninitialize();
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::reg::scratch::Scratch;

    fn set_text(root: HKEY, path: &str, name: &str, ty: REG_VALUE_TYPE, text: &str) {
        let k = reg::create(root, path, KEY_READ | KEY_WRITE | KEY_WOW64_64KEY).unwrap();
        let v: Vec<u8> = wide(text).iter().flat_map(|c| c.to_le_bytes()).collect();
        let n = wide(name);
        unsafe {
            assert_eq!(RegSetValueExW(k.0, pcw(&n), None, ty, Some(&v)), windows::Win32::Foundation::ERROR_SUCCESS);
        }
    }

    #[test]
    fn expands_known_vars_and_leaves_the_rest() {
        std::env::set_var("BLINK_FIXTURE_VAR", "C:\\Fx");
        assert_eq!(expand_env("%BLINK_FIXTURE_VAR%\\a.exe"), "C:\\Fx\\a.exe");
        assert_eq!(expand_env("%NOPE_NOT_SET_1%\\a"), "%NOPE_NOT_SET_1%\\a");
        assert_eq!(expand_env("100% sure %"), "100% sure %");
        assert_eq!(expand_env("%%"), "%%");
    }

    #[test]
    fn scans_run_values_in_each_source_with_identity_and_state() {
        let s = Scratch::new("scan");
        let tmp = std::env::temp_dir().join(format!("BlinkStartupFixture-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("user")).unwrap();
        std::fs::create_dir_all(tmp.join("common")).unwrap();
        let loc = Locations { hkcu: s.user.0, hklm: s.machine.0, user_dir: tmp.join("user"), common_dir: tmp.join("common") };
        set_text(loc.hkcu, approved::RUN, "BlinkStartupFixture A", REG_SZ, r"C:\nope\a.exe --x");
        set_text(loc.hklm, approved::RUN, "BlinkStartupFixture A", REG_SZ, r"C:\nope\machine.exe");
        set_text(loc.hklm, approved::RUN32, "BlinkStartupFixture A", REG_EXPAND_SZ, r"%BLINK_SCAN_FX%\w.exe");
        std::env::set_var("BLINK_SCAN_FX", "C:\\Expanded");
        std::fs::write(tmp.join("user").join("Folder One.lnk"), b"not a real shortcut").unwrap();
        std::fs::write(tmp.join("user").join("desktop.ini"), b"x").unwrap();
        std::fs::write(tmp.join("common").join("Common One.bat"), b"@echo off").unwrap();
        let id_disabled = EntryId::new(Kind::Run, Scope::User, View::Native, "BlinkStartupFixture A");
        approved::write(&loc, &id_disabled, Some(&approved::disabled_bytes(approved::filetime_now()))).unwrap();

        let sc = scan(&loc);
        let by =|kind, scope, view, key: &str| sc.entries.iter().find(|e| e.id == EntryId::new(kind, scope, view, key)).unwrap_or_else(|| panic!("missing {key}: {:?}", sc.entries.iter().map(|e| e.id.encode()).collect::<Vec<_>>()));
        // same name in three sources = three separate entries, never merged
        let a_user = by(Kind::Run, Scope::User, View::Native, "BlinkStartupFixture A");
        let a_all = by(Kind::Run, Scope::AllUsers, View::Native, "BlinkStartupFixture A");
        let a_32 = by(Kind::Run, Scope::AllUsers, View::Wow32, "BlinkStartupFixture A");
        assert_eq!(a_user.command, r"C:\nope\a.exe --x");
        assert_eq!(a_user.state(), approved::State::Disabled);
        assert_eq!(a_all.command, r"C:\nope\machine.exe");
        assert_eq!(a_32.raw, r"%BLINK_SCAN_FX%\w.exe", "the stored value is kept unexpanded");
        assert_eq!(a_32.command, r"C:\Expanded\w.exe");
        // the user-scope StartupApproved value is the per-user override of the all-users entry too
        assert_eq!(a_all.live.user_override, a_user.live.approved);
        assert_eq!(a_all.state(), approved::State::Unknown, "two approval layers that disagree have no authoritative winner");
        // folders: desktop.ini is not an entry; a non-shortcut file is listed by name
        let f1 = by(Kind::Folder, Scope::User, View::Native, "Folder One.lnk");
        assert_eq!(f1.display(), "Folder One");
        assert!(f1.target.is_none(), "an unreadable shortcut has no target, not a guess");
        assert!(sc.entries.iter().all(|e| !e.id.key.eq_ignore_ascii_case("desktop.ini")));
        by(Kind::Folder, Scope::AllUsers, View::Native, "Common One.bat");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn blink_itself_is_recognised() {
        assert!(is_blink_image(r"C:\Users\x\AppData\Local\Programs\Blink\blink.exe"));
        assert!(is_blink_image(r#""D:\tools\Blink.EXE""#));
        assert!(!is_blink_image(r"C:\Program Files\Other\other.exe"));
        assert!(!is_blink_image(""));
    }
}
