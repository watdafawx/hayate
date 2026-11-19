//! Opt-in "open Hayate instead of Task Manager" (docs/task-manager-replacement.md).
//!
//! * Settings toggle -> UAC -> `hayate.exe --taskmgr-enable` (elevated helper):
//!   copies itself to `%ProgramFiles%\Hayate\hayate-tm.exe`, checks that nobody
//!   but admins can change that file or anything above it, writes
//!   `HKLM\...\Image File Execution Options\taskmgr.exe\Debugger = "<launcher>"`,
//!   runs a headless probe, and only then records `Enabled=1`. Any failure puts
//!   the registry back exactly.
//! * The launcher is `hayate.exe` started under the name `hayate-tm.exe`
//!   (`launcher()`, the first thing `main` does): it messages a Hayate that is
//!   already running; with none running it **cold-starts Hayate from its own
//!   protected image** (`RealLaunch::start_hayate`: file locked against swaps, ACL
//!   chain re-verified, clean command line, environment rebuilt from known folders,
//!   3 s for the window to appear). On Shift, a hung Hayate, an untrusted launcher or
//!   any failure it starts the real Task Manager with `DEBUG_PROCESS`, which makes
//!   `CreateProcess` skip IFEO. It never executes any path but its own, never writes
//!   anything, never trusts its arguments or environment. The cold-started Hayate is a
//!   `hayate-tm.exe` whose parent is the same image (`cold_child_here`) and runs as the app.
//! * All decisions are in `logic` and tested with an in-memory registry. The
//!   real registry (`RealRegistry`) is only constructed by the helper, the
//!   launcher and the Settings status read; under `cfg(test)` it panics.

pub mod logic;

use logic::*;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::osutil::{self, pcw, wide};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::*;
use windows::Win32::Security::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Diagnostics::Debug::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---- the real registry (HKLM, 64-bit view) -------------------------------------

/// Diagnostic log line. Elevated paths (helper, launcher install) never write to the
/// user-writable %LOCALAPPDATA% hayate.log: that path could be a link or swapped by any
/// process of the user, and an elevated writer would follow it. The helper reports by exit code.
fn log(line: &str) {
    if log_allowed(osutil::is_elevated()) {
        osutil::log(line);
    }
}

pub struct RealRegistry;

impl RealRegistry {
    pub fn new() -> RealRegistry {
        #[cfg(test)]
        panic!("tests must never open the real registry: use an injected Registry");
        #[cfg(not(test))]
        RealRegistry
    }

    fn open(&self, key: &str, write: bool) -> Option<HKEY> {
        let k = wide(key);
        let mut h = HKEY::default();
        let sam = KEY_WOW64_64KEY | if write { KEY_READ | KEY_WRITE } else { KEY_READ };
        unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, pcw(&k), None, sam, &mut h) }.is_ok().then_some(h)
    }

    fn create(&self, key: &str) -> Result<HKEY, String> {
        let k = wide(key);
        let mut h = HKEY::default();
        let r = unsafe { RegCreateKeyExW(HKEY_LOCAL_MACHINE, pcw(&k), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WOW64_64KEY | KEY_READ | KEY_WRITE, None, &mut h, None) };
        if r.is_ok() {
            Ok(h)
        } else {
            Err(format!(r"Cannot open HKLM\{key} for writing: {}", r.to_hresult().message()))
        }
    }
}

impl Registry for RealRegistry {
    fn get(&self, key: &str, name: &str) -> Result<Option<String>, String> {
        let Some(h) = self.open(key, false) else { return Ok(None) };
        let n = wide(name);
        let mut ty = REG_VALUE_TYPE(0);
        let mut len = 0u32;
        let r = unsafe { RegQueryValueExW(h, pcw(&n), None, Some(&mut ty), None, Some(&mut len)) };
        if !r.is_ok() {
            unsafe { let _ = RegCloseKey(h); }
            return Ok(None);
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
        let mut len2 = (buf.len() * 2) as u32;
        let r = unsafe { RegQueryValueExW(h, pcw(&n), None, Some(&mut ty), Some(buf.as_mut_ptr() as *mut u8), Some(&mut len2)) };
        unsafe { let _ = RegCloseKey(h); }
        if !r.is_ok() || ty != REG_SZ {
            return Ok(None);
        }
        let n = (len2 as usize / 2).min(buf.len());
        Ok(Some(osutil::from_wide(&buf[..n])))
    }

    fn set(&mut self, key: &str, name: &str, value: &str) -> Result<(), String> {
        let h = self.create(key)?;
        let (n, v) = (wide(name), wide(value));
        let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2) };
        let r = unsafe { RegSetValueExW(h, pcw(&n), None, REG_SZ, Some(bytes)) };
        unsafe { let _ = RegCloseKey(h); }
        if r.is_ok() {
            Ok(())
        } else {
            Err(format!(r"Cannot write HKLM\{key} {name}: {}", r.to_hresult().message()))
        }
    }

    fn delete_value(&mut self, key: &str, name: &str) -> Result<(), String> {
        let Some(h) = self.open(key, true) else { return Ok(()) };
        let n = wide(name);
        let r = unsafe { RegDeleteValueW(h, pcw(&n)) };
        unsafe { let _ = RegCloseKey(h); }
        if r.is_ok() || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!(r"Cannot delete HKLM\{key} {name}: {}", r.to_hresult().message()))
        }
    }

    fn delete_key(&mut self, key: &str) -> Result<(), String> {
        let (parent, leaf) = key.rsplit_once('\\').ok_or("bad key")?;
        let Some(p) = self.open(parent, true) else { return Ok(()) };
        let l = wide(leaf);
        let mut sub = HKEY::default();
        let ok = unsafe { RegOpenKeyExW(p, pcw(&l), None, KEY_WOW64_64KEY | KEY_READ | KEY_WRITE, &mut sub) }.is_ok();
        let mut res = Ok(());
        if ok {
            let del = unsafe { RegDeleteTreeW(sub, PCWSTR::null()) };
            unsafe { let _ = RegCloseKey(sub); }
            if !del.is_ok() {
                res = Err(format!(r"Cannot delete HKLM\{key}: {}", del.to_hresult().message()));
            }
            let d2 = unsafe { RegDeleteKeyExW(p, pcw(&l), KEY_WOW64_64KEY.0, None) };
            if res.is_ok() && !d2.is_ok() && d2 != ERROR_FILE_NOT_FOUND {
                res = Err(format!(r"Cannot delete HKLM\{key}: {}", d2.to_hresult().message()));
            }
        }
        unsafe { let _ = RegCloseKey(p); }
        res
    }

    fn is_empty(&self, key: &str) -> bool {
        let Some(h) = self.open(key, false) else { return false };
        let (mut subs, mut vals) = (0u32, 0u32);
        let r = unsafe { RegQueryInfoKeyW(h, None, None, None, Some(&mut subs), None, None, Some(&mut vals), None, None, None, None) };
        unsafe { let _ = RegCloseKey(h); }
        r.is_ok() && subs == 0 && vals == 0
    }

    fn subkeys(&self, key: &str) -> Vec<String> {
        let Some(h) = self.open(key, false) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0.. {
            let mut buf = [0u16; 260];
            let mut n = buf.len() as u32;
            if unsafe { RegEnumKeyExW(h, i, Some(PWSTR(buf.as_mut_ptr())), &mut n, None, Some(PWSTR::null()), None, None) }.is_err() {
                break;
            }
            out.push(osutil::from_wide(&buf[..n as usize]));
        }
        unsafe { let _ = RegCloseKey(h); }
        out
    }
}

// ---- the real machine ------------------------------------------------------------

/// The installed hayate.exe (or its folder) below Program Files must be admin-only and free of links
/// (same checks as the launcher). Used after an all-users install copied it.
pub fn verify_admin_only(path: &str, require_file: bool) -> std::result::Result<(), String> {
    let pf = program_files();
    if pf.is_empty() || !crate::update::logic::inside_or_same(path, &pf) {
        return Err(format!("{path} is not below Program Files"));
    }
    RealHost::chain_any(path, require_file, &pf)
}

fn program_files() -> String {
    crate::setup::program_files().unwrap_or_default()
}

fn sysroot() -> String {
    let mut b = [0u16; 260];
    let n = unsafe { windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW(Some(&mut b)) } as usize;
    osutil::from_wide(&b[..n.min(b.len())])
}

pub struct RealHost;

impl RealHost {
    fn sid_string(sid: PSID) -> Option<String> {
        let mut s = PWSTR::null();
        unsafe { ConvertSidToStringSidW(sid, &mut s) }.ok()?;
        let r = unsafe { s.to_string().ok() };
        unsafe { LocalFree(Some(HLOCAL(s.0 as *mut _))) };
        r
    }

    /// owner + allow list of one path, as the policy in `logic` wants it
    fn security(path: &str) -> std::result::Result<(String, Option<Vec<Ace>>), String> {
        let w = wide(path);
        let (mut owner, mut dacl, mut sd) = (PSID::default(), std::ptr::null_mut::<ACL>(), PSECURITY_DESCRIPTOR::default());
        let r = unsafe { GetNamedSecurityInfoW(pcw(&w), SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION, Some(&mut owner), None, Some(&mut dacl), None, &mut sd) };
        if !r.is_ok() {
            return Err(format!("cannot read the access list of {path}: {}", r.to_hresult().message()));
        }
        let res = Self::owner_and_aces(owner, dacl, path);
        unsafe { LocalFree(Some(HLOCAL(sd.0))) };
        res
    }

    /// Owner and access list of an OPEN file (needs READ_CONTROL): what the handle really is,
    /// not what the path resolves to a moment later.
    fn security_of_handle(h: HANDLE, what: &str) -> std::result::Result<(String, Option<Vec<Ace>>), String> {
        let (mut owner, mut dacl, mut sd) = (PSID::default(), std::ptr::null_mut::<ACL>(), PSECURITY_DESCRIPTOR::default());
        let r = unsafe { GetSecurityInfo(h, SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION, Some(&mut owner), None, Some(&mut dacl), None, Some(&mut sd)) };
        if !r.is_ok() {
            return Err(format!("cannot read the access list of {what}: {}", r.to_hresult().message()));
        }
        let res = Self::owner_and_aces(owner, dacl, what);
        unsafe { LocalFree(Some(HLOCAL(sd.0))) };
        res
    }

    fn owner_and_aces(owner: PSID, dacl: *mut ACL, path: &str) -> std::result::Result<(String, Option<Vec<Ace>>), String> {
        let o = Self::sid_string(owner).ok_or_else(|| format!("cannot read the owner of {path}"))?;
        if dacl.is_null() {
            return Ok((o, None));
        }
        Ok((o, Some(Self::aces(dacl, path)?)))
    }

    /// the allow/deny list of a DACL as the policy in `logic` wants it
    fn aces(dacl: *mut ACL, path: &str) -> std::result::Result<Vec<Ace>, String> {
        let mut info = ACL_SIZE_INFORMATION::default();
        unsafe { GetAclInformation(dacl, &mut info as *mut _ as *mut _, std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32, AclSizeInformation) }.map_err(|e| e.message())?;
        let mut aces = Vec::new();
        for i in 0..info.AceCount {
            let mut p = std::ptr::null_mut::<core::ffi::c_void>();
            unsafe { GetAce(dacl, i, &mut p) }.map_err(|e| e.message())?;
            let hdr = unsafe { &*(p as *const ACE_HEADER) };
            let allow = match hdr.AceType {
                0 => true,
                1 => false,
                t => return Err(format!("{path} has an access entry of type {t} that Hayate does not check")),
            };
            let ace = unsafe { &*(p as *const ACCESS_ALLOWED_ACE) };
            let sid = Self::sid_string(PSID(&ace.SidStart as *const u32 as *mut _)).ok_or("cannot read an access entry")?;
            aces.push(Ace { sid, mask: ace.Mask, allow, inherit_only: hdr.AceFlags & 0x08 != 0 });
        }
        Ok(aces)
    }

    /// HKLM key (64-bit view) must be admin-owned with no write access for anyone else.
    /// A missing key is fine (nothing to trust yet; it is created under admin-only HKLM\SOFTWARE).
    fn key_admin_only(key: &str) -> std::result::Result<(), String> {
        let k = wide(key);
        let mut h = HKEY::default();
        let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, pcw(&k), None, KEY_WOW64_64KEY | KEY_READ, &mut h) };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if !r.is_ok() {
            return Err(format!(r"cannot open HKLM\{key}: {}", r.to_hresult().message()));
        }
        let info = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut n = 0u32;
        let _ = unsafe { RegGetKeySecurity(h, info, None, &mut n) };
        let mut buf = vec![0u64; (n as usize).div_ceil(8).max(1)];
        let sd = PSECURITY_DESCRIPTOR(buf.as_mut_ptr() as *mut _);
        let g = unsafe { RegGetKeySecurity(h, info, Some(sd), &mut n) };
        unsafe { let _ = RegCloseKey(h); }
        if g != ERROR_SUCCESS {
            return Err(format!(r"cannot read the permissions of HKLM\{key} (Windows error {})", g.0));
        }
        let (mut owner, mut dflt) = (PSID::default(), windows::core::BOOL(0));
        unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut dflt) }.map_err(|e| e.message())?;
        let o = Self::sid_string(owner).ok_or_else(|| format!(r"cannot read the owner of HKLM\{key}"))?;
        let (mut present, mut acl) = (windows::core::BOOL(0), std::ptr::null_mut::<ACL>());
        unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut dflt) }.map_err(|e| e.message())?;
        let aces = if present.as_bool() && !acl.is_null() { Some(Self::aces(acl, key)?) } else { None };
        // registry write bits share the file bits the policy checks (SET_VALUE 0x2, CREATE_SUB_KEY 0x4)
        match acl_problem(Level::Dir, &o, aces.as_deref()) {
            Some(why) => Err(format!(r"HKLM\{key}: {why}")),
            None => Ok(()),
        }
    }

    /// The launcher path is the expected one, and (when `require_file`) the file is
    /// there; every part of the path that exists is admin-only and no link. Without
    /// `require_file` (before the copy) missing parts are fine: they do not exist yet.
    fn chain(path: &str, require_file: bool) -> std::result::Result<(), String> {
        let pf = program_files();
        if expected_launcher(&pf).is_none_or(|e| norm(&e) != norm(path)) {
            return Err(format!("{path} is not %ProgramFiles%\\Hayate\\{LAUNCHER_NAME}"));
        }
        Self::chain_any(path, require_file, &pf)
    }

    /// The same walk for any file below Program Files (the all-users hayate.exe).
    fn chain_any(path: &str, require_file: bool, pf: &str) -> std::result::Result<(), String> {
        let pf = pf.to_string();
        if require_file && !Path::new(path).is_file() {
            return Err(format!("{path} is missing"));
        }
        let mut cur = Some(Path::new(path));
        while let Some(p) = cur {
            let s = p.to_string_lossy().into_owned();
            if s.is_empty() {
                break;
            }
            let w = wide(&s);
            let attrs = unsafe { GetFileAttributesW(pcw(&w)) };
            if attrs == INVALID_FILE_ATTRIBUTES {
                if !require_file && !p.exists() {
                    cur = p.parent();
                    continue;
                }
                return Err(format!("cannot read {s}"));
            }
            if attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                return Err(format!("{s} is a link or junction"));
            }
            let (owner, aces) = Self::security(&s)?;
            if let Some(why) = acl_problem(level_for(&s, path, &pf), &owner, aces.as_deref()) {
                return Err(format!("{s}: {why}"));
            }
            cur = p.parent();
        }
        Ok(())
    }
}

impl Host for RealHost {
    fn launcher_trusted(&self, path: &str) -> std::result::Result<(), String> {
        Self::chain(path, true)
    }

    fn precheck(&self, path: &str) -> std::result::Result<(), String> {
        Self::chain(path, false)
    }

    fn state_trusted(&self) -> std::result::Result<(), String> {
        Self::key_admin_only(r"SOFTWARE\Hayate")?;
        Self::key_admin_only(STATE_KEY)
    }

    fn install_launcher(&mut self, path: &str) -> std::result::Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let dir = Path::new(path).parent().ok_or("bad launcher path")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
        let new = unique_name(dir, "");
        std::fs::copy(&exe, &new).map_err(|e| {
            let _ = std::fs::remove_file(&new);
            format!("Cannot copy the launcher to {}: {e}", new.display())
        })?;
        swap_files(&mut RealFs, path, &new.to_string_lossy(), &old_path(path))?;
        log(&format!("taskmgr: launcher {} installed at {path}", exe.display()));
        Ok(())
    }

    fn launcher_exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }

    fn remove_launcher(&mut self, path: &str) -> bool {
        let old = old_path(path);
        if let Some(d) = Path::new(path).parent() {
            // staged copies (unique names) a cut-short run left behind
            for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                let n = e.file_name().to_string_lossy().to_ascii_lowercase();
                if n.starts_with("hayate-tm.new") && n.ends_with(".exe") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::remove_file(&old);
        let mut how = "removed";
        if std::fs::remove_file(path).is_err() && Path::new(path).exists() {
            // running (a Task Manager request that cold-started Hayate): a running image cannot be
            // deleted but can be renamed inside its own admin-only folder; the renamed file is
            // deleted at the next reboot (best effort) and removed with the folder on uninstall
            how = if std::fs::rename(path, &old).is_ok() {
                let o = wide(&old);
                let _ = unsafe { MoveFileExW(pcw(&o), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
                "renamed to hayate-tm.old.exe (it is running), deleted at reboot"
            } else {
                "kept (it is running), removed on the next switch-off or uninstall"
            };
        }
        if let Some(d) = Path::new(path).parent() {
            // the cold-start lock (best effort: a launcher mid cold start still holds it), then the
            // folder, only when empty (a machine install keeps its folder)
            let _ = std::fs::remove_file(d.join(".coldstart.lock"));
            let _ = std::fs::remove_dir(d);
        }
        let gone = !Path::new(path).exists();
        log(&format!("taskmgr: launcher {path} {how}"));
        gone
    }

    fn probe(&mut self, target: &str, native: Option<&str>) -> std::result::Result<(), String> {
        let image = if is_real(target) { format!(r"{}\System32\{REAL_TARGET}", sysroot()) } else { native.unwrap_or_default().to_string() };
        if !Path::new(&image).is_file() {
            return Err(format!("{image} not found"));
        }
        let nonce = format!("{:x}", osutil::now_ms());
        let mut cmd = wide(&format!("\"{image}\" {SELF_TEST_ARG} {nonce}"));
        let app = wide(&image);
        let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
        let mut pi = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(pcw(&app), Some(PWSTR(cmd.as_mut_ptr())), None, None, false, CREATE_NO_WINDOW, None, None, &si, &mut pi).map_err(|e| format!("cannot start {image}: {e}"))?;
            let _ = CloseHandle(pi.hThread);
            let w = WaitForSingleObject(pi.hProcess, 15_000);
            let mut code = 0u32;
            let res = if w == WAIT_TIMEOUT {
                // nothing answered: the real image is running instead of the launcher
                let _ = TerminateProcess(pi.hProcess, 1);
                // the file stays locked until the process is really gone: wait for it before any swap or restore
                let _ = WaitForSingleObject(pi.hProcess, 5_000);
                Err("the image started instead of the launcher: Image File Execution Options did not apply".to_string())
            } else if GetExitCodeProcess(pi.hProcess, &mut code).is_ok() && code as i32 == EXIT_SELF_TEST_OK {
                Ok(())
            } else {
                Err(format!("the launcher answered with code {code}: {}", exit_message(code as i32)))
            };
            let _ = CloseHandle(pi.hProcess);
            res
        }
    }

    fn lock_cold_start(&mut self) -> std::result::Result<(), String> {
        match ColdLock::acquire() {
            Some(l) if l.held => {
                SWAP_LOCK.with(|c| *c.borrow_mut() = Some(l));
                Ok(())
            }
            Some(_) => Err("another Task Manager start holds the cold-start lock".into()),
            None => Err("the cold-start lock cannot be taken".into()),
        }
    }

    fn unlock_cold_start(&mut self) {
        SWAP_LOCK.with(|c| *c.borrow_mut() = None);
    }

    fn source_sha(&self) -> std::result::Result<String, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        crate::update::sha256_file(&exe)
    }

    fn launcher_sha(&self, path: &str) -> std::result::Result<String, String> {
        crate::update::sha256_file(Path::new(path))
    }

    fn old_exists(&self, path: &str) -> bool {
        Path::new(&old_path(path)).is_file()
    }

    fn source_ordinal(&self) -> u64 {
        build_ordinal()
    }

    fn stage_launcher(&mut self, path: &str, sha: &str) -> std::result::Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let dir = Path::new(path).parent().ok_or("bad launcher path")?;
        // a name nobody else uses: the swap only ever installs the file this run staged
        let new = unique_name(dir, "");
        let drop_new = |e: String| {
            let _ = std::fs::remove_file(&new);
            e
        };
        std::fs::copy(&exe, &new).map_err(|e| drop_new(format!("Cannot copy the launcher to {}: {e}", new.display())))?;
        // the copy must be the image the hash was taken of (the source may have changed on disk meanwhile)
        match crate::update::sha256_file(&new) {
            Ok(h) if h.eq_ignore_ascii_case(sha) => {
                STAGED.with(|c| *c.borrow_mut() = Some(new.clone()));
                Ok(())
            }
            Ok(_) => Err(drop_new("the copy does not match its source (sha256); the source changed while it was copied".into())),
            Err(e) => Err(drop_new(e)),
        }
    }

    fn swap_launcher(&mut self, path: &str) -> std::result::Result<(), String> {
        let new = STAGED.with(|c| c.borrow_mut().take()).ok_or("no launcher was staged by this run")?;
        swap_files(&mut RealFs, path, &new.to_string_lossy(), &old_path(path))?;
        log(&format!("taskmgr: launcher at {path} swapped, previous kept as hayate-tm.old.exe"));
        Ok(())
    }

    fn restore_launcher(&mut self, path: &str) -> std::result::Result<(), String> {
        let dir = Path::new(path).parent().ok_or("bad launcher path")?;
        let new = unique_name(dir, "r");
        restore_files(&mut RealFs, path, &new.to_string_lossy(), &old_path(path))?;
        log(&format!("taskmgr: previous launcher put back at {path}"));
        Ok(())
    }
}

/// `hayate-tm.old.exe` beside the launcher
fn old_path(launcher: &str) -> String {
    launcher.replace(LAUNCHER_NAME, "hayate-tm.old.exe")
}

/// `hayate-tm.new.<tag><pid>-<time>.exe` beside the launcher: staged copies never share a name
fn unique_name(dir: &Path, tag: &str) -> std::path::PathBuf {
    dir.join(format!("hayate-tm.new.{tag}{}-{:x}.exe", std::process::id(), osutil::now_ms()))
}

/// Build ordinal of this exe (seconds since 1970 at build time; `HAYATE_BUILD_ORDINAL` overrides it for
/// tests). A larger number is a newer build: a launcher is only ever refreshed forward.
pub fn build_ordinal() -> u64 {
    env!("HAYATE_BUILD").parse().unwrap_or(0)
}

/// The real file operations of a launcher swap (`logic::swap_files` / `restore_files`).
struct RealFs;

impl RealFs {
    fn mv(from: &str, to: &str, flags: MOVE_FILE_FLAGS) -> std::result::Result<(), String> {
        let (f, t) = (wide(from), wide(to));
        unsafe { MoveFileExW(pcw(&f), pcw(&t), flags) }.map_err(|e| e.message())
    }
}

impl Fs for RealFs {
    fn exists(&self, p: &str) -> bool {
        Path::new(p).exists()
    }
    fn copy(&mut self, from: &str, to: &str) -> std::result::Result<(), String> {
        if Path::new(to).exists() {
            return Err(format!("{to} exists"));
        }
        std::fs::copy(from, to).map(|_| ()).map_err(|e| e.to_string())
    }
    fn replace(&mut self, from: &str, to: &str) -> std::result::Result<(), String> {
        Self::mv(from, to, MOVEFILE_REPLACE_EXISTING)
    }
    fn rename(&mut self, from: &str, to: &str) -> std::result::Result<(), String> {
        if Path::new(to).exists() {
            return Err(format!("{to} exists"));
        }
        Self::mv(from, to, MOVE_FILE_FLAGS(0))
    }
    fn remove(&mut self, p: &str) -> std::result::Result<(), String> {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
    fn protect(&mut self, p: &str) {
        // SYSTEM and Administrators only, no inherited Users read (LOCK_SDDL)
        let sddl = wide(LOCK_SDDL);
        let mut psd = PSECURITY_DESCRIPTOR::default();
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(pcw(&sddl), SDDL_REVISION_1, &mut psd, None) }.is_err() {
            return;
        }
        let (mut present, mut acl, mut dflt) = (windows::core::BOOL(0), std::ptr::null_mut::<ACL>(), windows::core::BOOL(0));
        if unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut acl, &mut dflt) }.is_ok() && present.as_bool() && !acl.is_null() {
            let w = wide(p);
            let _ = unsafe { SetNamedSecurityInfoW(pcw(&w), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, None, None, Some(acl as *const ACL), None) };
        }
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
    }
    fn delete_at_reboot(&mut self, p: &str) {
        let w = wide(p);
        let _ = unsafe { MoveFileExW(pcw(&w), PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
    }
    fn pause(&mut self) {
        std::thread::sleep(Duration::from_millis(300));
    }
}

thread_local! {
    /// The staged copy this run made (one helper thread): the swap installs this file and no other.
    static STAGED: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

thread_local! {
    /// The cold-start lock the refresh holds while it swaps the launcher file (one helper thread).
    static SWAP_LOCK: std::cell::RefCell<Option<ColdLock>> = const { std::cell::RefCell::new(None) };
}

// ---- the launcher (hayate.exe started as hayate-tm.exe) -------------------------------

fn key_down(vk: VIRTUAL_KEY) -> bool {
    (unsafe { GetAsyncKeyState(vk.0 as i32) } as u16) & 0x8000 != 0
}

/// First thing in `main`: when this exe was started as `hayate-tm.exe` it is the
/// IFEO launcher, runs to its end and exits. Returns false for a normal Hayate, and for
/// the Hayate this launcher cold-started from its own image (`cold_child_here`).
pub fn launcher() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    if !exe.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(LAUNCHER_NAME)) {
        return false;
    }
    let exe = exe.to_string_lossy().into_owned();
    if cold_child_here(&exe) {
        // elevated, and its data dir is the medium user's: it writes no hayate.log
        // (`osutil::data_dir` also refuses a data dir behind a link); docs §7.3a "Data dir"
        osutil::mark_cold_started();
        return false;
    }
    let code = run_launcher(&exe);
    std::process::exit(code);
}

fn parent_pid() -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let me = std::process::id();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        let mut more = Process32FirstW(snap, &mut e).is_ok();
        while more {
            if e.th32ProcessID == me {
                found = Some(e.th32ParentProcessID);
                break;
            }
            more = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// This process is the Hayate the launcher cold-started: elevated, its parent is this same
/// image, and the file and every folder above it are still admin-only. Argv and the
/// environment are not looked at (they are the caller's to write).
fn cold_child_here(exe: &str) -> bool {
    if !osutil::is_elevated() {
        return false;
    }
    let parent = parent_pid().and_then(osutil::image_path);
    cold_child(parent.as_deref(), exe) && RealHost.launcher_trusted(exe).is_ok()
}

const COLD_LOCK_MS: u32 = 3000;

/// One cold start at a time (a second Task Manager request while the first Hayate is still
/// starting waits, then finds its window). The lock is an exclusive (share mode 0) file inside
/// the admin-only `%ProgramFiles%\Hayate`, created with its own protected DACL (SYSTEM and
/// Administrators only, `LOCK_SDDL`): the folder's inherited Users read entry is NOT inherited, so
/// a medium user cannot open it, even read-only, and so cannot hold it (a named mutex in `Global\`
/// can be squatted). An existing file with any other allow entry is deleted and recreated, or, if
/// it cannot be deleted, there is no lock and the launcher goes native. "unusable" (no lock at
/// all) is told apart from "held by another start" (wait up to `COLD_LOCK_MS`, then give up).
struct ColdLock {
    h: HANDLE,
    /// we hold the exclusive handle; false = another start held it for the whole wait: never start a Hayate
    held: bool,
}

impl ColdLock {
    fn acquire() -> Option<ColdLock> {
        // no Program Files folder = no lock (the launcher goes native)
        let path_s = lock_path().ok()?;
        // an existing file whose access list lets a non-admin open it (inherited Users entry, a
        // planted file) is deleted and created again; when that fails (someone holds it) there is no lock
        if Path::new(&path_s).exists() {
            let ours = RealHost::security(&path_s).is_ok_and(|(o, a)| lock_file_policy(&o, a.as_deref()) == LockFile::Use);
            if !ours && std::fs::remove_file(&path_s).is_err() {
                return None;
            }
        }
        let path = wide(&path_s);
        let sddl = wide(LOCK_SDDL);
        let mut psd = PSECURITY_DESCRIPTOR::default();
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(pcw(&sddl), SDDL_REVISION_1, &mut psd, None) }.is_err() {
            return None;
        }
        let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd.0, bInheritHandle: windows::core::BOOL(0) };
        let t0 = Instant::now();
        let out = loop {
            let r = unsafe { CreateFileW(pcw(&path), GENERIC_WRITE.0 | READ_CONTROL.0, FILE_SHARE_MODE(0), Some(&sa), OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, None) };
            let err = match &r {
                Ok(_) => 0,
                Err(e) => (e.code().0 as u32) & 0xFFFF,
            };
            match lock_try(err) {
                LockTry::Got => break r.ok().map(|h| ColdLock { h, held: true }),
                LockTry::Unusable => break None,
                LockTry::Busy => {
                    if t0.elapsed() >= Duration::from_millis(COLD_LOCK_MS as u64) {
                        break Some(ColdLock { h: INVALID_HANDLE_VALUE, held: false });
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        };
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
        // belt and braces: what we hold must really be admin-only now. Read from the OPEN handle
        // (not the path again): share mode 0 pins the file, and the folder is admin-only, so
        // nobody who is not an admin can swap the path between our create and this check anyway,
        // and the handle is what we actually hold.
        match out {
            Some(l) if l.held && !RealHost::security_of_handle(l.h, "the cold-start lock").is_ok_and(|(o, a)| lock_file_policy(&o, a.as_deref()) == LockFile::Use) => None,
            o => o,
        }
    }
}

/// One machine-wide exclusion for EVERY mutating helper step (enable, disable, refresh/repair, and the
/// auto refresh's bookkeeping): a named mutex whose DACL is SYSTEM + Administrators only (`LOCK_SDDL`).
/// A medium user cannot take part in it; one who created the name first can only make a step time out
/// ("busy"), never see or change what it protects. Taken before any state is read.
struct OpLock(HANDLE);

impl OpLock {
    fn take() -> std::result::Result<OpLock, String> {
        let sddl = wide(LOCK_SDDL);
        let mut psd = PSECURITY_DESCRIPTOR::default();
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(pcw(&sddl), SDDL_REVISION_1, &mut psd, None) }.map_err(|e| e.message())?;
        let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: psd.0, bInheritHandle: windows::core::BOOL(0) };
        let h = unsafe { CreateMutexW(Some(&sa), false, w!("Global\\HayateTaskMgrHelper")) };
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
        let h = h.map_err(|e| format!("the Task Manager step lock cannot be opened ({})", e.message()))?;
        let w = unsafe { WaitForSingleObject(h, 30_000) };
        if w == WAIT_OBJECT_0 || w == WAIT_ABANDONED {
            Ok(OpLock(h))
        } else {
            unsafe {
                let _ = CloseHandle(h);
            }
            Err("another Task Manager step is still running".into())
        }
    }
}

impl Drop for OpLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

fn lock_path() -> Result<String, String> {
    let pf = crate::setup::program_files().ok_or("cannot read the Program Files folder")?;
    Ok(format!("{pf}\\Hayate\\.coldstart.lock"))
}

impl Drop for ColdLock {
    fn drop(&mut self) {
        if self.held {
            unsafe {
                let _ = CloseHandle(self.h);
            }
        }
    }
}

/// Name of the desktop this thread runs on ("Default" = interactive, "Winlogon" = secure).
fn desktop_name() -> Option<String> {
    use windows::Win32::System::StationsAndDesktops::*;
    unsafe {
        let d = GetThreadDesktop(GetCurrentThreadId()).ok()?;
        let mut buf = [0u16; 128];
        let mut need = 0u32;
        GetUserObjectInformationW(HANDLE(d.0), UOI_NAME, Some(buf.as_mut_ptr().cast()), (buf.len() * 2) as u32, Some(&mut need)).ok()?;
        Some(osutil::from_wide(&buf))
    }
}

fn run_launcher(exe: &str) -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut target = target_from_args(&args);
    if !staging_allowed() {
        target = REAL_TARGET.to_string(); // staging targets are a debug-build feature
    }
    let reg = RealRegistry::new();
    let state = read_state(&reg, &target);
    let c = LaunchCtx { args, shift: key_down(VK_SHIFT), ctrl: key_down(VK_CONTROL), guard_set: crate::osutil::env_var_os(GUARD_ENV).is_some() };
    // the launcher is meant to run elevated (substitute of an auto-elevated image): an unelevated
    // one trusts nothing and only hands over to the native Task Manager
    let trusted = if osutil::is_elevated() { RealHost.launcher_trusted(exe) } else { Err("the launcher is not running elevated".to_string()) };
    let native = state.as_ref().map(|s| s.native_path.clone()).unwrap_or(None);
    let native = native_path(&target, &sysroot(), native.as_deref(), exe);
    match plan(&c, state.as_ref()) {
        Plan::Abort => EXIT_LOOP,
        Plan::SelfTest => {
            if !osutil::is_elevated() {
                return EXIT_NOT_ELEVATED;
            }
            if trusted.is_err() {
                return EXIT_UNTRUSTED;
            }
            if state.is_none() {
                return EXIT_NO_STATE;
            }
            match native.as_deref() {
                Some(n) if start_native(n, true) => EXIT_SELF_TEST_OK,
                _ => EXIT_NATIVE_FAILED,
            }
        }
        plan => {
            // only a Hayate recorded at enable time, or this launcher's own protected image, is ever
            // messaged; only this launcher's own image is ever started, and only while it is itself
            // where and how it must be (`RealLaunch::start_hayate` checks that again)
            // no lock on the secure desktop either: `launch` goes straight to native there
            // (an unreadable desktop name counts as "not interactive")
            let interactive = desktop_name().is_some_and(|n| interactive_desktop(&n));
            let lock = if plan == Plan::Show && trusted.is_ok() && interactive { ColdLock::acquire() } else { None };
            let mut h = RealLaunch { exe: exe.to_string(), recorded: state.as_ref().map(|s| s.hayate_path.clone()).unwrap_or_default(), trusted, native, child: None, interactive, locked: lock.as_ref().is_some_and(|l| l.held) };
            let d = launch(&plan, &mut h);
            if !d.ok {
                // nobody is watching this process: say it, Task Manager did not open
                error_box("Hayate could not start Windows Task Manager. Hold Shift next time, or run Hayate > Settings > Open Hayate instead of Task Manager = off (needs administrator rights).");
            }
            exit_code(&d)
        }
    }
}

fn error_box(text: &str) {
    let (t, c) = (wide(text), wide("Hayate: Task Manager"));
    unsafe { MessageBoxW(None, pcw(&t), pcw(&c), MB_OK | MB_ICONERROR) };
}

/// Staging targets and `--target-path`: debug builds (rig tests) only.
fn staging_allowed() -> bool {
    cfg!(debug_assertions)
}

struct RealLaunch {
    /// this launcher's own image (== the verified Program Files path)
    exe: String,
    recorded: String,
    trusted: Result<(), String>,
    native: Option<String>,
    /// the Hayate this launcher started (process handle)
    child: Option<HANDLE>,
    interactive: bool,
    locked: bool,
}

impl RealLaunch {
    /// A Hayate window of the recorded image or the launcher's own image answers within
    /// `timeout_ms`: show it in front.
    fn look(&self, timeout_ms: u32) -> Seen {
        let msg = crate::win::show_msg();
        let mut prev: Option<HWND> = None;
        loop {
            let Ok(h) = (unsafe { FindWindowExW(None, prev, w!("HayateMain"), None) }) else { return Seen::Absent };
            if h.is_invalid() {
                return Seen::Absent;
            }
            prev = Some(h);
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
            if !osutil::image_path(pid).is_some_and(|p| hayate_window_ok(&p, &self.recorded, &self.exe)) {
                continue;
            }
            let mut res = 0usize;
            unsafe {
                let _ = AllowSetForegroundWindow(pid);
                // wp 3: "Task Manager was asked for": show in front, nothing else
                let r = SendMessageTimeoutW(h, msg, WPARAM(3), LPARAM(0), SMTO_ABORTIFHUNG | SMTO_BLOCK, timeout_ms, Some(&mut res));
                return if r.0 != 0 { Seen::Shown } else { Seen::Hung };
            }
        }
    }

    /// Variables of a cold-started Hayate that come from the system (known folders), never
    /// from the environment the caller handed the elevated launcher.
    fn system_env() -> std::result::Result<Vec<(String, String)>, String> {
        let (local, roaming, profile) = crate::setup::user_folders().ok_or("cannot read the user's profile folders")?;
        let root = sysroot();
        if root.is_empty() {
            return Err("cannot read the Windows folder".into());
        }
        let program_files = crate::setup::program_files().ok_or("cannot read the Program Files folder")?;
        let program_files_x86 = crate::setup::program_files_x86().unwrap_or_else(|| format!("{program_files} (x86)"));
        let program_data = crate::setup::program_data().ok_or("cannot read the ProgramData folder")?;
        Ok(system_vars(&SystemFolders { local: &local, roaming: &roaming, profile: &profile, root: &root, program_files: &program_files, program_files_x86: &program_files_x86, program_data: &program_data }))
    }
}

impl LaunchHost for RealLaunch {
    fn on_interactive_desktop(&self) -> bool {
        self.interactive
    }

    fn lock_held(&self) -> bool {
        self.locked
    }

    fn trusted(&self) -> std::result::Result<(), String> {
        self.trusted.clone()
    }

    fn show_running(&mut self) -> Seen {
        self.look(2000)
    }

    fn start_hayate(&mut self) -> std::result::Result<(), String> {
        use std::os::windows::fs::OpenOptionsExt;
        // hold the launcher file open with no write/delete/rename sharing while it is re-verified
        // and started: nobody can swap it between the check and CreateProcess
        let _pin = std::fs::OpenOptions::new().read(true).share_mode(1).open(&self.exe).map_err(|e| format!("cannot lock {}: {e}", self.exe))?;
        RealHost.launcher_trusted(&self.exe)?;
        if !osutil::is_elevated() {
            return Err("not elevated".into());
        }
        let parent: Vec<(String, String)> = std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect();
        let mut env = child_env(&parent, &Self::system_env()?);
        env.sort_by_key(|(k, _)| k.to_uppercase());
        let mut block: Vec<u16> = Vec::new();
        for (k, v) in &env {
            block.extend(format!("{k}={v}").encode_utf16());
            block.push(0);
        }
        block.push(0);
        let dir = osutil::parent_dir(&self.exe).to_string();
        let (app, dirw) = (wide(&self.exe), wide(&dir));
        let mut cmd = wide(&format!("\"{}\"", self.exe));
        let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
        let mut pi = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessW(pcw(&app), Some(PWSTR(cmd.as_mut_ptr())), None, None, false, CREATE_UNICODE_ENVIRONMENT, Some(block.as_ptr() as *const _), pcw(&dirw), &si, &mut pi).map_err(|e| format!("cannot start {}: {e}", self.exe))?;
            let _ = CloseHandle(pi.hThread);
            let _ = AllowSetForegroundWindow(pi.dwProcessId);
        }
        self.child = Some(pi.hProcess);
        Ok(())
    }

    fn wait_window(&mut self, ms: u32) -> bool {
        let t0 = Instant::now();
        let ok = loop {
            if self.look(400) == Seen::Shown {
                break true;
            }
            // the started Hayate already ended (it found another instance, or crashed): no point waiting
            if self.child.is_some_and(|h| unsafe { WaitForSingleObject(h, 0) } == WAIT_OBJECT_0) {
                break false;
            }
            if t0.elapsed() >= Duration::from_millis(ms as u64) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if ok {
            if let Some(h) = self.child.take() {
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
        }
        ok
    }

    fn stop_started(&mut self) {
        if let Some(h) = self.child.take() {
            unsafe {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
    }

    fn start_native(&mut self) -> bool {
        self.native.as_deref().is_some_and(|n| start_native(n, false))
    }
}

/// Start the native image with `DEBUG_PROCESS` (IFEO is skipped, nothing is
/// toggled), then let it go: no kill-on-exit, detach. `test_only`: create it
/// suspended and terminate it before it ever runs (the probe's dry run).
fn start_native(image: &str, test_only: bool) -> bool {
    let app = wide(image);
    let mut cmd = wide(&format!("\"{image}\""));
    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    let flags = PROCESS_CREATION_FLAGS(NATIVE_FLAGS | if test_only { CREATE_SUSPENDED.0 } else { 0 });
    unsafe {
        std::env::set_var(format!("HAYATE_{GUARD_ENV}"), "1");
        if CreateProcessW(pcw(&app), Some(PWSTR(cmd.as_mut_ptr())), None, None, false, flags, None, None, &si, &mut pi).is_err() {
            return false;
        }
        let _ = CloseHandle(pi.hThread);
        if test_only {
            let _ = TerminateProcess(pi.hProcess, 0);
            let _ = CloseHandle(pi.hProcess);
            return true;
        }
        let _ = DebugSetProcessKillOnExit(false);
        let mut ev = DEBUG_EVENT::default();
        if WaitForDebugEvent(&mut ev, 5000).is_ok() {
            if ev.dwDebugEventCode == CREATE_PROCESS_DEBUG_EVENT {
                let f = ev.u.CreateProcessInfo.hFile;
                if !f.is_invalid() {
                    let _ = CloseHandle(f);
                }
            }
            let _ = ContinueDebugEvent(ev.dwProcessId, ev.dwThreadId, DBG_CONTINUE);
        }
        let ok = DebugActiveProcessStop(pi.dwProcessId).is_ok();
        let _ = CloseHandle(pi.hProcess);
        ok
    }
}

// ---- the elevated helper (hayate.exe --taskmgr-enable / --taskmgr-disable / --taskmgr-refresh) ----

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Enable,
    Disable,
    /// turn off a redirect installed by the pre-rename build (its launcher is in Program Files\Blink)
    DisableLegacy,
    /// replace the launcher file with this exe (docs §7.5 "Launcher update"); takes no path argument
    Refresh,
}

/// `main`: a `--taskmgr-*` start runs once and exits (no window).
pub fn cli() -> bool {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let Some(op) = a.first().map(|s| s.as_str()).and_then(|s| match s {
        "--taskmgr-enable" => Some(Op::Enable),
        "--taskmgr-disable" => Some(Op::Disable),
        "--taskmgr-disable-legacy" => Some(Op::DisableLegacy),
        "--taskmgr-refresh" => Some(Op::Refresh),
        _ => None,
    }) else {
        return false;
    };
    let flag = |n: &str| a.iter().position(|x| x == n).and_then(|i| a.get(i + 1)).cloned();
    let target = flag("--target");
    let image = flag("--target-path");
    let code = run_helper_op(op, target.as_deref(), image.as_deref());
    log(&format!("taskmgr: {} {} -> exit {code} ({})", a[0], target.as_deref().unwrap_or("all"), exit_message(code)));
    std::process::exit(code);
}

/// A leftover staged copy (`hayate-tm.new*.exe`) older than this is a cut-short run's: swept at the start of a step.
const STAGED_MAX_AGE: Duration = Duration::from_secs(600);

fn sweep_staged(pf: &str) {
    let Some(l) = expected_launcher(pf) else { return };
    let Some(dir) = Path::new(&l).parent().map(Path::to_path_buf) else { return };
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().to_ascii_lowercase();
        if n.starts_with("hayate-tm.new") && n.ends_with(".exe") && e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > STAGED_MAX_AGE) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The last failed step's text, target independent: `HKLM\SOFTWARE\Hayate\TaskManager` value `LastError`.
fn clear_last_error(reg: &mut dyn Registry) {
    let _ = reg.delete_value(STATE_KEY, "LastError");
    if reg.is_empty(STATE_KEY) {
        let _ = reg.delete_key(STATE_KEY);
    }
}

fn run_helper_op(op: Op, target: Option<&str>, image: Option<&str>) -> i32 {
    if !osutil::is_elevated() {
        return EXIT_NOT_ELEVATED;
    }
    let mut reg = RealRegistry::new();
    // ONE step at a time on this machine, before any state is read
    let _op = match OpLock::take() {
        Ok(l) => l,
        Err(e) => {
            log(&format!("taskmgr: {e}"));
            // no lock, so no state is touched; the text alone is written (cleared by the next step), so the
            // asking Hayate never shows the text of an older failure for this one
            let _ = reg.set(STATE_KEY, "LastError", &clean_text(&e));
            return EXIT_BUSY;
        }
    };
    // the elevated helper writes no hayate.log: its failure text goes into the admin-only state key
    // (read by the Hayate that asked), cleared at the start of every run, whatever the run decides
    clear_last_error(&mut reg);
    // disable without --target turns everything of ours off (staging state too, in debug builds)
    let all = op == Op::Disable && target.is_none();
    let Some(target) = valid_target(target.unwrap_or(REAL_TARGET)) else { return EXIT_REFUSED };
    if !helper_args_allowed(&target, image, staging_allowed()) {
        return EXIT_REFUSED;
    }
    // the refresh copies the helper's OWN image and nothing else: no path is ever accepted
    if op == Op::Refresh && image.is_some() {
        return EXIT_REFUSED;
    }
    let st = crate::setup::status();
    // the real Task Manager only from an installed Hayate (a portable copy may drive a staging target)
    if !matches!(op, Op::Disable | Op::DisableLegacy) && is_real(&target) && st.at == crate::setup::logic::Where::Portable {
        return EXIT_REFUSED;
    }
    let pf = program_files();
    sweep_staged(&pf);
    let mut host = RealHost;
    let r = match op {
        Op::Enable => enable(&mut reg, &mut host, &target, &pf, &st.exe, image).map(|o| format!("{o:?}")),
        Op::Disable if all => disable_all(&mut reg, &mut host, &pf, staging_allowed()).map(|o| format!("{o:?}")),
        Op::Disable => disable(&mut reg, &mut host, &target, &pf).map(|o| format!("{o:?}")),
        Op::DisableLegacy => disable_legacy(&mut reg, &mut host, &target, &pf).map(|o| format!("{o:?}")),
        Op::Refresh => refresh(&mut reg, &mut host, &target, &pf, &st.exe).map(|o| format!("{o:?}")),
    };
    match r {
        Ok(o) => {
            log(&format!("taskmgr: {o}"));
            EXIT_OK
        }
        Err(e) => {
            log(&format!("taskmgr: {e}"));
            // written last of all, after anything the step deleted (a disable removes the whole key)
            let _ = reg.set(STATE_KEY, "LastError", &clean_text(&e.to_string()));
            e.exit_code()
        }
    }
}

/// The text the elevated helper left for its last failed run (None: none, or the key is not trustworthy).
fn last_error() -> Option<String> {
    RealHost.state_trusted().ok()?;
    RealRegistry::new().get(STATE_KEY, "LastError").ok().flatten().map(|t| clean_text(&t)).filter(|t| !t.is_empty())
}

// ---- Settings ----------------------------------------------------------------------

static CACHE: Mutex<Option<(Instant, Status)>> = Mutex::new(None);

/// Current state of the real redirect, read at most every 3 s (only while the
/// Settings panel is open).
pub fn ui_status() -> Status {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, s)) = g.as_ref() {
        if t.elapsed() < Duration::from_secs(3) {
            return s.clone();
        }
    }
    let s = status(&RealRegistry::new(), &RealHost, REAL_TARGET, &program_files());
    *g = Some((Instant::now(), s.clone()));
    s
}

fn forget() {
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *FRESH.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

// ---- launcher freshness (Settings offer, docs 7.5 "Launcher update") --------------------

/// What Settings knows about the launcher file: the hashes it compared.
#[derive(Clone)]
struct FreshView {
    fresh: Fresh,
    launcher_sha: Option<String>,
    running_sha: Option<String>,
}

static FRESH: Mutex<Option<(Instant, FreshView)>> = Mutex::new(None);
static FRESH_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// sha256 of this process's own exe, hashed once (the file is not changed while Hayate runs)
static RUNNING_SHA: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
/// (size, last write) of a file at its last hash, and that hash: a hash is redone only when the file changed
type HashKey = (u64, Option<std::time::SystemTime>);
type HashMemo = std::collections::HashMap<String, (HashKey, Option<String>)>;
static HASHES: Mutex<Option<HashMemo>> = Mutex::new(None);
/// an elevated self-update just started this build: the next auto check ignores its "tried" memory
static FORCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Called (from a worker thread) when the launcher freshness answer changed; the main window sets it.
static REPAINT: Mutex<Option<Box<dyn Fn() + Send + Sync>>> = Mutex::new(None);

pub fn set_repaint(f: impl Fn() + Send + Sync + 'static) {
    *REPAINT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
}

fn running_sha() -> Option<String> {
    RUNNING_SHA.get_or_init(|| std::env::current_exe().ok().and_then(|p| crate::update::sha256_file(&p).ok())).clone()
}

fn sha_cached(path: &str) -> Option<String> {
    let md = std::fs::metadata(path).ok()?;
    let key: HashKey = (md.len(), md.modified().ok());
    if let Some((k, h)) = HASHES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).get(path) {
        if *k == key {
            return h.clone();
        }
    }
    let h = crate::update::sha256_file(Path::new(path)).ok();
    HASHES.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(path.to_string(), (key, h.clone()));
    h
}

/// Launcher vs this Hayate (reads files and hashes them: never on the UI thread). The cold-started
/// launcher image compares itself with the installed Hayate instead (it has no build ordinal of that one).
fn compute_fresh() -> FreshView {
    let reg = RealRegistry::new();
    let st = read_state(&reg, REAL_TARGET);
    let launcher = expected_launcher(&program_files()).and_then(|l| sha_cached(&l));
    let (running, running_build) = if is_launcher_image() {
        (st.as_ref().map(|s| s.hayate_path.clone()).filter(|p| !p.trim().is_empty()).and_then(|p| sha_cached(&p)), None)
    } else {
        (running_sha(), Some(build_ordinal()))
    };
    FreshView { fresh: freshness(launcher.as_deref(), running.as_deref(), st.and_then(|s| s.build), running_build), launcher_sha: launcher, running_sha: running }
}

/// The last answer, refreshed on a worker thread at most every 3 s (what the Settings painter reads:
/// no file is opened or hashed on the UI thread; until the first answer nothing is offered).
fn fresh_view() -> FreshView {
    let known = FRESH.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let due = known.as_ref().is_none_or(|(t, _)| t.elapsed() >= Duration::from_secs(3));
    if due && !FRESH_BUSY.swap(true, std::sync::atomic::Ordering::AcqRel) {
        std::thread::spawn(|| {
            let v = compute_fresh();
            let changed = FRESH.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_none_or(|(_, o)| o.fresh != v.fresh);
            *FRESH.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v));
            FRESH_BUSY.store(false, std::sync::atomic::Ordering::Release);
            // a new answer: the open Settings panel paints it now, not at the next mouse move
            if changed {
                if let Some(f) = REPAINT.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    f();
                }
            }
        });
    }
    known.map(|(_, v)| v).unwrap_or(FreshView { fresh: Fresh::Unknown, launcher_sha: None, running_sha: None })
}

/// This process is the launcher image: the Hayate the launcher cold-started. It never refreshes itself.
pub fn is_launcher_image() -> bool {
    osutil::cold_started() || std::env::current_exe().is_ok_and(|e| e.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(LAUNCHER_NAME)))
}

/// What Settings shows under the Task Manager switch about the launcher file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Offer {
    None,
    /// this Hayate is newer than the launcher: line + [Refresh launcher]
    Refresh,
    /// the launcher file is gone, the previous copy is there: line + [Repair launcher]
    Repair,
    /// the cold-started launcher image sees a different installed Hayate: a hint only, no button
    Hint,
    /// the launcher is a newer build than this Hayate: nothing to do (a note, no button)
    Newer,
}

pub fn offer(st: &Status) -> Offer {
    if !allowed() {
        return Offer::None;
    }
    if is_launcher_image() {
        return if *st == Status::On && fresh_view().fresh == Fresh::Stale { Offer::Hint } else { Offer::None };
    }
    // not hidden while a step runs: the block keeps its height (the button is disabled instead)
    let Some(l) = expected_launcher(&program_files()) else { return Offer::None };
    let enabled = read_state(&RealRegistry::new(), REAL_TARGET).is_some_and(|s| s.enabled);
    if repair_offer(st, enabled, Path::new(&l).exists(), Path::new(&old_path(&l)).exists(), true, false, false) {
        return Offer::Repair;
    }
    match (st, fresh_view().fresh) {
        (Status::On, Fresh::Stale) => Offer::Refresh,
        (Status::On, Fresh::NotNewer) => Offer::Newer,
        _ => Offer::None,
    }
}

pub fn offer_text(o: Offer) -> &'static str {
    match o {
        Offer::Refresh => "The Task Manager launcher is an older build than this Hayate.",
        Offer::Repair => "The Task Manager launcher file is missing, so Task Manager cannot open until it is put back.",
        Offer::Hint => "The Task Manager launcher differs from the installed Hayate. Quit this Hayate, start it from the Start menu, then press Refresh launcher in Settings > Task Manager.",
        Offer::Newer => "The Task Manager launcher is newer than this Hayate; nothing to do.",
        Offer::None => "",
    }
}

pub fn offer_button(o: Offer) -> Option<&'static str> {
    match o {
        Offer::Refresh => Some("Refresh launcher"),
        Offer::Repair => Some("Repair launcher"),
        _ => None,
    }
}

/// An elevated self-update just started this build (called from `update::early`).
pub fn note_updated() {
    FORCED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The switch is usable from an installed Hayate only.
pub fn allowed() -> bool {
    crate::setup::status().at != crate::setup::logic::Where::Portable
}

/// Run the elevated helper (one UAC prompt) and wait for it.
fn helper(arg: &str) -> std::result::Result<(), String> {
    let exe = std::path::PathBuf::from(&crate::setup::status().exe);
    let (file, params) = (wide(&exe.to_string_lossy()), wide(arg));
    let dir = wide(&exe.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default());
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: pcw(&file),
        lpParameters: pcw(&params),
        lpDirectory: pcw(&dir),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED | windows::Win32::System::Com::COINIT_DISABLE_OLE1DDE);
        ShellExecuteExW(&mut sei).map_err(|e| if e.code() == ERROR_CANCELLED.to_hresult() { "cancelled in the UAC prompt: nothing changed".to_string() } else { e.message() })?;
        let h = sei.hProcess;
        let mut w = WaitForSingleObject(h, 120_000);
        if w == WAIT_TIMEOUT {
            // still running: the gate stays closed until it ends (a second click must not start a second helper)
            osutil::log("taskmgr: the elevated step takes longer than 2 minutes; still waiting for it");
            w = WaitForSingleObject(h, INFINITE);
        }
        let mut code = 1u32;
        let _ = GetExitCodeProcess(h, &mut code);
        let _ = CloseHandle(h);
        forget();
        if w == WAIT_TIMEOUT {
            return Err("the elevated step did not finish in 2 minutes".into());
        }
        if code as i32 == EXIT_OK {
            Ok(())
        } else {
            let msg = exit_message(code as i32);
            let detailed = matches!(code as i32, EXIT_UNTRUSTED | EXIT_PROBE | EXIT_KEPT | EXIT_BUSY | EXIT_IO | EXIT_REGISTRY | EXIT_REFUSED | EXIT_FOREIGN);
            Err(match last_error().filter(|_| detailed) {
                Some(d) => format!("{d} ({msg})"),
                None => msg,
            })
        }
    }
}

/// Settings switch: UAC, then the helper does everything. Blocks until the helper is done.
pub fn set_enabled(on: bool) -> std::result::Result<(), String> {
    // this is the user's own Hayate (not the elevated helper): its log is the normal one
    osutil::log(&format!("taskmgr: Settings asked to turn the redirect {} (this Hayate elevated={})", if on { "on" } else { "off" }, osutil::is_elevated()));
    let t = Instant::now();
    let r = helper(if on { "--taskmgr-enable" } else { "--taskmgr-disable" });
    osutil::log(&format!("taskmgr: helper finished in {} ms: {}", t.elapsed().as_millis(), match &r { Ok(()) => "ok".to_string(), Err(e) => e.clone() }));
    r
}

static GATE: Gate = Gate::new();
static RESULT: Mutex<Option<std::result::Result<(), String>>> = Mutex::new(None);

/// A helper run is under way (the Settings line says so, a second click is ignored).
pub fn busy() -> bool {
    GATE.busy()
}

/// Settings click: run the helper on a worker thread so the window never freezes for the
/// seconds it takes (a frozen window turns into a ghost after 5 s and swallows clicks).
/// `done` runs on the worker when the result is ready (it posts to the UI thread).
pub fn start_toggle(on: bool, done: impl FnOnce() + Send + 'static) {
    if !GATE.enter() {
        return;
    }
    std::thread::spawn(move || {
        let r = set_enabled(on);
        *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        GATE.leave();
        done();
    });
}

/// Settings "Refresh launcher": one UAC prompt (none when this Hayate is already elevated), then the
/// elevated helper replaces the launcher file with its own image and checks it. Never from the
/// launcher image itself.
pub fn start_refresh(done: impl FnOnce() + Send + 'static) {
    if is_launcher_image() || !allowed() || !GATE.enter() {
        return;
    }
    std::thread::spawn(move || {
        osutil::log(&format!("taskmgr: Settings asked to refresh the launcher (this Hayate elevated={})", osutil::is_elevated()));
        let r = run_refresh_helper();
        *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        GATE.leave();
        done();
    });
}

fn run_refresh_helper() -> std::result::Result<(), String> {
    let t = Instant::now();
    let r = helper("--taskmgr-refresh");
    osutil::log(&format!("taskmgr: refresh helper finished in {} ms: {}", t.elapsed().as_millis(), match &r { Ok(()) => "ok".to_string(), Err(e) => e.clone() }));
    r
}

/// From the main window once it exists, in an ELEVATED installed Hayate: when the redirect is on and the
/// launcher is older than this Hayate (or its file is missing), refresh it without a prompt (the helper
/// starts elevated from an elevated process). Only when this exe passes `verify_admin_only` (the same
/// rule as the uninstall gate: a user-writable exe never gets a silent elevated copy). One attempt per
/// (launcher, this Hayate) pair, remembered in the admin-only state key (not in the user's data dir), so
/// a failing refresh never loops; an elevated self-update forces one more. The Settings button stays
/// for everything else. Always on a worker thread, always logged.
pub fn auto_refresh(done: impl FnOnce() + Send + 'static) {
    if !osutil::is_elevated() || is_launcher_image() || !allowed() || !GATE.enter() {
        return;
    }
    std::thread::spawn(move || {
        let forced = FORCED.swap(false, std::sync::atomic::Ordering::Relaxed);
        let pf = program_files();
        let exe = std::env::current_exe().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        if let Err(e) = verify_admin_only(&exe, true) {
            osutil::log(&format!("taskmgr: no automatic launcher refresh: this Hayate is not an admin-only install ({e}); the Settings button asks first"));
            GATE.leave();
            return;
        }
        let mut reg = RealRegistry::new();
        if let Err(e) = RealHost.state_trusted() {
            osutil::log(&format!("taskmgr: no automatic launcher refresh: {e}"));
            GATE.leave();
            return;
        }
        let st = status(&reg, &RealHost, REAL_TARGET, &pf);
        forget();
        let v = compute_fresh();
        let sk = state_key(REAL_TARGET);
        let launcher = expected_launcher(&pf).unwrap_or_default();
        let enabled = read_state(&reg, REAL_TARGET).is_some_and(|s| s.enabled);
        let repair = repair_offer(&st, enabled, Path::new(&launcher).exists(), Path::new(&old_path(&launcher)).exists(), true, false, false);
        let pair = pair_key(v.launcher_sha.as_deref().unwrap_or(""), v.running_sha.as_deref().unwrap_or(""));
        let tried = reg.get(&sk, "RefreshTried").ok().flatten().map(|t| t.trim().to_string());
        let offer = repair || refresh_offer(&st, &v.fresh, true, false, false);
        // a missing launcher breaks Task Manager outright: it is repaired at every start (the repair either works or removes the value)
        if !auto_refresh_due(offer, true, tried.as_deref(), &pair, forced || repair) {
            GATE.leave();
            return;
        }
        osutil::log(&format!("taskmgr: elevated start, {} ({pair}{}): {}", if repair { "launcher file missing" } else { "launcher older than this Hayate" }, if forced { ", after an elevated update" } else { "" }, if repair { "repairing it" } else { "refreshing it" }));
        // remembered BEFORE the attempt: a crash or a failure is not retried at every start
        // under the same machine-wide exclusion as the helper steps, and only while the redirect is still
        // on: a disable that ran meanwhile must never find its state key written to (never recreated)
        {
            let Ok(_op) = OpLock::take() else {
                GATE.leave();
                return;
            };
            if read_state(&reg, REAL_TARGET).is_none_or(|s| !s.enabled) {
                GATE.leave();
                return;
            }
            let _ = reg.set(&sk, "RefreshTried", &pair);
        }
        let r = run_refresh_helper();
        let failed = r.clone().err();
        *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        GATE.leave();
        osutil::log(&format!("taskmgr: automatic launcher refresh {}", match &failed { None => "done".to_string(), Some(e) => format!("failed: {e}") }));
        done();
    });
}

/// The finished run's result, once (None: nothing finished since the last call).
pub fn take_result() -> Option<std::result::Result<(), String>> {
    RESULT.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Before Hayate uninstalls itself: a redirect that is on (or half there) is
/// turned off first, with UAC. Cancelled or failed: the uninstall stops.
pub fn uninstall_gate() -> std::result::Result<(), String> {
    forget();
    let reg = RealRegistry::new();
    let user_exe = crate::setup::status().at == crate::setup::logic::Where::User;
    // a redirect installed by the pre-rename build counts as ours (exact quoted
    // launcher in Program Files\Blink only; any other form aborts). It is turned
    // off with the same elevated helper the Settings switch uses, so a per-user
    // install can uninstall too (UAC shows the exe, as in Settings).
    let legacy = legacy_uninstall_step(&reg, &program_files());
    match legacy {
        UninstallStep::Nothing => {}
        UninstallStep::Refuse(m) => return Err(m),
        UninstallStep::TurnOff => {
            if !GATE.enter() {
                return Err("A Task Manager step is still running. Wait a moment and try again. Nothing was removed.".into());
            }
            let r = helper("--taskmgr-disable-legacy");
            GATE.leave();
            r.map_err(|e| format!("Hayate still redirects Task Manager through its pre-rename launcher and could not turn that off ({e}). Nothing was removed."))?;
            forget();
        }
    }
    let step = uninstall_needed(&RealRegistry::new(), &RealHost, &program_files(), staging_allowed());
    match gate_plan(step, user_exe) {
        GatePlan::Proceed => return Ok(()),
        GatePlan::Refuse(m) => return Err(m),
        GatePlan::TurnOff => {}
    }
    if !GATE.enter() {
        return Err("A Task Manager step is still running. Wait a moment and try again. Nothing was removed.".into());
    }
    let r = set_enabled(false);
    GATE.leave();
    r.map_err(|e| format!("Hayate still redirects Task Manager and could not turn that off ({e}). Turn it off in Settings first. Nothing was removed."))
}

/// Settings line under the switch.
pub fn describe(s: &Status) -> String {
    if busy() {
        return "Working: waiting for the administrator step (a few seconds)...".into();
    }
    match s {
        Status::Off => "Off: Ctrl+Shift+Esc opens Windows Task Manager.".into(),
        Status::On => "On for all users: Task Manager opens Hayate, started from its protected copy in Program Files (as administrator) if it is not open. Hold Shift for Windows Task Manager.".into(),
        Status::Stale(why) => format!("Broken: Task Manager may not open ({why}). Turn the switch off to repair."),
        Status::Reset => "Off: Windows reset it. Turn it on again if you want it.".into(),
        Status::Foreign(_) => "Another program already redirects Task Manager. Hayate leaves it alone.".into(),
        Status::PendingRemoval => "Off. hayate-tm.exe is still present in Program Files (it was running, or is left over). Switch Hayate Task Manager on and off again, run install.ps1 -Uninstall, or delete it as administrator.".into(),
    }
}
