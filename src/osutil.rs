//! Small shared helpers for the Rules / Watch backend: wide strings, data dir,
//! time, process image paths, NT device → DOS path, service ImagePath cleanup.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::QueryDosDeviceW;
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};

/// The product name every user-visible text uses.
pub const APP_NAME: &str = "Hayate";

/// Environment variable prefix of the pre-rename build whose value is still read
/// when the `HAYATE_` name is not set (rig tooling and older docs use them).
const LEGACY_ENV: &str = "BLINK_";

fn env_key(suffix: &str) -> String {
    format!("HAYATE_{suffix}")
}

/// `HAYATE_<suffix>`, else the pre-rename `BLINK_<suffix>` (never removed: scripts
/// and rig test setups still set the old names).
pub fn env_var(suffix: &str) -> Result<String, std::env::VarError> {
    match std::env::var(env_key(suffix)) {
        Ok(v) => Ok(v),
        Err(std::env::VarError::NotPresent) => std::env::var(format!("{LEGACY_ENV}{suffix}")),
        Err(e) => Err(e),
    }
}

/// [`env_var`] as an `OsString` (`None` when neither name is set).
pub fn env_var_os(suffix: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(env_key(suffix)).or_else(|| std::env::var_os(format!("{LEGACY_ENV}{suffix}")))
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcw(v: &[u16]) -> PCWSTR {
    PCWSTR(v.as_ptr())
}

/// UTF-16 up to the first NUL.
pub fn from_wide(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}

pub unsafe fn from_pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

/// `HAYATE_MULTI=1` (or legacy `BLINK_MULTI=1`): dev/test instance next to the user's
/// running Hayate: skips the single-instance check, uses its own window class, ETW
/// session name and (unless HAYATE_DATA_DIR is set) its own data dir `%LOCALAPPDATA%\Hayate-multi`.
pub fn multi() -> bool {
    static M: OnceLock<bool> = OnceLock::new();
    *M.get_or_init(|| !KNOWN_ONLY.load(std::sync::atomic::Ordering::Relaxed) && env_var_os("MULTI").is_some_and(|v| v != "0"))
}

/// Set in an elevated uninstall / all-users install: the data dir is the token user's known-folder
/// `LocalAppData\Hayate`, whatever HAYATE_DATA_DIR / HAYATE_MULTI / `--update-data-dir` say.
static KNOWN_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn lock_data_dir_to_known_folder() {
    KNOWN_ONLY.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// `%LOCALAPPDATA%\Hayate`, created on first use. Tests can redirect it with HAYATE_DATA_DIR.
/// Unit tests never touch the real one: without HAYATE_DATA_DIR they get
/// `%TEMP%\hayate-test-<pid>` (cleared by a later run once a day old).
pub fn data_dir() -> PathBuf {
    let known = KNOWN_ONLY.load(std::sync::atomic::Ordering::Relaxed);
    let p = if known {
        match crate::setup::local_app_data() {
            Some(b) => PathBuf::from(b).join("Hayate"),
            None => return PathBuf::from(REFUSED_DATA_DIR),
        }
    } else if let Some(d) = env_var_os("DATA_DIR") {
        PathBuf::from(d)
    } else if cfg!(test) {
        test_data_dir()
    } else {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
        PathBuf::from(base).join(if multi() { "Hayate-multi" } else { "Hayate" })
    };
    // An elevated Hayate never follows a link out of the user-writable data dir (the medium
    // user can turn it into a junction/symlink): it gets a path nothing can be opened under,
    // so reads see defaults and writes fail, instead of writing wherever the link points.
    if is_elevated() && !cfg!(test) && (known || env_var_os("DATA_DIR").is_none()) && reparse_in_chain(&p) {
        return PathBuf::from(REFUSED_DATA_DIR);
    }
    let _ = std::fs::create_dir_all(&p);
    p
}

/// Hex of this process's token user SID (compare two accounts; never a path).
pub fn user_sid_hex() -> Option<String> {
    use windows::Win32::Security::{GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut t = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t).ok()?;
        let tok = Handle(t);
        let mut n = 0u32;
        let _ = GetTokenInformation(tok.0, TokenUser, None, 0, &mut n);
        if n == 0 {
            return None;
        }
        let mut buf = vec![0u8; n as usize];
        GetTokenInformation(tok.0, TokenUser, Some(buf.as_mut_ptr() as *mut _), n, &mut n).ok()?;
        let tu = std::ptr::read_unaligned(buf.as_ptr() as *const TOKEN_USER);
        let len = GetLengthSid(tu.User.Sid) as usize;
        if len == 0 || len > 68 {
            return None;
        }
        Some(std::slice::from_raw_parts(tu.User.Sid.0 as *const u8, len).iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// A path with an illegal name (no I/O, no lookup, always fails): stands in for a data dir that was refused.
pub const REFUSED_DATA_DIR: &str = r"|:\hayate-data-dir-refused";

/// Is `p` or any existing folder above it a reparse point (junction, symlink, mount point)?
pub fn reparse_in_chain(p: &std::path::Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    p.ancestors().filter(|a| a.parent().is_some()).any(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.file_attributes() & 0x400 != 0))
}

/// The Windows folder from the system (GetSystemWindowsDirectoryW), never `%SystemRoot%`.
/// `Err` when it cannot be read: callers skip the action (no hard-coded `C:\Windows` for exec paths).
pub fn windows_dir() -> Result<String, String> {
    let mut b = [0u16; 260];
    let n = unsafe { windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW(Some(&mut b)) } as usize;
    if n == 0 || n >= b.len() {
        return Err("cannot read the Windows folder".into());
    }
    Ok(from_wide(&b[..n]))
}

/// True in the elevated Hayate the Task Manager launcher cold-started: its PATH is minimal and
/// bare-name launches (winget) cannot work, so those actions are off there.
static COLD_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn cold_started() -> bool {
    COLD_STARTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The cold-started elevated Hayate writes no log: its `%LOCALAPPDATA%\Hayate` is the medium
/// user's (docs/task-manager-replacement.md §7.3a "Data dir").
static NO_LOG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn disable_log() {
    NO_LOG.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Called once in the cold-started elevated Hayate: marks it cold-started and silences its log.
pub fn mark_cold_started() {
    COLD_STARTED.store(true, std::sync::atomic::Ordering::Relaxed);
    disable_log();
}

/// Per-process scratch data dir for `cargo test`; the first call clears
/// leftovers of earlier runs (dirs whose pid is gone are older than a day).
fn test_data_dir() -> PathBuf {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(|| {
        let tmp = std::env::temp_dir();
        if let Ok(rd) = std::fs::read_dir(&tmp) {
            let day = std::time::Duration::from_secs(86400);
            for e in rd.flatten() {
                let old = e.metadata().and_then(|m| m.modified()).map(|t| t.elapsed().map(|a| a > day).unwrap_or(false)).unwrap_or(false);
                if old && e.file_name().to_string_lossy().starts_with("hayate-test-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        tmp.join(format!("hayate-test-{}", std::process::id()))
    })
    .clone()
}

/// Write to `<file>.tmp` then rename over: a crash never leaves a half file.
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// Read at most `max` bytes of a file, never allocating more than `max + 1`:
/// a file that is bigger is an error (`InvalidData`), not a full read followed
/// by a size check.
pub fn read_bounded(path: &std::path::Path, max: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(max as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > max {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "file is too large"));
    }
    Ok(buf)
}

#[cfg(test)]
thread_local! {
    /// test-only fault injection for [`write_durable`]: 1 = temp write fails,
    /// 2 = flush fails, 3 = replace fails (the thread that sets it only)
    pub static DURABLE_FAIL: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// Replace `path` with `data` and return `Ok` only once the new content is on
/// stable storage as far as Windows can tell:
///   1. the data goes to a temp file next to `path`;
///   2. `File::sync_all` (FlushFileBuffers) flushes that file to the device;
///   3. `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` swaps it in; Microsoft
///      documents WRITE_THROUGH as "the function does not return until the file
///      is actually moved on the disk" (learn.microsoft.com, MoveFileExW).
/// Windows has no documented directory-flush call, so none is claimed: durability
/// is exactly what steps 2 and 3 are documented to give (a disk that lies about
/// its write cache cannot be detected from here). On any failure the old file is
/// left as it was and the error is returned.
pub fn write_durable(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    #[cfg(test)]
    let fail = DURABLE_FAIL.with(|f| f.get());
    #[cfg(not(test))]
    let fail = 0u8;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let staged = (|| {
        if fail == 1 {
            return Err(std::io::Error::other("injected: temp write failed"));
        }
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        if fail == 2 {
            return Err(std::io::Error::other("injected: flush failed"));
        }
        f.sync_all()
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let r = if fail == 3 {
        Err(std::io::Error::other("injected: replace failed"))
    } else {
        let (a, b) = (wide(&tmp.to_string_lossy()), wide(&path.to_string_lossy()));
        unsafe { MoveFileExW(pcw(&a), pcw(&b), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) }.map_err(|e| std::io::Error::from_raw_os_error((e.code().0 & 0xFFFF) as i32))
    };
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// FILETIME (100 ns since 1601) → unix ms.
pub fn filetime_to_ms(ft: i64) -> u64 {
    const EPOCH_DIFF: i64 = 116_444_736_000_000_000;
    if ft <= EPOCH_DIFF {
        return 0;
    }
    ((ft - EPOCH_DIFF) / 10_000) as u64
}

#[derive(Debug)]
pub struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0 .0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Open `pid` with `rights` only if it is still the process the list showed:
/// same creation time (FILETIME, what `Proc.create` holds) and not exited.
/// A reused PID, an exited process or an unknown start time (`create == 0`)
/// give `None`, so a stale row can never act on a replacement process.
pub fn open_same(pid: u32, create: i64, rights: windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS) -> Option<Handle> {
    open_verified(pid, create, rights).ok()
}

/// Why `open_verified` gave no handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenFail {
    /// the OS refused these rights (protected / higher privilege)
    Denied,
    /// exited, or the PID now belongs to another process (creation time differs)
    Gone,
    /// no usable identity (system pid, unknown start time)
    NoIdentity,
    Other(String),
}

/// Creation time of the process behind `h` (FILETIME, as `Proc.create`).
pub fn create_time_of(h: HANDLE) -> Option<i64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetProcessTimes;
    let (mut c, mut x, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u) }.ok()?;
    Some(((c.dwHighDateTime as i64) << 32) | c.dwLowDateTime as i64)
}

/// `open_same` with the reason. The returned handle is the ONLY thing an
/// action may touch afterwards: while it is open the PID cannot be reused, so
/// every call made through it lands on the verified process, never on a
/// replacement that took the PID after the list was drawn.
pub fn open_verified(pid: u32, create: i64, rights: windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS) -> Result<Handle, OpenFail> {
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};
    use windows::Win32::System::Threading::GetExitCodeProcess;
    if pid <= 4 || create == 0 {
        return Err(OpenFail::NoIdentity);
    }
    let h = match unsafe { OpenProcess(rights | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => Handle(h),
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => return Err(OpenFail::Denied),
        Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Err(OpenFail::Gone),
        Err(e) => return Err(OpenFail::Other(e.message())),
    };
    match create_time_of(h.0) {
        Some(c) if c == create => {}
        Some(_) => return Err(OpenFail::Gone),
        None => return Err(OpenFail::Other("cannot read process start time".into())),
    }
    // fail closed: an exit code that cannot be read proves nothing about "still running"
    let mut code = 0u32;
    match unsafe { GetExitCodeProcess(h.0, &mut code) } {
        Ok(()) if code == 259 => Ok(h), // 259 = STILL_ACTIVE
        Ok(()) => Err(OpenFail::Gone),
        Err(e) => Err(OpenFail::Other(format!("cannot read exit code: {}", e.message()))),
    }
}

/// (creation time, image path) of `pid`, both read through ONE handle so they
/// describe the same process. `None` when it cannot be opened or has no start time.
pub fn identity(pid: u32) -> Option<(i64, Option<String>)> {
    if pid <= 4 {
        return None;
    }
    let h = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let c = create_time_of(h.0).filter(|&c| c != 0)?;
    Some((c, image_of_handle(h.0)))
}

pub fn image_of_handle(h: HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).ok()? };
    Some(String::from_utf16_lossy(&buf[..n as usize]))
}

/// Win32 image path of a pid (works on protected processes too).
pub fn image_path(pid: u32) -> Option<String> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? };
    let h = Handle(h);
    image_of_handle(h.0)
}

pub fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

pub fn parent_dir(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(i) => &path[..i],
        None => "",
    }
}

// ---- NT device path → DOS path ---------------------------------------------

static DEVMAP: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();

fn load_devmap() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for l in b'A'..=b'Z' {
        let drive = format!("{}:", l as char);
        let w = wide(&drive);
        let mut buf = [0u16; 512];
        let n = unsafe { QueryDosDeviceW(pcw(&w), Some(&mut buf)) };
        if n > 0 {
            out.push((from_wide(&buf), drive));
        }
    }
    out
}

/// Pure mapping step (testable): `\Device\HarddiskVolume3\x` with map → `C:\x`.
pub fn map_device_path(nt: &str, map: &[(String, String)]) -> Option<String> {
    for (dev, drive) in map {
        if nt.len() > dev.len() && nt[..dev.len()].eq_ignore_ascii_case(dev) && nt.as_bytes()[dev.len()] == b'\\' {
            return Some(format!("{}{}", drive, &nt[dev.len()..]));
        }
    }
    None
}

/// `\Device\HarddiskVolumeN\...` → `C:\...`. Unknown devices are returned unchanged.
pub fn nt_to_dos(nt: &str) -> String {
    if !nt.starts_with("\\Device\\") {
        return nt.to_string();
    }
    let m = DEVMAP.get_or_init(|| Mutex::new(load_devmap()));
    if let Some(p) = map_device_path(nt, &m.lock().unwrap()) {
        return p;
    }
    // new volume mounted since: reload once
    let fresh = load_devmap();
    let r = map_device_path(nt, &fresh);
    *m.lock().unwrap() = fresh;
    r.unwrap_or_else(|| nt.to_string())
}

// ---- service ImagePath cleanup ---------------------------------------------

fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Strip quotes and args from a service ImagePath and make it an absolute DOS
/// path: handles `"C:\x y\a.exe" --arg`, `\??\C:\..`, `\SystemRoot\..`,
/// `%SystemRoot%\..`, `system32\drivers\x.sys`.
pub fn clean_image_path(raw: &str) -> String {
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    clean_image_path_with(raw, &sysroot)
}

pub fn clean_image_path_with(raw: &str, sysroot: &str) -> String {
    let s = raw.trim();
    let mut p = if let Some(r) = s.strip_prefix('"') {
        r.split('"').next().unwrap_or(r).to_string()
    } else {
        // unquoted: cut at the first ".exe"/".sys" followed by space, else first space after an extension
        let lower = s.to_ascii_lowercase();
        let mut cut = s.len();
        for ext in [".exe ", ".sys ", ".dll "] {
            if let Some(i) = lower.find(ext) {
                cut = cut.min(i + ext.len() - 1);
            }
        }
        s[..cut].to_string()
    };
    p = expand_env(&p);
    if let Some(r) = p.strip_prefix("\\??\\") {
        p = r.to_string();
    }
    let lower = p.to_ascii_lowercase();
    if lower.starts_with("\\systemroot\\") {
        p = format!("{}{}", sysroot, &p["\\systemroot".len()..]);
    } else if lower.starts_with("system32\\") || lower.starts_with("syswow64\\") {
        p = format!("{}\\{}", sysroot, p);
    } else if lower.starts_with("\\device\\") {
        p = nt_to_dos(&p);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_map() {
        let m = vec![("\\Device\\HarddiskVolume3".to_string(), "C:".to_string()), ("\\Device\\HarddiskVolume12".into(), "X:".into())];
        assert_eq!(map_device_path("\\Device\\HarddiskVolume3\\Windows\\a.exe", &m).unwrap(), "C:\\Windows\\a.exe");
        assert_eq!(map_device_path("\\Device\\HarddiskVolume12\\Dev\\b.exe", &m).unwrap(), "X:\\Dev\\b.exe");
        // Volume3 must not match Volume31
        assert!(map_device_path("\\Device\\HarddiskVolume31\\x", &m).is_none());
    }

    #[test]
    fn image_paths() {
        let r = "C:\\Windows";
        assert_eq!(clean_image_path_with("\"C:\\Program Files\\G\\up.exe\" --system --windows-service", r), "C:\\Program Files\\G\\up.exe");
        assert_eq!(clean_image_path_with("\\SystemRoot\\System32\\drivers\\x.sys", r), "C:\\Windows\\System32\\drivers\\x.sys");
        assert_eq!(clean_image_path_with("System32\\drivers\\y.sys", r), "C:\\Windows\\System32\\drivers\\y.sys");
        assert_eq!(clean_image_path_with("\\??\\C:\\Drv\\z.sys", r), "C:\\Drv\\z.sys");
        assert_eq!(clean_image_path_with("C:\\Windows\\system32\\svchost.exe -k netsvcs -p", r), "C:\\Windows\\system32\\svchost.exe");
    }

    #[test]
    fn legacy_env_names_are_still_read() {
        const S: &str = "RENAME_GUARD_PROBE";
        std::env::remove_var("HAYATE_RENAME_GUARD_PROBE");
        std::env::set_var("BLINK_RENAME_GUARD_PROBE", "old");
        assert_eq!(env_var(S).as_deref(), Ok("old"));
        assert_eq!(env_var_os(S).as_deref(), Some(std::ffi::OsStr::new("old")));
        std::env::set_var("HAYATE_RENAME_GUARD_PROBE", "new");
        assert_eq!(env_var(S).as_deref(), Ok("new"));
        assert_eq!(env_var_os(S).as_deref(), Some(std::ffi::OsStr::new("new")), "HAYATE_ wins");
        std::env::remove_var("HAYATE_RENAME_GUARD_PROBE");
        std::env::remove_var("BLINK_RENAME_GUARD_PROBE");
        assert!(env_var(S).is_err() && env_var_os(S).is_none());
    }

    #[test]
    fn tests_never_use_the_real_data_dir() {
        if env_var_os("DATA_DIR").is_none() {
            let d = data_dir();
            assert!(d.starts_with(std::env::temp_dir()), "{d:?}");
            assert!(d.file_name().unwrap().to_string_lossy().starts_with("hayate-test-"));
        }
    }

    #[test]
    fn filetime() {
        assert_eq!(filetime_to_ms(116_444_736_000_000_000 + 10_000), 1);
    }
}

/// Tiny diagnostic log: %LOCALAPPDATA%\Hayate\hayate.log, reset when > 256 KB.
pub fn log(line: &str) {
    let line = format!("{} {}
", now_unix(), line);
    // a disk that is 100 % busy must not stall the caller (often the UI or the
    // sampler): once the I/O worker runs, the write goes through its queue
    if crate::bgio::running() {
        crate::bgio::submit_fast("log", move || log_now(&line));
    } else {
        log_now(&line);
    }
}

fn log_now(line: &str) {
    use std::io::Write;
    if NO_LOG.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let p = data_dir().join("hayate.log");
    if std::fs::metadata(&p).map(|m| m.len() > 256 * 1024).unwrap_or(false) {
        let _ = std::fs::remove_file(&p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Is this process elevated? `None` when the token cannot be read: callers that
/// gate a security decision (the startup manager) must treat `None` as "do not",
/// never as "normal user". Cached once it is known.
pub fn elevation() -> Option<bool> {
    static E: OnceLock<Option<bool>> = OnceLock::new();
    *E.get_or_init(|| unsafe {
        use windows::Win32::System::Threading::GetCurrentProcess;
        token_elevated(GetCurrentProcess())
    })
}

/// True when this process runs with an elevated (admin) token. Cached. An
/// unreadable token counts as "not elevated" HERE (UI hints and legacy callers);
/// anything that starts programs or changes machine state uses [`elevation`].
pub fn is_elevated() -> bool {
    elevation().unwrap_or(false)
}

/// Is process `pid` elevated? None when its token cannot be read (then callers
/// must not assume either way).
pub fn process_elevated(pid: u32) -> Option<bool> {
    unsafe {
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let r = token_elevated(h);
        let _ = CloseHandle(h);
        r
    }
}

unsafe fn token_elevated(process: HANDLE) -> Option<bool> {
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::OpenProcessToken;
    let mut tok = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut tok).ok()?;
    let mut e = TOKEN_ELEVATION::default();
    let mut n = 0u32;
    let ok = GetTokenInformation(tok, TokenElevation, Some(&mut e as *mut _ as *mut _), std::mem::size_of::<TOKEN_ELEVATION>() as u32, &mut n).is_ok();
    let _ = CloseHandle(tok);
    ok.then_some(e.TokenIsElevated != 0)
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use windows::Win32::System::Threading::{PROCESS_ACCESS_RIGHTS, PROCESS_TERMINATE};

    fn sleeper() -> (std::process::Child, i64) {
        let c = crate::osutil::quiet_cmd("ping").args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        let create = identity(c.id()).expect("child identity").0;
        (c, create)
    }

    #[test]
    fn open_verified_names_the_reason() {
        let (mut c, create) = sleeper();
        let none = PROCESS_ACCESS_RIGHTS(0);
        assert!(open_verified(c.id(), create, PROCESS_TERMINATE).is_ok());
        assert_eq!(open_verified(c.id(), create + 1, none).unwrap_err(), OpenFail::Gone, "PID names another process");
        assert_eq!(open_verified(c.id(), 0, none).unwrap_err(), OpenFail::NoIdentity);
        assert_eq!(open_verified(4, create, none).unwrap_err(), OpenFail::NoIdentity);
        let (cr, path) = identity(c.id()).unwrap();
        assert_eq!(cr, create, "identity reads the same creation time");
        assert!(path.is_some_and(|p| p.to_lowercase().ends_with("ping.exe")));
        let _ = c.kill();
        let _ = c.wait();
        assert_eq!(open_verified(c.id(), create, none).unwrap_err(), OpenFail::Gone, "exited");
    }

    #[test]
    fn a_held_handle_pins_the_pid() {
        // the handle that was verified is the one used: it keeps naming the same
        // process after that process is killed, never a replacement
        let (mut c, create) = sleeper();
        let h = open_verified(c.id(), create, PROCESS_TERMINATE).unwrap();
        let _ = c.kill();
        let _ = c.wait();
        assert_eq!(create_time_of(h.0), Some(create));
    }
}

/// Test fixtures: a child process that never flashes a console window.
#[cfg(test)]
pub fn quiet_cmd(prog: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new(prog);
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}
