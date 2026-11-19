//! One-shot process actions: trim, restart (unelevated / as admin), close
//! windows, locate, and an unelevated launcher Blink can reuse (Startup tab).

use windows::core::{BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Security::*;
use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::osutil::{self, wide, Handle, OpenFail};

fn open(pid: u32, rights: PROCESS_ACCESS_RIGHTS) -> Result<Handle, String> {
    unsafe { OpenProcess(rights, false, pid) }.map(Handle).map_err(|e| {
        if e.code() == ERROR_ACCESS_DENIED.to_hresult() {
            "access denied (protected)".to_string()
        } else if e.code() == ERROR_INVALID_PARAMETER.to_hresult() {
            "process exited".to_string()
        } else {
            e.message()
        }
    })
}

/// Open `pid` for an action, proven to still be the process the list showed
/// (`create` = its creation time). Every call the action makes goes through this
/// one handle, which also keeps the PID from being reused while the action runs.
fn open_target(pid: u32, create: i64, rights: PROCESS_ACCESS_RIGHTS) -> Result<Handle, String> {
    osutil::open_verified(pid, create, rights).map_err(|e| match e {
        OpenFail::Denied => "access denied (protected)".to_string(),
        OpenFail::Gone => "process exited (or its PID was reused)".to_string(),
        OpenFail::NoIdentity => "start time unknown; refresh and try again".to_string(),
        OpenFail::Other(m) => m,
    })
}

/// Empty the working set once (pages fault back in on use). Verified against `create`.
pub fn trim(pid: u32, create: i64) -> Result<(), String> {
    let h = open_target(pid, create, PROCESS_SET_QUOTA)?;
    unsafe { EmptyWorkingSet(h.0) }.map_err(|e| e.message())
}

/// Start `cmdline` unelevated. Returns the new pid, or 0 when Explorer started
/// it (the shell does not report a pid).
///
/// Elevated Blink asks Explorer (already unelevated) to run it through
/// IShellDispatch2::ShellExecute: no token APIs, no privileges, the documented
/// approach. Only if that fails does it fall back to Explorer's token.
pub fn launch_unelevated(cmdline: &str, cwd: Option<&str>) -> Result<u32, String> {
    // Not elevated: a normal child already runs unelevated.
    // only a PROVEN normal token may start a plain child; an unreadable one takes the Explorer route
    if osutil::elevation() == Some(false) {
        return launch_elevated(cmdline, cwd);
    }
    let (file, args) = split_cmd(cmdline);
    match explorer_exec(&file, &args, cwd) {
        Ok(()) => Ok(0),
        Err(e) => {
            osutil::log(&format!("explorer launch failed: {e}; using shell token"));
            launch_with_shell_token(cmdline, cwd)
        }
    }
}

/// "\"C:\\a b\\x.exe\" -n 1" → ("C:\\a b\\x.exe", "-n 1")
fn split_cmd(cmd: &str) -> (String, String) {
    let s = cmd.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        match rest.find('"') {
            Some(i) => (rest[..i].to_string(), rest[i + 1..].trim_start().to_string()),
            None => (rest.to_string(), String::new()),
        }
    } else {
        match s.find(char::is_whitespace) {
            Some(i) => (s[..i].to_string(), s[i..].trim_start().to_string()),
            None => (s.to_string(), String::new()),
        }
    }
}

/// Desktop folder view → its Shell.Application → ShellExecute, all inside
/// Explorer. Runs on its own STA thread so the caller's COM state is irrelevant.
fn explorer_exec(file: &str, args: &str, cwd: Option<&str>) -> Result<(), String> {
    use windows::core::{Interface, BSTR};
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Shell::*;
    let (file, args, cwd) = (file.to_string(), args.to_string(), cwd.unwrap_or("").to_string());
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let r = (|| -> windows::core::Result<()> {
            let sw: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
            let loc = VARIANT::from(0i32); // CSIDL_DESKTOP
            let root = VARIANT::default();
            let mut hwnd = 0i32;
            let disp = sw.FindWindowSW(&loc, &root, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH)?;
            let sp: IServiceProvider = disp.cast()?;
            let browser: IShellBrowser = sp.QueryService(&SID_STopLevelBrowser)?;
            let view = browser.QueryActiveShellView()?;
            let bg: IDispatch = view.GetItemObject(SVGIO_BACKGROUND)?;
            let fvd: IShellFolderViewDual = bg.cast()?;
            let sd: IShellDispatch2 = fvd.Application()?.cast()?;
            sd.ShellExecute(
                &BSTR::from(file.as_str()),
                &VARIANT::from(BSTR::from(args.as_str())),
                &VARIANT::from(BSTR::from(cwd.as_str())),
                &VARIANT::from(BSTR::new()),
                &VARIANT::from(1i32), // SW_SHOWNORMAL
            )
        })();
        CoUninitialize();
        r.map_err(|e| e.message())
    })
    .join()
    .map_err(|_| "explorer launch thread panicked".to_string())?
}

/// What must hold for a token to count as "the normal interactive user token".
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TokenFacts {
    /// the user SID, byte for byte
    pub user: Vec<u8>,
    pub session: u32,
    pub elevated: bool,
}

/// The shell's token may be used to start a program for the user only when it is
/// the same user, in the same session, and NOT elevated. Anything else (a
/// different user, an elevated Explorer, a token we cannot read) fails closed:
/// nothing is launched rather than launched with more rights than the user's
/// normal desktop has.
pub(crate) fn check_shell_token(own: &TokenFacts, shell: &TokenFacts) -> Result<(), String> {
    if own.user != shell.user {
        return Err("the shell runs as a different user".into());
    }
    if own.session != shell.session {
        return Err("the shell runs in a different session".into());
    }
    if shell.elevated {
        return Err("the shell itself is elevated, so no unelevated token is available".into());
    }
    Ok(())
}

unsafe fn token_facts(tok: HANDLE) -> Option<TokenFacts> {
    fn fixed<T>(tok: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Option<T> {
        unsafe {
            let mut v: T = std::mem::zeroed();
            let mut n = 0u32;
            GetTokenInformation(tok, class, Some(&mut v as *mut T as *mut _), std::mem::size_of::<T>() as u32, &mut n).ok()?;
            Some(v)
        }
    }
    let mut n = 0u32;
    let _ = GetTokenInformation(tok, TokenUser, None, 0, &mut n);
    if n == 0 {
        return None;
    }
    let mut buf = vec![0u8; n as usize];
    GetTokenInformation(tok, TokenUser, Some(buf.as_mut_ptr() as *mut _), n, &mut n).ok()?;
    let tu = std::ptr::read_unaligned(buf.as_ptr() as *const TOKEN_USER);
    let len = GetLengthSid(tu.User.Sid) as usize;
    if len == 0 || len > 68 {
        return None;
    }
    let user = std::slice::from_raw_parts(tu.User.Sid.0 as *const u8, len).to_vec();
    let session: u32 = fixed(tok, TokenSessionId)?;
    let el: TOKEN_ELEVATION = fixed(tok, TokenElevation)?;
    Some(TokenFacts { user, session, elevated: el.TokenIsElevated != 0 })
}

/// The shell's primary token, only if [`check_shell_token`] accepts it.
fn verified_shell_token() -> Result<Handle, String> {
    let prim = shell_token()?;
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut own).map_err(|e| format!("own token: {}", e.message()))?;
        let own = Handle(own);
        let (o, s) = (token_facts(own.0).ok_or("cannot read this process's token")?, token_facts(prim.0).ok_or("cannot read the shell's token")?);
        check_shell_token(&o, &s)?;
    }
    Ok(prim)
}

/// How a program is started for the user. Chosen from Blink's own elevation and
/// the kind of target alone, so it can be unit-tested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserWay {
    /// Blink is not elevated: a child of Blink already has the user's normal token
    Direct,
    /// Blink is not elevated, target is a shortcut / shell item: the shell opens it
    ShellOpen,
    /// Blink is elevated: Explorer starts it; fallback only the verified shell token.
    /// Never CreateProcess with Blink's own token.
    ThroughExplorer,
}

pub(crate) fn user_way(blink_elevated: bool, shell_item: bool) -> UserWay {
    match (blink_elevated, shell_item) {
        (true, _) => UserWay::ThroughExplorer,
        (false, true) => UserWay::ShellOpen,
        (false, false) => UserWay::Direct,
    }
}

/// [`user_way`] for an elevation answer that may be missing. A token that could
/// not be read is NOT "normal": a direct CreateProcess would then pass on a
/// possibly elevated token, so nothing is started.
pub(crate) fn user_way_for(elevation: Option<bool>, shell_item: bool) -> Result<UserWay, String> {
    match elevation {
        Some(e) => Ok(user_way(e, shell_item)),
        None => Err("Hayate could not read its own token, so it cannot prove the program would start with your normal rights. Nothing was started.".into()),
    }
}

/// Start `file args` as the normal interactive user and say how.
///
/// Fails closed: when the unelevated start cannot be proven, nothing runs and
/// the error says why. `shell_item` = a `.lnk` / `.url` / `shell:` target that
/// the shell must open (CreateProcess cannot run it).
pub fn launch_as_user(file: &str, args: &str, cwd: Option<&str>, shell_item: bool) -> Result<String, String> {
    match user_way_for(osutil::elevation(), shell_item)? {
        UserWay::Direct => {
            let line = if args.is_empty() { format!("\"{file}\"") } else { format!("\"{file}\" {args}") };
            launch_elevated(&line, cwd).map(|pid| format!("(pid {pid})"))
        }
        UserWay::ShellOpen => crate::startup::launch::shell_open(file, args, cwd).map(|_| "via the shell".to_string()),
        UserWay::ThroughExplorer => match explorer_exec(file, args, cwd) {
            Ok(()) => Ok("via Explorer (unelevated)".into()),
            Err(e) => {
                osutil::log(&format!("startup: explorer launch failed: {e}; trying the verified shell token"));
                let line = if shell_item {
                    let sys = crate::osutil::windows_dir()?;
                    let target = if args.is_empty() { format!("\"{file}\"") } else { format!("\"{file}\" {args}") };
                    format!("\"{sys}\\explorer.exe\" {target}")
                } else if args.is_empty() {
                    format!("\"{file}\"")
                } else {
                    format!("\"{file}\" {args}")
                };
                launch_with_shell_token(&line, cwd).map(|pid| format!("via the shell token (pid {pid})"))
            }
        },
    }
}

/// Fallback: Explorer's token + CreateProcessWithTokenW (needs admin).
fn launch_with_shell_token(cmdline: &str, cwd: Option<&str>) -> Result<u32, String> {
    let prim = verified_shell_token()?;
    unsafe {
        let mut cl = wide(cmdline);
        let cwdw = cwd.map(wide);
        let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessWithTokenW(
            prim.0,
            CREATE_PROCESS_LOGON_FLAGS(0),
            PCWSTR::null(),
            Some(PWSTR(cl.as_mut_ptr())),
            CREATE_UNICODE_ENVIRONMENT,
            None,
            cwdw.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr())),
            &si,
            &mut pi,
        )
        .map_err(|e| format!("CreateProcessWithTokenW: {}", e.message()))?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        Ok(pi.dwProcessId)
    }
}

/// Primary token duplicated from the shell (Explorer) process.
fn shell_token() -> Result<Handle, String> {
    unsafe {
        let shell = GetShellWindow();
        if shell.0.is_null() {
            return Err("no shell window (Explorer not running)".into());
        }
        let mut spid = 0u32;
        GetWindowThreadProcessId(shell, Some(&mut spid));
        let sh = open(spid, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let mut tok = HANDLE::default();
        OpenProcessToken(sh.0, TOKEN_DUPLICATE, &mut tok).map_err(|e| format!("shell token: {}", e.message()))?;
        let tok = Handle(tok);
        let mut prim = HANDLE::default();
        DuplicateTokenEx(
            tok.0,
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_SESSIONID,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut prim,
        )
        .map_err(|e| format!("duplicate token: {}", e.message()))?;
        Ok(Handle(prim))
    }
}

/// Plain CreateProcess: inherits Blink's elevated token.
pub fn launch_elevated(cmdline: &str, cwd: Option<&str>) -> Result<u32, String> {
    unsafe {
        let mut cl = wide(cmdline);
        let cwdw = cwd.map(wide);
        let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
        let mut pi = PROCESS_INFORMATION::default();
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cl.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            None,
            cwdw.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr())),
            &si,
            &mut pi,
        )
        .map_err(|e| format!("CreateProcess: {}", e.message()))?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        Ok(pi.dwProcessId)
    }
}

/// Terminate and relaunch with the same command line (cwd = exe folder).
/// `as_admin` = keep Blink's elevation; default is unelevated. Returns new pid.
///
/// The process is opened ONCE, verified against `create`, and the image path,
/// command line and the terminate all come from that handle: what is relaunched
/// is exactly the process that was verified, and a replacement under the same PID
/// is never read or killed.
pub fn restart(pid: u32, create: i64, as_admin: bool) -> Result<u32, String> {
    let h = open_target(pid, create, PROCESS_TERMINATE | PROCESS_SYNCHRONIZE)?;
    let path = osutil::image_of_handle(h.0).ok_or("cannot read image path")?;
    let cmd = crate::sample::nt::command_line(h.0 .0).unwrap_or_else(|| format!("\"{}\"", path));
    let cwd = osutil::parent_dir(&path).to_string();
    if as_admin && osutil::elevation() != Some(true) {
        return Err("Hayate is not running as admin".into());
    }
    if !as_admin && osutil::elevation() != Some(false) {
        // fail before killing anything if we could not relaunch: the same verified
        // shell token the fallback of `launch_unelevated` would need, not just "Explorer exists"
        verified_shell_token().map_err(|e| format!("cannot relaunch it unelevated, so it was not closed: {e}"))?;
    }
    unsafe {
        TerminateProcess(h.0, 1).map_err(|e| format!("terminate: {}", e.message()))?;
        let _ = WaitForSingleObject(h.0, 3000);
    }
    let cwd = (!cwd.is_empty()).then_some(cwd.as_str());
    if as_admin {
        launch_elevated(&cmd, cwd)
    } else {
        launch_unelevated(&cmd, cwd)
    }
}

/// Post WM_CLOSE to the process's visible top-level windows. Returns how many.
/// Verified against `create`; the handle stays open during the enumeration, so the
/// PID cannot be taken over by another process meanwhile.
pub fn close_windows(pid: u32, create: i64) -> usize {
    let Ok(_pin) = open_target(pid, create, PROCESS_SYNCHRONIZE) else { return 0 };
    struct Ctx {
        pid: u32,
        n: usize,
    }
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        let c = &mut *(l.0 as *mut Ctx);
        let mut p = 0u32;
        GetWindowThreadProcessId(h, Some(&mut p));
        if p == c.pid && IsWindowVisible(h).as_bool() && GetWindow(h, GW_OWNER).map_or(true, |o| o.0.is_null()) {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
            c.n += 1;
        }
        BOOL(1)
    }
    let mut c = Ctx { pid, n: 0 };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut c as *mut Ctx as isize));
    }
    c.n
}

/// Open Explorer with the file selected (unelevated, so the window is normal).
pub fn locate(path: &str) -> Result<(), String> {
    let windir = crate::osutil::windows_dir()?;
    launch_unelevated(&format!("\"{}\\explorer.exe\" /select,\"{}\"", windir, path), None).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleeper() -> (std::process::Child, i64) {
        let c = crate::osutil::quiet_cmd("ping").args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        let create = osutil::identity(c.id()).expect("child identity").0;
        (c, create)
    }

    fn facts(user: u8, session: u32, elevated: bool) -> TokenFacts {
        TokenFacts { user: vec![1, 1, 0, 0, 0, 0, 0, 5, user], session, elevated }
    }

    #[test]
    fn shell_token_is_used_only_for_the_same_user_session_and_unelevated() {
        let own = facts(1, 2, true);
        assert!(check_shell_token(&own, &facts(1, 2, false)).is_ok());
        assert!(check_shell_token(&own, &facts(9, 2, false)).unwrap_err().contains("different user"));
        assert!(check_shell_token(&own, &facts(1, 3, false)).unwrap_err().contains("different session"));
        assert!(check_shell_token(&own, &facts(1, 2, true)).unwrap_err().contains("elevated"), "an elevated shell gives no unelevated token: fail closed");
    }

    #[test]
    fn this_process_token_reads_back_and_matches_itself() {
        unsafe {
            let mut t = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t).unwrap();
            let t = Handle(t);
            let f = token_facts(t.0).expect("own token facts");
            assert!(f.user.len() >= 8 && f.user[0] == 1, "a SID starts with revision 1");
            assert_eq!(f.elevated, osutil::is_elevated());
            // the same token as the "shell" is accepted only when it is not elevated
            assert_eq!(check_shell_token(&f, &f).is_ok(), !f.elevated);
        }
    }

    #[test]
    fn an_elevated_blink_never_starts_programs_with_its_own_token() {
        assert_eq!(user_way(true, false), UserWay::ThroughExplorer);
        assert_eq!(user_way(true, true), UserWay::ThroughExplorer);
        assert_eq!(user_way(false, false), UserWay::Direct);
        assert_eq!(user_way(false, true), UserWay::ShellOpen);
        for shell in [false, true] {
            assert_ne!(user_way(true, shell), UserWay::Direct, "Direct is CreateProcess with Hayate's token");
        }
    }

    /// RED against the old gate (`is_elevated()` = `elevation().unwrap_or(false)`): a token that could not be
    /// read counted as a normal user and took the direct CreateProcess route with Blink's own token.
    #[test]
    fn an_unreadable_token_starts_nothing() {
        for shell in [false, true] {
            assert!(user_way_for(None, shell).unwrap_err().contains("Nothing was started"));
            assert_eq!(user_way_for(Some(false), shell), Ok(user_way(false, shell)));
            assert_eq!(user_way_for(Some(true), shell), Ok(UserWay::ThroughExplorer));
        }
    }

    fn kill(pid: u32) {
        if let Some((create, _)) = osutil::identity(pid) {
            if let Some(h) = osutil::open_same(pid, create, PROCESS_TERMINATE) {
                unsafe {
                    let _ = TerminateProcess(h.0, 1);
                }
            }
        }
    }

    #[test]
    fn trim_and_restart_refuse_a_pid_that_is_not_the_picked_process() {
        let (mut c, create) = sleeper();
        let pid = c.id();
        // the list showed another process under this PID: nothing is trimmed, read or killed
        assert!(trim(pid, create + 1).unwrap_err().contains("exited"));
        assert!(trim(pid, 0).is_err());
        assert!(restart(pid, create + 1, false).unwrap_err().contains("exited"));
        assert!(restart(pid, 0, false).is_err());
        assert!(c.try_wait().unwrap().is_none(), "child was terminated by a stale restart");
        // the verified process trims fine
        trim(pid, create).unwrap();
        let _ = c.kill();
        let _ = c.wait();
        assert!(trim(pid, create).is_err(), "exited process");
    }

    #[test]
    fn restart_relaunches_the_verified_process_image_and_command_line() {
        if osutil::is_elevated() {
            return; // an elevated test run would relaunch through Explorer
        }
        let (mut c, create) = sleeper();
        let pid = c.id();
        let image = osutil::image_path(pid).unwrap();
        let np = restart(pid, create, false).expect("restart");
        // the original was terminated through the verified handle
        assert!(c.wait().is_ok());
        assert_ne!(np, 0);
        assert_ne!(np, pid);
        assert_eq!(osutil::image_path(np).as_deref(), Some(image.as_str()), "relaunched the verified image");
        let cl = crate::sample::nt::command_line(
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, np) }.map(Handle).unwrap().0 .0,
        )
        .unwrap_or_default();
        assert!(cl.contains("127.0.0.1") && cl.contains("-n"), "command line carried over: {cl}");
        kill(np);
    }

    #[test]
    fn close_windows_is_verified() {
        let (mut c, create) = sleeper();
        assert_eq!(close_windows(c.id(), create + 1), 0);
        assert_eq!(close_windows(c.id(), 0), 0);
        let _ = c.kill();
        let _ = c.wait();
    }
}
