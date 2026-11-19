//! Hands-off in-place updating from a local drop folder (the publish step output).
//!
//! Opt-in: `%LOCALAPPDATA%\Blink\update.ini` (written by install.ps1) names a
//! `source` folder and the `install_dir`. Only a copy running from
//! `install_dir\blink.exe` updates itself; dev builds never do.
//!
//! * check: every 60 s the UI timer queues one thread-pool job (never on the
//!   UI thread, never two at once). Unchanged source = one GetFileAttributesExW
//!   on `blink.exe.sha256`.
//! * stage: a new sha256 → copy `blink.exe` to `<install>\blink.new.exe`,
//!   hash it (BCrypt SHA-256), keep it only when it matches.
//! * apply (UI thread, on the timer): window hidden or 2 min without input →
//!   rename blink.exe → blink.old.exe, blink.new.exe → blink.exe, close.
//! * relaunch (`finish`, after the message loop ended and features shut down):
//!   CreateProcessW of the new blink.exe with the same args (+ window state),
//!   inheriting this token. This process then waits as a watchdog: if the new
//!   one exits (or never marks itself ready) within ~40 s, it is rolled back:
//!   blink.exe → blink.bad.exe, blink.old.exe → blink.exe, relaunch the old one,
//!   and that build is never staged again.
//!
//! Elevated Blink (trust boundary): the source folder is writable by every
//! unelevated process of the user, and a matching .sha256 proves integrity,
//! not who made the file. So an elevated Blink never hands its token to a new
//! build. It stages and verifies as above, then only offers
//! "Update ready · <sha> · restart" in the status bar. Nothing is swapped at
//! idle / hidden moments. On click it swaps the files and asks UAC for the new
//! exe: `runas` from an elevated process would skip the prompt, so Explorer
//! starts this build unelevated as a helper (`--update-elevate`), which
//! ShellExecuteExW("runas")s the new blink.exe and reports the pid (or the
//! cancel) in `update-uac.txt`. The old instance then watches that pid exactly
//! like above. Cancel / failure / no answer: the files go back, this build is
//! relaunched (it is the one the user elevated), nothing is marked bad.
//!
//! Local file copies + renames inside the install dir + CreateProcessW of our
//! own exe (unelevated path) or a UAC prompt for it (elevated path): no
//! downloads, no registry, no tasks, no services, no shells.

pub mod logic;

use logic::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::osutil::{self, log, now_unix, pcw, wide};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::*;

pub const TIMER_CHECK: usize = 0xB1_0D;
pub const TIMER_READY: usize = 0xB1_0E;
const CHECK_MS: u32 = 60_000;
/// while a build is staged: look for a good moment more often
const STAGED_MS: u32 = 15_000;
/// the trial instance marks itself ready after this long with its window up
const READY_MS: u32 = 10_000;

struct Paths {
    source: PathBuf,
    dir: PathBuf,
    exe: PathBuf,
    new_exe: PathBuf,
    old_exe: PathBuf,
    bad_exe: PathBuf,
    ver: PathBuf,
    new_ver: PathBuf,
    old_ver: PathBuf,
}

impl Paths {
    fn src_exe(&self) -> PathBuf {
        self.source.join("blink.exe")
    }
    fn src_sha(&self) -> PathBuf {
        self.source.join("blink.exe.sha256")
    }
    fn src_ver(&self) -> PathBuf {
        self.source.join("version.txt")
    }
}

struct Ctx {
    /// None: feature off for this process (no config, dev build, multi instance)
    paths: Option<Paths>,
    /// Auto (unelevated) or Ask (elevated: click + UAC only)
    mode: Apply,
    /// argv[1..]
    args: Vec<String>,
    restore: Restore,
}

#[derive(Default)]
struct Inner {
    /// (last write FILETIME, size) of the source sha file at the last full look
    last_attrs: Option<(u64, u64)>,
    current_sha: String,
    current_label: String,
    staged: Option<(String, String)>,
    last_check: u64,
    source_ok: Option<bool>,
    /// install dir not writable (per-machine install, not elevated): stop trying
    write_failed: bool,
    /// leftovers (blink.old.exe ...) removed
    cleaned: bool,
    locked_logged: bool,
    /// trial run not yet marked ready: keep blink.old.exe for the watchdog
    trial_pending: bool,
    /// (text, until unix, ok)
    note: Option<(String, u64, bool)>,
    /// the last UAC prompt for an update was cancelled (recently)
    declined: bool,
    /// `note` is the "rolled back" one: the status bar offers a manual retry
    retry_note: bool,
    /// result of the last manual check (text, ok, unix time)
    manual: Option<(String, bool, u64)>,
    /// a manual check is queued or running
    manual_running: bool,
    /// unelevated: the user asked for this build, so apply it at the next tick
    /// without waiting for a hidden / idle moment
    manual_apply: bool,
}

/// A swap done on the UI thread, relaunched by `finish`.
struct Pending {
    restore: Restore,
    label: String,
    /// elevated: start the new build through a UAC prompt
    uac: bool,
}

static CTX: OnceLock<Ctx> = OnceLock::new();
static INNER: Mutex<Option<Inner>> = Mutex::new(None);
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// a manual check was asked for: the worker runs it after the current job
static MANUAL_REQ: AtomicBool = AtomicBool::new(false);
/// how long a manual check's result stays in the status bar
const MANUAL_NOTE_SECS: u64 = 300;
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// main window, so the worker can ask for an immediate tick after staging
static HWND_MAIN: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

fn ctx() -> &'static Ctx {
    CTX.get_or_init(load)
}

fn paths() -> Option<&'static Paths> {
    ctx().paths.as_ref()
}

fn inner<R>(f: impl FnOnce(&mut Inner) -> R) -> R {
    let mut g = INNER.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Inner::default))
}

fn state_file() -> PathBuf {
    osutil::data_dir().join("update-state.ini")
}

fn read_state() -> State {
    std::fs::read_to_string(state_file()).map(|t| parse_state(&t)).unwrap_or_default()
}

fn save_state(s: &State) {
    if let Err(e) = osutil::write_atomic(&state_file(), write_state(s).as_bytes()) {
        log(&format!("update: cannot write update-state.ini: {e}"));
    }
}

fn load() -> Ctx {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let restore = parse_restore(&args);
    let off = |why: &str| {
        if !why.is_empty() {
            log(&format!("update: off ({why})"));
        }
        None
    };
    let paths = (|| {
        // a BLINK_MULTI dev instance updates only with its own explicit data dir (test machine runs)
        if osutil::multi() && std::env::var_os("BLINK_DATA_DIR").is_none() {
            return off("");
        }
        let text = std::fs::read_to_string(osutil::data_dir().join("update.ini")).ok()?;
        let cfg = parse_config(&text);
        let source = cfg.source.filter(|_| cfg.enabled)?;
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?.to_path_buf();
        let install = cfg.install_dir.unwrap_or_else(|| {
            std::env::var("LOCALAPPDATA").map(|l| format!("{l}\\Programs\\Blink")).unwrap_or_default()
        });
        if !same_dir(&dir.to_string_lossy(), &install) {
            return off(&format!("running from {}, installed copy is in {install}", dir.display()));
        }
        if !exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case("blink.exe")) {
            return off(&format!("exe is {}, not blink.exe", exe.display()));
        }
        // also through junctions / 8.3 names / `..`: compare the resolved paths too
        let real = |s: &str| std::fs::canonicalize(s).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| s.to_string());
        if let Err(why) = source_allowed(&source, &install).and_then(|_| source_allowed(&real(&source), &real(&install))) {
            return off(&format!("{why} ({source})"));
        }
        let j = |n: &str| dir.join(n);
        Some(Paths {
            source: PathBuf::from(source),
            exe: j("blink.exe"),
            new_exe: j("blink.new.exe"),
            old_exe: j("blink.old.exe"),
            bad_exe: j("blink.bad.exe"),
            ver: j("version.txt"),
            new_ver: j("blink.new.version.txt"),
            old_ver: j("version.old.txt"),
            dir,
        })
    })();
    Ctx { paths, args, restore, mode: apply_mode(osutil::is_elevated()) }
}

fn mode() -> Apply {
    ctx().mode
}

/// First thing in main: an elevated relaunch (UAC path) went through Explorer
/// and lost our environment; restore BLINK_DATA_DIR / BLINK_MULTI from its
/// private args before anything reads them.
pub fn env_from_args() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = parse_restore(&args);
    if let Some(d) = r.data_dir {
        std::env::set_var("BLINK_DATA_DIR", d);
    }
    if r.multi {
        std::env::set_var("BLINK_MULTI", "1");
    }
}

/// `--update-elevate` helper mode (unelevated, started through Explorer by an
/// elevated Blink): ask UAC to start the swapped-in blink.exe, write the
/// outcome for the waiting old instance, exit. Returns true in helper mode.
pub fn helper() -> bool {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
    use windows::Win32::UI::Shell::*;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(h) = helper_args(&args) else { return false };
    let report = |r: &Uac| {
        if let Err(e) = osutil::write_atomic(Path::new(&h.result), write_uac(&h.nonce, r).as_bytes()) {
            log(&format!("update helper: cannot write {}: {e}", h.result));
        }
    };
    // only ever blink.exe next to this helper (the install dir)
    let exe = PathBuf::from(&h.exe);
    let here = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    let ok = exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case("blink.exe"))
        && here.zip(exe.parent()).is_some_and(|(a, b)| same_dir(&a.to_string_lossy(), &b.to_string_lossy()));
    if !ok {
        log(&format!("update helper: refusing {} (not blink.exe next to the helper)", h.exe));
        report(&Uac::Failed(87));
        return true;
    }
    let (file, params) = (wide(&h.exe), wide(&join_args(&h.args)));
    let dir = wide(&exe.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default());
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: windows::core::w!("runas"),
        lpFile: pcw(&file),
        lpParameters: pcw(&params),
        lpDirectory: pcw(&dir),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    let r = unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        ShellExecuteExW(&mut sei)
    };
    let out = match r {
        Ok(()) if !sei.hProcess.is_invalid() => {
            let pid = unsafe { GetProcessId(sei.hProcess) };
            unsafe {
                let _ = CloseHandle(sei.hProcess);
            }
            if pid == 0 {
                Uac::Failed(0)
            } else {
                Uac::Launched(pid)
            }
        }
        Ok(()) => Uac::Failed(0),
        Err(e) => {
            // HRESULT_FROM_WIN32 → the Win32 code; any other HRESULT stays whole
            let hr = e.code().0 as u32;
            let code = if hr >> 16 == 0x8007 { hr & 0xFFFF } else { hr };
            if code == ERROR_CANCELLED.0 {
                Uac::Cancelled
            } else {
                Uac::Failed(code)
            }
        }
    };
    log(&format!("update helper: UAC start of {} -> {out:?}", h.exe));
    report(&out);
    true
}

/// From main, before the window: resolve the config, record a trial start.
pub fn early() {
    #[cfg(feature = "test-broken-update")]
    {
        log("update: test-broken-update build: exiting at start (rollback test)");
        std::process::exit(3);
    }
    let c = ctx();
    let Some(p) = &c.paths else { return };
    let st = read_state();
    let ver = std::fs::read_to_string(&p.ver).ok().map(|t| parse_version(&t));
    let now = now_unix();
    if c.restore.trial.is_some() && osutil::is_elevated() {
        // an elevated self-update just started this build: the Task Manager launcher (when the redirect
        // is on) is now an older build, so the start-up check runs even if it already ran for another pair
        crate::taskmgr::note_updated();
    }
    inner(|i| {
        if let Some(t) = &c.restore.trial {
            i.current_sha = t.clone();
            i.trial_pending = true;
        }
        i.current_label = label(ver.as_ref(), &i.current_sha);
        if !st.updated_to.is_empty() && now.saturating_sub(st.updated_at) < NOTE_SECS {
            i.note = Some((format!("Updated to {}", st.updated_to), st.updated_at + NOTE_SECS, true));
        }
        if !st.rolled_back.is_empty() && now.saturating_sub(st.rolled_back_at) < NOTE_SECS && st.rolled_back_at >= st.updated_at {
            i.note = Some((rolled_back_note(&st.rolled_back), st.rolled_back_at + NOTE_SECS, false));
            i.retry_note = true;
        }
        i.declined = !st.declined.is_empty() && now.saturating_sub(st.declined_at) < NOTE_SECS;
    });
    log(&format!(
        "update: on, source {}, install {}, running {}{}",
        p.source.display(),
        p.dir.display(),
        inner(|i| i.current_label.clone()),
        if c.mode == Apply::Ask { " (elevated: new builds only on click, through UAC)" } else { "" }
    ));
    if let Some(t) = &c.restore.trial {
        let mut st = st;
        if &st.trial == t {
            st.trial_pid = std::process::id();
            st.quit_pid = 0;
            save_state(&st);
            log(&format!("update: trial start of {} pid {}", &t[..12], st.trial_pid));
        } else {
            log("update: --update-trial without a matching state entry (ignored)");
        }
    }
}

/// From win::run once the window exists: start the check timer.
pub fn start(hwnd: HWND) {
    if paths().is_none() {
        return;
    }
    HWND_MAIN.store(hwnd.0 as isize, Ordering::Relaxed);
    unsafe {
        SetTimer(Some(hwnd), TIMER_CHECK, CHECK_MS, None);
        if ctx().restore.trial.is_some() {
            SetTimer(Some(hwnd), TIMER_READY, READY_MS, None);
        }
    }
    if ctx().restore.trial.is_none() {
        // back from a cancelled UAC prompt: offer the build again right away
        queue(inner(|i| i.declined));
    }
}

/// Tab index to restore after an update relaunch.
pub fn restore_tab() -> Option<usize> {
    ctx().restore.tab
}

/// Show the window as the previous instance had it (an update relaunch), or
/// the normal way: hidden when resident, else shown. `show` is the normal
/// show+activate; `quiet` marks the window visible and calls ShowWindow(cmd).
pub fn restore_window(hwnd: HWND, resident: bool, show: fn(HWND), quiet: fn(HWND, SHOW_WINDOW_CMD)) -> Show {
    let r = &ctx().restore;
    let mode = r.show.unwrap_or(if resident { Show::Hidden } else { Show::Front });
    unsafe {
        if let Some((l, t, rr, b)) = r.place {
            let wp = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                showCmd: SW_HIDE.0 as u32,
                rcNormalPosition: RECT { left: l, top: t, right: rr, bottom: b },
                ..Default::default()
            };
            let _ = SetWindowPlacement(hwnd, &wp);
        }
        match mode {
            Show::Hidden => {}
            Show::Front => {
                show(hwnd);
                if r.maximized {
                    let _ = ShowWindow(hwnd, SW_MAXIMIZE);
                }
            }
            // never steal focus from what the user is doing now
            Show::Back => {
                quiet(hwnd, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            Show::Min => quiet(hwnd, SW_SHOWMINNOACTIVE),
        }
    }
    if r.show.is_some() {
        log(&format!("update: restored window {mode:?}, tab {:?}", r.tab));
    }
    mode
}

/// WM_TIMER from win.rs. Returns true when the window should close now
/// (a staged build is about to be swapped in; `finish` relaunches).
pub fn on_timer(hwnd: HWND, id: usize, tab: usize) -> bool {
    match id {
        TIMER_READY => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_READY);
            }
            mark_ready();
            false
        }
        TIMER_CHECK => {
            // elevated: a staged build waits for the click; keep watching the source
            if mode() == Apply::Auto && inner(|i| i.staged.is_some()) {
                // a manual check asked for this build: apply it without waiting for a good moment
                let asked = inner(|i| std::mem::take(&mut i.manual_apply));
                return try_apply(hwnd, tab, asked);
            }
            queue(true);
            false
        }
        _ => false,
    }
}

/// Elevated Blink with a verified build staged: the status bar offers "restart".
pub fn offer() -> bool {
    paths().is_some() && mode() == Apply::Ask && INNER.try_lock().ok().is_some_and(|g| g.as_ref().is_some_and(|i| i.staged.is_some()))
}

/// The user clicked "restart" in the status bar. True: the window should
/// close now (`finish` asks UAC for the new build).
pub fn click(hwnd: HWND, tab: usize) -> bool {
    if !offer() {
        return false;
    }
    log("update: restart clicked");
    try_apply(hwnd, tab, true)
}

/// The user closes the window on purpose (X, Esc, tray Quit) and the process will exit. During
/// a trial that is not a failed start: say so in the state file so the watching instance keeps
/// this build (exit code 0 alone is not proof: it also covers exits nobody can explain).
pub fn note_user_quit() {
    let Some(t) = ctx().restore.trial.clone() else { return };
    let mut st = read_state();
    if st.trial != t || st.trial_pid != std::process::id() || st.ready == t {
        return;
    }
    st.quit_pid = std::process::id();
    save_state(&st);
    log("update: the trial instance is closed by the user before it was ready (not a failure)");
}

fn mark_ready() {
    let Some(t) = ctx().restore.trial.clone() else { return };
    let mut st = read_state();
    if st.trial != t || st.trial_pid != std::process::id() {
        log("update: ready timer, but the state file names another trial (ignored)");
        return;
    }
    let lbl = inner(|i| {
        i.trial_pending = false;
        i.current_label.clone()
    });
    st.ready = t;
    st.updated_to = lbl.clone();
    st.updated_at = now_unix();
    save_state(&st);
    inner(|i| i.note = Some((format!("Updated to {lbl}"), st.updated_at + NOTE_SECS, true)));
    log(&format!("update: {lbl} ready (trial passed)"));
    queue(false);
}

/// Queue the thread-pool job unless one is still running (slow/asleep source disk).
fn queue(check: bool) {
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    unsafe extern "system" fn job(p: *mut core::ffi::c_void) -> u32 {
        let check = !p.is_null();
        cleanup();
        if check {
            check_source();
        }
        while MANUAL_REQ.swap(false, Ordering::AcqRel) {
            manual_check_source();
        }
        IN_FLIGHT.store(false, Ordering::Release);
        // asked for between the loop above and the store: run it in a new job
        if MANUAL_REQ.load(Ordering::Acquire) {
            queue(false);
        }
        0
    }
    let arg = if check { std::ptr::dangling::<core::ffi::c_void>() } else { std::ptr::null() };
    if unsafe { QueueUserWorkItem(Some(job), Some(arg), WT_EXECUTELONGFUNCTION) }.is_err() {
        IN_FLIGHT.store(false, Ordering::Release);
    }
}

fn exists(p: &Path) -> bool {
    let w = wide(&p.to_string_lossy());
    unsafe { GetFileAttributesW(pcw(&w)) != INVALID_FILE_ATTRIBUTES }
}

fn delete(p: &Path) -> std::result::Result<(), String> {
    let w = wide(&p.to_string_lossy());
    unsafe { DeleteFileW(pcw(&w)) }.map_err(|e| e.message())
}

fn rename(from: &Path, to: &Path, replace: bool) -> std::result::Result<(), String> {
    let (a, b) = (wide(&from.to_string_lossy()), wide(&to.to_string_lossy()));
    let fl = if replace { MOVEFILE_REPLACE_EXISTING } else { MOVE_FILE_FLAGS(0) };
    unsafe { MoveFileExW(pcw(&a), pcw(&b), fl) }.map_err(|e| format!("{} -> {}: {}", file(from), file(to), e.message()))
}

fn file(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Remove leftovers of the last swap (inside the install dir only).
fn cleanup() {
    let Some(p) = paths() else { return };
    let (cleaned, trial, staged) = inner(|i| (i.cleaned, i.trial_pending, i.staged.is_some()));
    if cleaned || trial {
        return;
    }
    let mut left = false;
    let mut targets = vec![&p.old_exe, &p.bad_exe, &p.old_ver];
    if !staged {
        targets.extend([&p.new_exe, &p.new_ver]);
    }
    for f in targets {
        if !exists(f) {
            continue;
        }
        match delete(f) {
            Ok(()) => log(&format!("update: removed {}", file(f))),
            Err(e) => {
                left = true;
                if !inner(|i| std::mem::replace(&mut i.locked_logged, true)) {
                    log(&format!("update: {} still in use ({e}); retry on the next check", file(f)));
                }
            }
        }
    }
    inner(|i| i.cleaned = !left);
}

/// (last write time, size) of a file: the only work done when nothing changed.
fn attrs(p: &Path) -> Option<(u64, u64)> {
    let w = wide(&p.to_string_lossy());
    let mut d = WIN32_FILE_ATTRIBUTE_DATA::default();
    unsafe { GetFileAttributesExW(pcw(&w), GetFileExInfoStandard, &mut d as *mut _ as *mut _) }.ok()?;
    let t = (d.ftLastWriteTime.dwHighDateTime as u64) << 32 | d.ftLastWriteTime.dwLowDateTime as u64;
    Some((t, (d.nFileSizeHigh as u64) << 32 | d.nFileSizeLow as u64))
}

pub(crate) fn sha256_file(p: &Path) -> std::result::Result<String, String> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let data = std::fs::read(p).map_err(|e| format!("read {}: {e}", file(p)))?;
    let mut out = [0u8; 32];
    let st = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, &data, &mut out) };
    if st.is_err() {
        return Err(format!("BCryptHash: {:#x}", st.0));
    }
    Ok(out.iter().map(|b| format!("{b:02x}")).collect())
}

fn check_source() {
    let Some(p) = paths() else { return };
    let now = now_unix();
    let a = attrs(&p.src_sha());
    let was_ok = inner(|i| {
        i.last_check = now;
        i.source_ok.replace(a.is_some())
    });
    let Some(a) = a else {
        if was_ok != Some(false) {
            log(&format!("update: source {} not reachable (no blink.exe.sha256); will keep checking", p.source.display()));
        }
        return;
    };
    if inner(|i| i.last_attrs == Some(a)) {
        return; // unchanged: done
    }
    if was_ok == Some(false) {
        log("update: source reachable again");
    }
    let remember = || inner(|i| i.last_attrs = Some(a));
    let Some(src_sha) = std::fs::read_to_string(p.src_sha()).ok().and_then(|t| parse_sha_file(&t)) else {
        log("update: blink.exe.sha256 in the source is not a sha256; ignored until it changes");
        return remember();
    };
    if let Err(e) = ensure_current(p) {
        log(&format!("update: cannot hash the running exe: {e}"));
        return;
    }
    let (cur, staged, write_failed) = inner(|i| (i.current_sha.clone(), i.staged.as_ref().map(|s| s.0.clone()), i.write_failed));
    let st = read_state();
    match should_stage(&src_sha, &cur, staged.as_deref(), &st.bad) {
        Stage::Nothing(why) => {
            if why != "same build" {
                log(&format!("update: source has {}: {why}", &src_sha[..12]));
            }
            return remember();
        }
        Stage::Stage if write_failed => return,
        Stage::Stage => {}
    }
    if let Err((forget, _)) = stage_build(p, &src_sha, a) {
        if forget {
            remember();
        }
    }
}

/// Our own hash + label, once per process.
fn ensure_current(p: &Paths) -> std::result::Result<(), String> {
    if inner(|i| i.current_sha.is_empty()) {
        let h = sha256_file(&p.exe)?;
        let ver = std::fs::read_to_string(&p.ver).ok().map(|t| parse_version(&t));
        inner(|i| {
            i.current_label = label(ver.as_ref(), &h);
            i.current_sha = h;
        });
    }
    Ok(())
}

/// Copy the source build to blink.new.exe, verify it and mark it staged.
/// Err((remember, why)): nothing staged; `remember` = do not look at this
/// source write again until it changes (hash mismatch).
fn stage_build(p: &Paths, src_sha: &str, a: (u64, u64)) -> std::result::Result<(), (bool, String)> {
    let src_ver = std::fs::read_to_string(p.src_ver()).ok().map(|t| parse_version(&t));
    let lbl = label(src_ver.as_ref(), src_sha);
    log(&format!("update: new build {lbl} ({}) in source, copying", &src_sha[..12]));
    let t0 = std::time::Instant::now();
    if let Err(e) = std::fs::copy(p.src_exe(), &p.new_exe) {
        let denied = e.kind() == std::io::ErrorKind::PermissionDenied;
        log(&format!("update: copy to {} failed: {e}{}", p.new_exe.display(), if denied { " (install dir not writable: auto-update needs the per-user install or an elevated Blink; stopped)" } else { "" }));
        if denied {
            inner(|i| i.write_failed = true);
        }
        return Err((false, format!("cannot copy to {}: {e}", file(&p.new_exe))));
    }
    match sha256_file(&p.new_exe) {
        Ok(h) if h == src_sha => {}
        Ok(h) => {
            let _ = delete(&p.new_exe);
            log(&format!("update: hash mismatch (copied {}, sha file says {}); waiting for a new publish", &h[..12], &src_sha[..12]));
            return Err((true, "the build does not match its .sha256".into()));
        }
        Err(e) => {
            let _ = delete(&p.new_exe);
            log(&format!("update: cannot hash blink.new.exe: {e}"));
            return Err((false, format!("cannot hash blink.new.exe: {e}")));
        }
    }
    if src_ver.is_some() {
        let _ = std::fs::copy(p.src_ver(), &p.new_ver);
    } else {
        let _ = delete(&p.new_ver);
    }
    inner(|i| {
        i.staged = Some((src_sha.to_string(), lbl.clone()));
        i.last_attrs = Some(a);
    });
    let ms = t0.elapsed().as_millis();
    if mode() == Apply::Ask {
        log(&format!("update: staged {lbl} as blink.new.exe ({ms} ms, hash ok); elevated: offered in the status bar, starts only on click, through UAC"));
        repaint();
        return Ok(());
    }
    log(&format!("update: staged {lbl} as blink.new.exe ({ms} ms, hash ok); swaps in when the window is hidden or after 2 min idle"));
    // look for a good moment now instead of on the next 60 s tick (hidden: swaps in right away)
    tick_now();
    Ok(())
}

fn repaint() {
    let h = HWND_MAIN.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = InvalidateRect(Some(HWND(h as *mut _)), None, false);
        }
    }
}

/// Run the UI thread's staged-build step now instead of on the next timer tick.
fn tick_now() {
    let h = HWND_MAIN.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_TIMER, WPARAM(TIMER_CHECK), LPARAM(0));
        }
    }
}

// ---- manual "Check for updates" -----------------------------------------------------

/// Auto-update is configured for this copy (an installed copy with an update source).
pub fn enabled() -> bool {
    paths().is_some()
}

/// The user asked for a check (Settings button, rolled-back status note).
/// Runs on the thread-pool job; the result shows up in `manual_status`.
pub fn check_now() {
    if paths().is_none() {
        return;
    }
    log("update: manual check requested");
    inner(|i| i.manual_running = true);
    MANUAL_REQ.store(true, Ordering::Release);
    queue(false);
}

/// (text, ok, running) for the Settings block: the last manual check's result.
pub fn manual_status() -> Option<(String, bool, bool)> {
    let g = INNER.try_lock().ok()?;
    let i = g.as_ref()?;
    if i.manual_running {
        return Some(("Checking\u{2026}".into(), true, true));
    }
    i.manual.as_ref().map(|(t, ok, at)| (format!("{t} \u{b7} {}", ago(now_unix().saturating_sub(*at))), *ok, false))
}

/// The status bar shows the "rolled back" note: clicking it runs a manual check.
pub fn retry_offer() -> bool {
    let now = now_unix();
    paths().is_some()
        && INNER.try_lock().ok().is_some_and(|g| {
            g.as_ref().is_some_and(|i| i.retry_note && i.note.as_ref().is_some_and(|n| now < n.1) && !(mode() == Apply::Ask && i.staged.is_some()))
        })
}

fn set_manual(text: String, ok: bool) {
    let now = now_unix();
    inner(|i| {
        i.manual_running = false;
        i.manual = Some((text.clone(), ok, now));
        i.note = Some((text, now + MANUAL_NOTE_SECS, ok));
        i.retry_note = false;
    });
    repaint();
}

fn manual_check_source() {
    let (text, ok) = match manual_run() {
        Ok(t) => (t, true),
        Err(e) => {
            log(&format!("update: manual check failed: {e}"));
            (manual_error(&e), false)
        }
    };
    log(&format!("update: manual check: {text}"));
    set_manual(text, ok);
}

/// Re-read the source and stage its build when it differs from the running
/// one, even if an earlier trial marked it bad. Err: text for the user.
fn manual_run() -> std::result::Result<String, String> {
    let p = paths().ok_or("auto-update is off for this copy")?;
    let now = now_unix();
    let a = attrs(&p.src_sha());
    inner(|i| {
        i.last_check = now;
        i.source_ok = Some(a.is_some());
    });
    let a = a.ok_or_else(|| format!("source not reachable ({})", p.source.display()))?;
    let src_sha = std::fs::read_to_string(p.src_sha()).ok().and_then(|t| parse_sha_file(&t)).ok_or("blink.exe.sha256 in the source is not a sha256")?;
    ensure_current(p).map_err(|e| format!("cannot hash the running exe: {e}"))?;
    let src_ver = std::fs::read_to_string(p.src_ver()).ok().map(|t| parse_version(&t));
    let lbl = label(src_ver.as_ref(), &src_sha);
    let (cur, staged) = inner(|i| (i.current_sha.clone(), i.staged.as_ref().map(|s| s.0.clone())));
    let before = read_state();
    let mut st = before.clone();
    let r = manual_check(&src_sha, &cur, staged.as_deref(), &lbl, &mut st);
    match &r {
        Manual::UpToDate => {}
        Manual::AlreadyStaged => {
            // unelevated: the user asked, so swap it in at the next tick
            if mode() == Apply::Auto {
                inner(|i| i.manual_apply = true);
                tick_now();
            }
        }
        Manual::Stage { retry } => {
            if st != before {
                if *retry {
                    log(&format!("update: manual check cleared bad mark for {lbl} ({})", &src_sha[..12]));
                }
                save_state(&st);
            }
            inner(|i| {
                i.write_failed = false;
                i.manual_apply = mode() == Apply::Auto;
            });
            if let Err((_, why)) = stage_build(p, &src_sha, a) {
                // nothing staged: keep the marks the way they were
                inner(|i| i.manual_apply = false);
                if st != before {
                    save_state(&before);
                }
                return Err(why);
            }
        }
    }
    Ok(manual_text(&r, &lbl))
}

fn idle_ms() -> u32 {
    let mut li = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe {
        if !GetLastInputInfo(&mut li).as_bool() {
            return 0;
        }
        windows::Win32::System::SystemInformation::GetTickCount().wrapping_sub(li.dwTime)
    }
}

fn try_apply(hwnd: HWND, tab: usize, clicked: bool) -> bool {
    let Some(p) = paths() else { return false };
    let uac = mode() == Apply::Ask;
    if !uac {
        unsafe {
            SetTimer(Some(hwnd), TIMER_CHECK, STAGED_MS, None);
        }
    }
    let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
    let idle = idle_ms();
    if !swap_now(mode(), clicked, visible, idle) {
        return false;
    }
    let Some((sha, lbl)) = inner(|i| i.staged.clone()) else { return false };
    // the staged file must still be the verified one
    if !exists(&p.new_exe) {
        log("update: blink.new.exe vanished; staging again");
        inner(|i| {
            i.staged = None;
            i.last_attrs = None;
        });
        unsafe {
            SetTimer(Some(hwnd), TIMER_CHECK, CHECK_MS, None);
        }
        return false;
    }
    // elevated: hash it again right before the swap (it sat on disk since staging)
    if uac {
        match sha256_file(&p.new_exe) {
            Ok(h) if h == sha => {}
            r => {
                log(&format!("update: blink.new.exe changed since it was verified ({r:?}); dropped, staging again"));
                let _ = delete(&p.new_exe);
                inner(|i| {
                    i.staged = None;
                    i.last_attrs = None;
                });
                return false;
            }
        }
    }
    let show = unsafe {
        if !visible {
            Show::Hidden
        } else if IsIconic(hwnd).as_bool() {
            Show::Min
        } else if GetForegroundWindow() == hwnd {
            Show::Front
        } else {
            Show::Back
        }
    };
    let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    let place = unsafe { GetWindowPlacement(hwnd, &mut wp) }.ok().map(|_| {
        let r = wp.rcNormalPosition;
        (r.left, r.top, r.right, r.bottom)
    });
    let maximized = unsafe { IsZoomed(hwnd).as_bool() };

    // swap files: blink.exe (running image; rename is allowed) → blink.old.exe
    if exists(&p.old_exe) {
        if let Err(e) = delete(&p.old_exe) {
            log(&format!("update: cannot apply yet, blink.old.exe in use ({e})"));
            return false;
        }
    }
    if let Err(e) = rename(&p.exe, &p.old_exe, false) {
        log(&format!("update: apply failed: {e}"));
        return false;
    }
    if let Err(e) = rename(&p.new_exe, &p.exe, false) {
        log(&format!("update: apply failed: {e}; putting blink.exe back"));
        if let Err(e2) = rename(&p.old_exe, &p.exe, false) {
            log(&format!("update: restore failed too: {e2}"));
        }
        return false;
    }
    let _ = delete(&p.old_ver);
    if exists(&p.ver) {
        let _ = rename(&p.ver, &p.old_ver, true);
    }
    if exists(&p.new_ver) {
        let _ = rename(&p.new_ver, &p.ver, true);
    }
    let cur = inner(|i| i.current_sha.clone());
    let mut st = read_state();
    st.trial = sha.clone();
    st.from = cur;
    st.trial_pid = 0;
    st.quit_pid = 0;
    st.ready.clear();
    save_state(&st);
    let mut restore = Restore { trial: Some(sha), show: Some(show), tab: Some(tab), place, maximized, ..Default::default() };
    if uac {
        // that start goes through Explorer + UAC: carry the test environment as args
        restore.data_dir = std::env::var("BLINK_DATA_DIR").ok().filter(|d| !d.is_empty());
        restore.multi = osutil::multi();
    }
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(Pending { restore, label: lbl.clone(), uac });
    if show == Show::Front {
        // we are the foreground app: let the new instance take the foreground
        unsafe {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
        }
    }
    log(&format!(
        "update: applying {lbl} (was {}): window {show:?}, idle {} s; files swapped, closing for relaunch{}",
        inner(|i| i.current_label.clone()),
        idle / 1000,
        if uac { " through UAC" } else { "" }
    ));
    true
}

fn spawn(exe: &Path, args: &[String]) -> std::result::Result<PROCESS_INFORMATION, String> {
    let line = command_line(&exe.to_string_lossy(), args);
    let mut cmd = wide(&line);
    let app = wide(&exe.to_string_lossy());
    let dir = wide(&exe.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default());
    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    // same token as this process: an elevated Blink restarts elevated, a normal one normal
    unsafe { CreateProcessW(pcw(&app), Some(windows::core::PWSTR(cmd.as_mut_ptr())), None, None, false, PROCESS_CREATION_FLAGS(0), None, pcw(&dir), &si, &mut pi) }
        .map_err(|e| e.message())?;
    unsafe {
        let _ = CloseHandle(pi.hThread);
    }
    Ok(pi)
}

/// From main after win::run returned: relaunch the swapped-in build and
/// watch it start; roll back when it does not come up.
pub fn finish() {
    let Some(Pending { restore, label: lbl, uac }) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    let Some(p) = paths() else { return };
    let Some(sha) = restore.trial.clone() else { return };
    let args = relaunch_args(&ctx().args, &restore);
    let (proc, pid) = if uac {
        match start_via_uac(p, &args) {
            Ok(x) => x,
            Err(UacEnd::Undo(why)) => {
                undo(p, &restore, &lbl, why);
                return;
            }
            Err(UacEnd::Gone) => {
                log(&format!("update: {lbl} (pid reported by the UAC helper) is already gone; rolling back"));
                rollback(p, &restore, &sha, &lbl);
                return;
            }
        }
    } else {
        match spawn(&p.exe, &args) {
            Ok(pi) => (pi.hProcess, pi.dwProcessId),
            Err(e) => {
                log(&format!("update: start of {lbl} failed: {e}"));
                rollback(p, &restore, &sha, &lbl);
                return;
            }
        }
    };
    log(&format!("update: started {lbl} pid {pid}{}; watching its start", if uac { " (elevated, user consented)" } else { "" }));
    let t0 = std::time::Instant::now();
    loop {
        let exited = unsafe { WaitForSingleObject(proc, 250) } == WAIT_OBJECT_0;
        let st = read_state();
        match trial_outcome(exited, &st, &sha, pid, t0.elapsed().as_millis() as u64) {
            Trial::Wait => continue,
            Trial::Ok => {
                log(&format!("update: {lbl} is up ({} ms); this old instance exits", t0.elapsed().as_millis()));
                break;
            }
            Trial::UserQuit => {
                // closed on purpose inside the trial window (exit code 0, said so itself): the
                // build stays, nothing is marked bad, no relaunch against the user's will
                let mut st = read_state();
                st.trial.clear();
                st.ready.clear();
                st.trial_pid = 0;
                st.quit_pid = 0;
                save_state(&st);
                log(&format!("update: {lbl} was closed by the user before it was ready ({} ms); kept, not a failure", t0.elapsed().as_millis()));
                break;
            }
            Trial::Rollback(why) => {
                let mut code = 0u32;
                unsafe {
                    if !exited {
                        let _ = TerminateProcess(proc, 1);
                        WaitForSingleObject(proc, 5000);
                    }
                    let _ = GetExitCodeProcess(proc, &mut code);
                }
                log(&format!("update: {lbl} failed its start ({why}, exit code {code:#x}); rolling back"));
                rollback(p, &restore, &sha, &lbl);
                break;
            }
        }
    }
    unsafe {
        let _ = CloseHandle(proc);
    }
}

/// blink.exe (failed build) → blink.bad.exe, blink.old.exe (this running
/// image) → blink.exe, never stage that build again, relaunch the old one.
fn rollback(p: &Paths, restore: &Restore, sha: &str, lbl: &str) {
    let mut ok = true;
    for (a, b) in [(&p.exe, &p.bad_exe), (&p.old_exe, &p.exe)] {
        if let Err(e) = rename(a, b, true) {
            log(&format!("update: rollback step failed: {e}"));
            ok = false;
        }
    }
    if exists(&p.old_ver) {
        let _ = rename(&p.old_ver, &p.ver, true);
    } else {
        let _ = delete(&p.ver);
    }
    let mut st = read_state();
    st.mark_bad(sha);
    st.trial.clear();
    st.ready.clear();
    st.trial_pid = 0;
    st.quit_pid = 0;
    st.rolled_back = lbl.to_string();
    st.rolled_back_at = now_unix();
    save_state(&st);
    if !ok && !exists(&p.exe) {
        log("update: rollback could not restore blink.exe; not relaunching");
        return;
    }
    relaunch_current(p, restore, &format!("rolled back; previous build relaunched ({lbl} marked bad)"));
}

/// Start blink.exe again with this process's token (rollback / undo). Elevated:
/// only when blink.exe really is the build this instance runs, so a failed
/// rename can never start the new build with our token.
fn relaunch_current(p: &Paths, restore: &Restore, what: &str) {
    if mode() == Apply::Ask {
        let cur = inner(|i| i.current_sha.clone());
        match sha256_file(&p.exe) {
            Ok(h) if !cur.is_empty() && h == cur => {}
            r => {
                log(&format!("update: blink.exe is not this build ({r:?}); not relaunching it elevated"));
                return;
            }
        }
    }
    let back = Restore { trial: None, data_dir: None, multi: false, ..restore.clone() };
    match spawn(&p.exe, &relaunch_args(&ctx().args, &back)) {
        Ok(pi) => {
            log(&format!("update: {what}: pid {}", pi.dwProcessId));
            unsafe {
                let _ = CloseHandle(pi.hProcess);
            }
        }
        Err(e) => log(&format!("update: relaunch failed ({what}): {e}")),
    }
}

enum UacEnd {
    /// no consent / no start: keep the current build
    Undo(&'static str),
    /// consent given, but the new build already exited: a start crash
    Gone,
}

fn uac_file() -> PathBuf {
    osutil::data_dir().join("update-uac.txt")
}

/// Elevated path: Explorer starts this build (now blink.old.exe) unelevated
/// as the helper; the helper asks UAC to start the new blink.exe and writes
/// the answer. Returns the new build's process (verified image) and pid.
fn start_via_uac(p: &Paths, args: &[String]) -> std::result::Result<(HANDLE, u32), UacEnd> {
    let result = uac_file();
    let _ = std::fs::remove_file(&result);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
    let nonce = format!("{:x}-{nanos:x}", std::process::id());
    let h = Helper { nonce: nonce.clone(), result: result.to_string_lossy().into_owned(), exe: p.exe.to_string_lossy().into_owned(), args: args.to_vec() };
    let cmd = helper_cmd(&p.old_exe.to_string_lossy(), &h);
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
    if let Err(e) = crate::rules::actions::launch_unelevated(&cmd, Some(&p.dir.to_string_lossy())) {
        log(&format!("update: cannot start the UAC helper: {e}"));
        return Err(UacEnd::Undo("UAC helper did not start"));
    }
    log("update: UAC helper started unelevated (through Explorer); waiting for the prompt's answer");
    let t0 = std::time::Instant::now();
    let pid = loop {
        let r = std::fs::read_to_string(&result).map(|t| parse_uac(&t, &nonce)).unwrap_or(Uac::Pending);
        match uac_step(&r, t0.elapsed().as_millis() as u64) {
            UacStep::Wait => std::thread::sleep(std::time::Duration::from_millis(200)),
            UacStep::Watch(pid) => break pid,
            UacStep::Undo(why) => {
                let _ = std::fs::remove_file(&result);
                log(&format!("update: {why} ({r:?}, after {} s)", t0.elapsed().as_secs()));
                return Err(UacEnd::Undo(why));
            }
        }
    };
    let _ = std::fs::remove_file(&result);
    // the answer sits in a user-writable file: only watch (and maybe
    // terminate) that pid when it really is the new blink.exe
    unsafe {
        let h = match OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, false, pid) {
            Ok(h) => h,
            Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Err(UacEnd::Gone),
            Err(e) => {
                log(&format!("update: cannot open pid {pid}: {}", e.message()));
                return Err(UacEnd::Undo("cannot watch the new build"));
            }
        };
        let mut buf = vec![0u16; 1024];
        let mut n = buf.len() as u32;
        let img = if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut n).is_ok() {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            String::new()
        };
        if norm_path(&img) != norm_path(&p.exe.to_string_lossy()) {
            let _ = CloseHandle(h);
            log(&format!("update: helper named pid {pid} = {img:?}, not {}; not touching it", p.exe.display()));
            return Err(UacEnd::Undo("helper named another process"));
        }
        Ok((h, pid))
    }
}

/// The UAC-gated start did not happen (cancelled, failed, no answer): put
/// this build back as blink.exe and keep running it. The new build is not
/// bad: it goes back to blink.new.exe and stays offered.
fn undo(p: &Paths, restore: &Restore, lbl: &str, why: &str) {
    log(&format!("update: {lbl} not started ({why}); keeping the current build"));
    let mut ok = true;
    for (a, b) in [(&p.exe, &p.new_exe), (&p.old_exe, &p.exe)] {
        if let Err(e) = rename(a, b, true) {
            log(&format!("update: undo step failed: {e}"));
            ok = false;
        }
    }
    if exists(&p.ver) {
        let _ = rename(&p.ver, &p.new_ver, true);
    }
    if exists(&p.old_ver) {
        let _ = rename(&p.old_ver, &p.ver, true);
    }
    let mut st = read_state();
    st.trial.clear();
    st.ready.clear();
    st.trial_pid = 0;
    st.quit_pid = 0;
    st.declined = lbl.to_string();
    st.declined_at = now_unix();
    save_state(&st);
    if !ok {
        log("update: undo incomplete; not relaunching");
        return;
    }
    relaunch_current(p, restore, "UAC not granted; current build relaunched");
}

/// Status-bar text: Some only when auto-update is on. (text, highlight)
pub fn status_line(wide_bar: bool) -> Option<(String, Option<bool>)> {
    let p = paths()?;
    let now = now_unix();
    let g = INNER.try_lock().ok()?;
    let i = g.as_ref()?;
    // elevated: the offer wins over notes (it is the only way in)
    if let (Apply::Ask, Some((_, l))) = (mode(), &i.staged) {
        return Some((offer_text(l, i.declined), Some(true)));
    }
    if let Some((t, until, ok)) = &i.note {
        if now < *until {
            return Some((t.clone(), Some(*ok)));
        }
    }
    if let Some((_, l)) = &i.staged {
        return Some((format!("Update {l} ready · swaps in when hidden or idle"), Some(true)));
    }
    let ver = if i.current_label.is_empty() { "?" } else { &i.current_label };
    let chk = match (i.source_ok, i.last_check) {
        (_, 0) => "first check in 1 min".to_string(),
        (Some(false), t) => format!("source offline · checked {}", ago(now.saturating_sub(t))),
        (_, t) => format!("checked {}", ago(now.saturating_sub(t))),
    };
    Some((
        if wide_bar { format!("Auto-update {ver} · {} · {chk}", p.source.display()) } else { format!("Auto-update {ver} · {chk}") },
        None,
    ))
}

/// First tray-tooltip line for an hour after an update / rollback.
pub fn tip_note() -> Option<String> {
    let now = now_unix();
    let g = INNER.try_lock().ok()?;
    let i = g.as_ref()?;
    if let (Apply::Ask, Some((_, l))) = (mode(), &i.staged) {
        return Some(format!("Update {l} ready: open Hayate, click restart"));
    }
    i.note.as_ref().filter(|n| now < n.1).map(|n| n.0.clone())
}
