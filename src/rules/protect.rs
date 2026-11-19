//! Safeguards for "Disallow running": processes Hayate must never end, however a
//! rule is written (hand-edited rules.ini, a pasted rule, a bug in the UI).
//!
//! Layers, all of which must pass (there is no override, no setting and no
//! "force" path):
//! 1. static, from the live image path and PID: PID <= 4, Hayate itself, any
//!    Hayate program (own file, `hayate*.exe` helpers, update and parked copies),
//!    anything inside the Windows folder or the Defender folders, and a list of
//!    essential system program names wherever they sit on disk;
//! 2. the OS's own answer, `IsProcessCritical` on the retained handle: ending a
//!    critical process crashes Windows. If the OS cannot answer, the process is
//!    refused (fail closed);
//! 3. the session: services and other session-0 processes are never ended. If
//!    the session cannot be read, the process is refused.
//!
//! Layer 1 is pure ([`static_check`], and [`path_check`] for a name or path alone)
//! so it is unit-tested with synthetic paths; [`check`] adds layers 2 and 3 on a
//! handle that was verified (PID + creation time) by the caller.
//!
//! The Windows folder comes from `GetWindowsDirectoryW` and the Program Files /
//! ProgramData folders from the known-folder API, never from environment variables
//! (a process can set those). Every path is canonicalised before it is compared:
//! `\\?\`, `\\.\`, `\??\` prefixes, `/`, doubled separators, `.` and `..`, trailing
//! dots and spaces, and 8.3 short names where the file exists.

use std::sync::OnceLock;

use windows::core::BOOL;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Threading::IsProcessCritical;
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86, KF_FLAG_DEFAULT};

use crate::osutil;

/// Why a process is never ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    SystemPid,
    OwnProcess,
    HayateProgram,
    WindowsFolder,
    SecurityTool,
    EssentialName,
    NoImage,
    Critical,
    CriticalUnknown(String),
    ServiceSession,
    SessionUnknown,
}

impl Refusal {
    /// One line for the Rules tab, the panel and the log.
    pub fn text(&self) -> String {
        match self {
            Refusal::SystemPid => "refused: the System process is never ended".into(),
            Refusal::OwnProcess => "refused: Hayate never ends itself".into(),
            Refusal::HayateProgram => "refused: Hayate never ends its own helper, updater or other Hayate program".into(),
            Refusal::WindowsFolder => "refused: programs in the Windows folder are never ended".into(),
            Refusal::SecurityTool => "refused: Windows security programs are never ended".into(),
            Refusal::EssentialName => "refused: essential Windows program, never ended".into(),
            Refusal::NoImage => "refused: the program file of this process cannot be read".into(),
            Refusal::Critical => "refused: Windows marks this process critical (ending it would crash Windows)".into(),
            Refusal::CriticalUnknown(e) => format!("refused: Windows would not say whether this process is critical ({e})"),
            Refusal::ServiceSession => "refused: services and other session-0 processes are never ended".into(),
            Refusal::SessionUnknown => "refused: the session of this process cannot be read".into(),
        }
    }
}

/// Program names that are never ended wherever the file sits (a copy outside
/// the Windows folder with one of these names is refused too: cheap, and a
/// rule for such a name is never what anyone means).
const ESSENTIAL: &[&str] = &[
    "system", "registry", "smss.exe", "csrss.exe", "wininit.exe", "winlogon.exe", "services.exe", "lsass.exe", "lsaiso.exe", "lsm.exe", "svchost.exe", "dwm.exe",
    "fontdrvhost.exe", "sihost.exe", "explorer.exe", "taskmgr.exe", "ctfmon.exe", "audiodg.exe", "spoolsv.exe", "wudfhost.exe", "dllhost.exe", "conhost.exe",
    "runtimebroker.exe", "taskhostw.exe", "searchhost.exe", "searchindexer.exe", "startmenuexperiencehost.exe", "shellexperiencehost.exe", "textinputhost.exe",
    "applicationframehost.exe", "lockapp.exe", "logonui.exe", "userinit.exe", "consent.exe", "werfault.exe", "msmpeng.exe", "nissrv.exe", "securityhealthservice.exe",
    "securityhealthsystray.exe", "smartscreen.exe", "mpdefendercoreservice.exe", "msmpeng", "wmiprvse.exe", "trustedinstaller.exe", "tiworker.exe",
];

/// Everything the static layer compares against, so tests can inject their own.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub own_pid: u32,
    /// normalized path of Hayate's own exe
    pub own_exe: String,
    /// normalized directories whose programs are never ended
    pub dirs: Vec<(String, Refusal)>,
}

impl Ctx {
    pub fn system() -> &'static Ctx {
        static C: OnceLock<Ctx> = OnceLock::new();
        C.get_or_init(|| {
            let mut dirs: Vec<(String, Refusal)> = Vec::new();
            // Windows' own answer; the C:\Windows fallback only when the call fails
            let root = windows_dir().unwrap_or_else(|| "C:\\Windows".into());
            dirs.push((norm(&root), Refusal::WindowsFolder));
            dirs.push((norm(&long_form(&root)), Refusal::WindowsFolder));
            for id in [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86, &FOLDERID_ProgramFilesX64] {
                if let Some(p) = known_folder(id) {
                    for sub in ["Windows Defender", "Windows Defender Advanced Threat Protection", "Windows Security"] {
                        dirs.push((norm(&format!("{p}\\{sub}")), Refusal::SecurityTool));
                    }
                }
            }
            if let Some(p) = known_folder(&FOLDERID_ProgramData) {
                for sub in ["Microsoft\\Windows Defender", "Microsoft\\Windows Security Health"] {
                    dirs.push((norm(&format!("{p}\\{sub}")), Refusal::SecurityTool));
                }
            }
            dirs.sort_by(|a, b| a.0.cmp(&b.0));
            dirs.dedup_by(|a, b| a.0 == b.0);
            let own_exe = std::env::current_exe().map(|p| norm(&long_form(&p.to_string_lossy()))).unwrap_or_default();
            Ctx { own_pid: std::process::id(), own_exe, dirs }
        })
    }
}

extern "system" {
    fn GetWindowsDirectoryW(buf: *mut u16, size: u32) -> u32;
    fn GetLongPathNameW(short: *const u16, long: *mut u16, size: u32) -> u32;
}

/// The Windows folder as Windows reports it (not `%SystemRoot%`).
pub fn windows_dir() -> Option<String> {
    let mut buf = [0u16; 520];
    let n = unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 2 && n < buf.len()).then(|| String::from_utf16_lossy(&buf[..n]))
}

fn known_folder(id: &windows::core::GUID) -> Option<String> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.filter(|s| !s.is_empty())
    }
}

/// Expand 8.3 short names (`C:\PROGRA~1`) when the file or folder exists; otherwise the
/// path unchanged (it is canonicalised the same way, so nothing is lost).
pub fn long_form(path: &str) -> String {
    let w = osutil::wide(path);
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetLongPathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        path.to_string()
    } else {
        String::from_utf16_lossy(&buf[..n])
    }
}

/// Canonical comparison form: lower case, `/` → `\`, no `\\?\`, `\\?\UNC\`, `\\.\` or `\??\`
/// prefix, doubled separators collapsed, `.` and `..` resolved (never above the drive or
/// share root), trailing dots and spaces dropped from each component (Windows ignores
/// them), no trailing `\`. Comparison form only.
pub fn norm(p: &str) -> String {
    let mut p = p.trim().replace('/', "\\");
    let lower = p.to_lowercase();
    let mut unc = false;
    if let Some(pre) = ["\\\\?\\unc\\", "\\\\.\\unc\\"].into_iter().find(|pre| lower.starts_with(pre)) {
        p = p[pre.len()..].to_string();
        unc = true;
    } else if let Some(pre) = ["\\\\?\\", "\\\\.\\", "\\??\\"].into_iter().find(|pre| lower.starts_with(pre)) {
        p = p[pre.len()..].to_string();
    } else {
        unc = p.starts_with("\\\\");
    }
    let mut out: Vec<String> = Vec::new();
    for raw in p.split('\\') {
        match raw {
            "" | "." => continue,
            ".." => {
                // the first component is the drive / server: never climb above it
                if out.len() > 1 {
                    out.pop();
                }
                continue;
            }
            _ => {}
        }
        let c = raw.trim_end_matches(['.', ' ']);
        if !c.is_empty() {
            out.push(c.to_lowercase());
        }
    }
    let body = out.join("\\");
    if unc {
        format!("\\\\{body}")
    } else {
        body
    }
}

fn under(path: &str, dir: &str) -> bool {
    !dir.is_empty() && path.len() > dir.len() && path.starts_with(dir) && path.as_bytes()[dir.len()] == b'\\'
}

/// The PID-independent part of layer 1: judge a program by its file name and folder alone.
/// [`static_check`] uses it on a live image, [`pattern_refusal`] on a rule's pattern (no
/// process, so no PID is involved).
pub fn path_check(ctx: &Ctx, path: Option<&str>) -> Result<(), Refusal> {
    let Some(path) = path.map(str::trim).filter(|p| !p.is_empty()) else { return Err(Refusal::NoImage) };
    let n = norm(path);
    if n.is_empty() {
        return Err(Refusal::NoImage);
    }
    if !ctx.own_exe.is_empty() && n == ctx.own_exe {
        return Err(Refusal::OwnProcess);
    }
    let name = osutil::file_name(&n);
    // hayate.exe, hayate.new.exe, hayate-1234.exe (parked), hayate-multi copies...
    let own_name = osutil::file_name(&ctx.own_exe);
    if name.starts_with("hayate") || (!own_name.is_empty() && name == own_name) {
        return Err(Refusal::HayateProgram);
    }
    for (dir, why) in &ctx.dirs {
        if under(&n, dir) {
            return Err(why.clone());
        }
    }
    if ESSENTIAL.contains(&name) {
        return Err(Refusal::EssentialName);
    }
    Ok(())
}

/// Layer 1 (pure). `path` is the live Win32 image path of the process.
pub fn static_check(ctx: &Ctx, pid: u32, path: Option<&str>) -> Result<(), Refusal> {
    if pid <= 4 {
        return Err(Refusal::SystemPid);
    }
    if pid == ctx.own_pid {
        return Err(Refusal::OwnProcess);
    }
    path_check(ctx, path)
}

/// Can a Disallow rule for this exe name / exact path ever act? `Some` = never (it would
/// only match programs the static layer always refuses), so the rule is not created, or is
/// dropped at load. A bare name is judged as if it sat in an ordinary folder (only the name
/// list and the Hayate names apply); a path is judged fully, 8.3 short names expanded when
/// the file exists. Pure name/path protection: no process and no PID is involved.
pub fn pattern_refusal(pattern: &str) -> Option<Refusal> {
    pattern_refusal_in(Ctx::system(), pattern)
}

pub fn pattern_refusal_in(ctx: &Ctx, pattern: &str) -> Option<Refusal> {
    let p = pattern.trim();
    if p.is_empty() {
        return Some(Refusal::NoImage);
    }
    let as_path = if p.contains(['\\', '/']) { long_form(p) } else { format!("Z:\\zz-ordinary-folder\\{p}") };
    path_check(ctx, Some(&as_path)).err()
}

extern "system" {
    fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
}

/// Session id of `pid` (None when Windows will not say).
pub fn session_of(pid: u32) -> Option<u32> {
    let mut s = 0u32;
    (unsafe { ProcessIdToSessionId(pid, &mut s) } != 0).then_some(s)
}

/// Layers 2 and 3 on `h` (needs `PROCESS_QUERY_LIMITED_INFORMATION`).
pub fn os_check(h: HANDLE, pid: u32) -> Result<(), Refusal> {
    let mut critical = BOOL(0);
    if let Err(e) = unsafe { IsProcessCritical(h, &mut critical) } {
        return Err(Refusal::CriticalUnknown(e.message()));
    }
    if critical.as_bool() {
        return Err(Refusal::Critical);
    }
    match session_of(pid) {
        Some(0) => Err(Refusal::ServiceSession),
        Some(_) => Ok(()),
        None => Err(Refusal::SessionUnknown),
    }
}

/// Every layer, for a process whose handle `h` was verified (PID + creation time)
/// and whose live image is `path`. Ok means "may be ended if a rule says so".
pub fn check(h: HANDLE, pid: u32, path: Option<&str>) -> Result<(), Refusal> {
    static_check(Ctx::system(), pid, path)?;
    // the same image with 8.3 short names expanded (a program started through a short path)
    static_check(Ctx::system(), pid, path.map(long_form).as_deref())?;
    os_check(h, pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            own_pid: 4242,
            own_exe: norm(r"C:\Users\Me\AppData\Local\Programs\Hayate\hayate.exe"),
            dirs: vec![(norm(r"C:\Windows"), Refusal::WindowsFolder), (norm(r"C:\Program Files\Windows Defender"), Refusal::SecurityTool)],
        }
    }

    #[test]
    fn system_pids_and_self_are_refused_first() {
        let c = ctx();
        for pid in [0, 1, 2, 3, 4] {
            assert_eq!(static_check(&c, pid, Some(r"C:\x\game.exe")), Err(Refusal::SystemPid));
        }
        assert_eq!(static_check(&c, 4242, Some(r"C:\x\game.exe")), Err(Refusal::OwnProcess));
    }

    #[test]
    fn hayate_programs_are_refused_wherever_they_sit() {
        let c = ctx();
        assert_eq!(static_check(&c, 900, Some(r"C:\Users\Me\AppData\Local\Programs\Hayate\hayate.exe")), Err(Refusal::OwnProcess));
        for p in [
            r"D:\copy\hayate.exe",
            r"C:\Users\Me\AppData\Local\Temp\Hayate-uninstalled\hayate-77.exe",
            r"C:\Users\Me\AppData\Local\Programs\Hayate\hayate.new.exe",
            r"E:\t\Hayate-Multi.exe",
            r"\\?\C:\Downloads\HAYATE.EXE",
        ] {
            assert_eq!(static_check(&c, 900, Some(p)), Err(Refusal::HayateProgram), "{p}");
        }
    }

    #[test]
    fn windows_folder_and_security_folders_are_refused_for_any_name() {
        let c = ctx();
        for p in [r"C:\Windows\System32\anything.exe", r"c:\windows\SysWOW64\x.exe", r"C:\WINDOWS\SystemApps\X\y.exe", r"\\?\C:\Windows\notepad.exe", "C:/Windows/x.exe"] {
            assert_eq!(static_check(&c, 900, Some(p)), Err(Refusal::WindowsFolder), "{p}");
        }
        assert_eq!(static_check(&c, 900, Some(r"C:\Program Files\Windows Defender\MsMpEng.exe")), Err(Refusal::SecurityTool));
        // a lookalike folder is not the Windows folder
        assert_eq!(static_check(&c, 900, Some(r"C:\Windows.old\x\game.exe")), Ok(()));
        assert_eq!(static_check(&c, 900, Some(r"C:\Windowsgame\game.exe")), Ok(()));
    }

    #[test]
    fn essential_names_are_refused_outside_the_windows_folder_too() {
        let c = ctx();
        for n in ["csrss.exe", "LSASS.EXE", "svchost.exe", "winlogon.exe", "explorer.exe", "dwm.exe", "MsMpEng.exe", "services.exe", "smss.exe", "wininit.exe"] {
            assert_eq!(static_check(&c, 900, Some(&format!(r"D:\tools\{n}"))), Err(Refusal::EssentialName), "{n}");
        }
    }

    #[test]
    fn rules_that_could_only_ever_be_refused_are_not_creatable() {
        assert_eq!(pattern_refusal("csrss.exe"), Some(Refusal::EssentialName));
        assert_eq!(pattern_refusal("HAYATE.exe"), Some(Refusal::HayateProgram));
        assert!(matches!(pattern_refusal(r"C:\Windows\System32\notepad.exe"), Some(Refusal::WindowsFolder)));
        assert_eq!(pattern_refusal("zz_hayate_disallow_fixture.exe"), None);
        assert_eq!(pattern_refusal(r"D:\Games\game.exe"), None);
    }

    #[test]
    fn path_forms_all_reach_the_same_verdict() {
        let c = ctx();
        for p in [
            r"C:\Windows\System32\x.exe",
            r"c:\WINDOWS\system32\X.EXE",
            "C:/Windows/System32/x.exe",
            r"\\?\C:\Windows\System32\x.exe",
            r"\\.\C:\Windows\System32\x.exe",
            r"\??\C:\Windows\System32\x.exe",
            r"C:\Windows\\System32\\x.exe",
            r"C:\Windows\.\System32\x.exe",
            r"C:\Games\..\Windows\System32\x.exe",
            r"C:\Windows.\System32\x.exe",
            r"C:\Windows \System32\x.exe",
            r"C:\Windows\System32\x.exe.",
            r"C:\Windows\System32\..\..\Windows\x.exe",
        ] {
            assert_eq!(path_check(&c, Some(p)), Err(Refusal::WindowsFolder), "{p}");
        }
        // too many `..` stop at the drive root: no wrap-around into another folder
        assert_eq!(path_check(&c, Some(r"C:\..\..\..\Windows\x.exe")), Err(Refusal::WindowsFolder));
        assert_eq!(path_check(&c, Some(r"C:\Games\..\Games\g.exe")), Ok(()));
        // a trailing dot or space cannot hide an essential or Hayate name
        assert_eq!(path_check(&c, Some(r"D:\t\svchost.exe.")), Err(Refusal::EssentialName));
        assert_eq!(path_check(&c, Some(r"D:\t\svchost.exe ")), Err(Refusal::EssentialName));
        assert_eq!(path_check(&c, Some(r"D:\t\Hayate.exe.")), Err(Refusal::HayateProgram));
        // UNC names are kept apart from drive paths
        assert_eq!(norm(r"\\?\UNC\srv\share\a.exe"), r"\\srv\share\a.exe");
        assert_eq!(norm(r"\\srv\share\a.exe"), r"\\srv\share\a.exe");
        assert_eq!(path_check(&c, Some(r"\\srv\share\Windows\x.exe")), Ok(()));
    }

    #[test]
    fn rule_patterns_are_judged_by_name_and_path_never_by_a_pid() {
        // pure name/path protection: the answer cannot depend on any PID, not even one equal to ours
        let mut c = ctx();
        c.own_pid = 100;
        assert_eq!(pattern_refusal_in(&c, "game.exe"), None);
        assert_eq!(pattern_refusal_in(&c, r"D:\Games\game.exe"), None);
        assert_eq!(pattern_refusal_in(&c, "svchost.exe"), Some(Refusal::EssentialName));
        assert_eq!(pattern_refusal_in(&c, "svchost.exe."), Some(Refusal::EssentialName));
        assert_eq!(pattern_refusal_in(&c, "hayate-x.exe"), Some(Refusal::HayateProgram));
        assert_eq!(pattern_refusal_in(&c, r"C:\Windows\.\System32\x.exe"), Some(Refusal::WindowsFolder));
        assert_eq!(pattern_refusal_in(&c, r"C:/Windows/System32/x.exe"), Some(Refusal::WindowsFolder));
        assert_eq!(pattern_refusal_in(&c, r"\\?\C:\Windows\x.exe"), Some(Refusal::WindowsFolder));
        assert_eq!(pattern_refusal_in(&c, ""), Some(Refusal::NoImage));
        assert_eq!(pattern_refusal_in(&c, r"C:\Program Files\Windows Defender\x.exe"), Some(Refusal::SecurityTool));
    }

    #[test]
    fn the_windows_folder_comes_from_the_os_not_from_environment_variables() {
        let from_os = windows_dir().expect("GetWindowsDirectoryW");
        assert!(Ctx::system().dirs.iter().any(|(d, why)| *why == Refusal::WindowsFolder && *d == norm(&from_os)), "{from_os} in {:?}", Ctx::system().dirs);
        // (the source reads no environment variable at all: nothing a process sets can move it)
        assert_eq!(path_check(Ctx::system(), Some(&format!(r"{from_os}\System32\anything.exe"))), Err(Refusal::WindowsFolder));
    }

    #[test]
    fn short_names_of_the_windows_folder_are_expanded_when_the_file_exists() {
        let from_os = windows_dir().unwrap();
        let long = long_form(&format!(r"{from_os}\System32\cmd.exe"));
        assert!(long.to_lowercase().ends_with(r"\system32\cmd.exe"), "{long}");
        // a path that does not exist is returned unchanged, never emptied
        assert_eq!(long_form(r"Z:\no\such\thing.exe"), r"Z:\no\such\thing.exe");
        // PROGRA~1-style names resolve to the real folder, which `check` then refuses by dir
        let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
        let short = format!(r"{}\PROGRA~1\Windows Defender\MsMpEng.exe", pf.split('\\').next().unwrap());
        if std::path::Path::new(&short).exists() {
            assert_eq!(path_check(Ctx::system(), Some(&long_form(&short))), Err(Refusal::SecurityTool));
        }
    }

    #[test]
    fn unreadable_image_fails_closed_and_ordinary_programs_pass() {
        let c = ctx();
        assert_eq!(static_check(&c, 900, None), Err(Refusal::NoImage));
        assert_eq!(static_check(&c, 900, Some("  ")), Err(Refusal::NoImage));
        assert_eq!(static_check(&c, 900, Some(r"C:\Games\Game\game.exe")), Ok(()));
        assert_eq!(static_check(&c, 900, Some(r"C:\src\hayateless-fixture\zz_hayate_disallow_fixture.exe")), Ok(()));
    }

    #[test]
    fn the_real_context_protects_the_test_process_and_the_windows_folder() {
        let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        assert!(static_check(Ctx::system(), std::process::id(), Some(&me)).is_err());
        let sysroot = windows_dir().unwrap();
        assert_eq!(static_check(Ctx::system(), 900, Some(&format!(r"{sysroot}\System32\cmd.exe"))), Err(Refusal::WindowsFolder));
    }

    #[test]
    fn os_layer_sees_a_normal_child_as_endable_and_this_session_as_not_zero() {
        let mut c = crate::osutil::quiet_cmd("ping").args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        let (create, path) = osutil::identity(c.id()).unwrap();
        let h = osutil::open_verified(c.id(), create, windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION).unwrap();
        assert_eq!(os_check(h.0, c.id()), Ok(()));
        assert!(path.is_some());
        let _ = c.kill();
        let _ = c.wait();
    }

    #[test]
    fn os_layer_agrees_with_windows_about_every_process_it_can_open() {
        // Read-only: ask Windows about every process this token can open, and require the
        // layer to refuse exactly the critical and session-0 ones (never a plain user process).
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let (mut open, mut refused, mut critical_seen, mut service_seen) = (0, 0, 0, 0);
        for (pid, _) in super::super::running() {
            let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else { continue };
            let h = osutil::Handle(h);
            let mut crit = BOOL(0);
            if unsafe { IsProcessCritical(h.0, &mut crit) }.is_err() {
                continue;
            }
            let Some(sess) = session_of(pid) else { continue };
            open += 1;
            let r = os_check(h.0, pid);
            let want_refused = crit.as_bool() || sess == 0;
            assert_eq!(r.is_err(), want_refused, "pid {pid}: critical={} session={sess}: {r:?}", crit.as_bool());
            if crit.as_bool() {
                assert_eq!(r, Err(Refusal::Critical));
                critical_seen += 1;
            } else if sess == 0 {
                assert_eq!(r, Err(Refusal::ServiceSession));
                service_seen += 1;
            }
            refused += r.is_err() as u32;
        }
        eprintln!("os layer: {open} openable processes, {refused} refused ({critical_seen} critical, {service_seen} session-0)");
        assert!(open > 5, "too few processes to mean anything");
    }
}
