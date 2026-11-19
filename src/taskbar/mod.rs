//! Taskbar access: an official Jump List on Blink's taskbar icon (Open Blink,
//! Processes, Vitals, Restart as administrator) and an optional snippet for
//! Nilesoft Shell's taskbar background menu. See docs/taskbar.md.
//!
//! * The Jump List is `ICustomDestinationList` user tasks for Blink's app ID:
//!   plain shell links to the installed blink.exe with one-shot arguments
//!   (`logic::TASKS`). The shell keeps them, so they show for a pinned icon
//!   that is not running too. Blink writes them only from an installed copy
//!   (a portable copy or a test instance would leave global entries pointing
//!   at a throwaway folder), once per start at most, 8 s after start, and only
//!   when the list changed (`jumplist.ini`). Removal needs the same standing.
//! * Every change goes through one coordinator (`coord`): one operation at a
//!   time, the latest switch always wins, a timed-out shell call can never
//!   commit later, uninstall closes the door for good. Never on the UI thread;
//!   the UI thread never waits on the shell longer than a bounded uninstall wait.
//! * Windows has no supported way to add to the empty-taskbar menu. The
//!   Nilesoft snippet is text the user imports; Blink never edits shell.nss.
//! * A start with `--open` / `--tab <name>` is routed to the running Blink
//!   (`forward`). `--restart-admin` is NOT: the process it started asks UAC
//!   itself (`restart_admin`), because a posted window message cannot say who
//!   sent it and must never be able to cause a prompt.

pub mod coord;
pub mod logic;

use crate::osutil::{self, log};
use coord::{Backend, Coordinator, Final, Inputs, Step};
use logic::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use windows::core::*;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// The taskbar intent this process was started with.
pub fn intent() -> Option<Intent> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    parse_intent(&a)
}

/// A Blink is already running: hand it the intent (show / tab only) and say so.
/// The caller exits. False when nothing runs or the intent has no wire form.
pub fn forward(msg: u32, i: Intent) -> bool {
    let Some((wp, lp)) = to_wire(i) else { return false };
    let Some(h) = crate::setup::find_running() else { return false };
    let ok = unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        PostMessageW(Some(h), msg, WPARAM(wp), LPARAM(lp)).is_ok()
    };
    log(&format!("taskbar: {i:?} forwarded to the running Blink: {ok}"));
    ok
}

/// `--restart-admin` in an unelevated process: ask UAC for an elevated Blink
/// from HERE. Nothing is posted to a running Blink, so no other process can make
/// this prompt appear; the running one is untouched unless UAC is accepted (the
/// elevated copy then asks it to quit, `quit_replaced`). A No ends here.
pub fn restart_admin() {
    let running = crate::setup::find_running().map(|h| {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        pid
    });
    match admin_route(running.map(|pid| (pid, osutil::process_elevated(pid)))) {
        AdminRoute::ElevateNew => elevate(None),
        AdminRoute::ElevateReplace(pid) => elevate(Some(pid)),
        AdminRoute::Show => {
            log("taskbar: restart as administrator: the running Blink is elevated already (or its token is unreadable): showing it, no prompt");
            let _ = forward(crate::win::show_msg(), Intent::Open);
        }
    }
}

fn elevate(replace: Option<u32>) {
    let Ok(exe) = std::env::current_exe() else { return };
    let data = std::env::var("BLINK_DATA_DIR").ok().filter(|d| !d.is_empty());
    let r = classify(crate::setup::runas(&exe, &elevate_args(data, osutil::multi(), replace)));
    match r {
        Elevated::Started => log("taskbar: restart as administrator: UAC accepted, elevated Blink started"),
        Elevated::Cancelled => log("taskbar: restart as administrator: UAC cancelled, nothing started, the running Blink is untouched"),
        Elevated::Failed(e) => log(&format!("taskbar: restart as administrator failed: {e}")),
    }
}

/// An elevated copy started with `--wait-pid <pid>`: when `pid` is an UNELEVATED
/// Blink (the one this copy replaces after a Jump List UAC), ask it to quit.
/// The message is not let through the integrity filter, so it only reaches a
/// process that is not elevated, from a process that is.
pub fn quit_replaced(pid: u32) {
    if !osutil::is_elevated() || osutil::process_elevated(pid) != Some(false) {
        return;
    }
    let Some(h) = crate::setup::find_running() else { return };
    let mut p = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut p)) };
    if p == pid {
        unsafe {
            let _ = PostMessageW(Some(h), crate::win::quit_msg(), WPARAM(0), LPARAM(0));
        }
        log(&format!("taskbar: asked the unelevated Blink (pid {pid}) to quit"));
    }
}

// ---- Jump List -----------------------------------------------------------------

/// Registration (and removal) is allowed from here: an installed copy, or the
/// disposable test identity of a debug build.
pub fn allowed() -> bool {
    let i = Shell.inputs();
    eligible(&i.at, i.multi, i.test).is_ok()
}

pub fn enabled() -> bool {
    crate::setup::jumplist_enabled()
}

fn state_file() -> PathBuf {
    osutil::data_dir().join("jumplist.ini")
}

/// COM apartment for one operation thread (the shell's list is an apartment object).
struct Sta;

impl Sta {
    fn enter() -> Sta {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        }
        Sta
    }
}

impl Drop for Sta {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn key_title() -> PROPERTYKEY {
    // System.Title
    PROPERTYKEY { fmtid: GUID::from_u128(0xF29F85E0_4FF9_1068_AB91_08002B27B3D9), pid: 2 }
}

unsafe fn task_link(exe: &str, t: &Task) -> Result<IShellLinkW> {
    let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
    sl.SetPath(&HSTRING::from(exe))?;
    sl.SetArguments(&HSTRING::from(t.arguments()))?;
    sl.SetWorkingDirectory(&HSTRING::from(osutil::parent_dir(exe)))?;
    sl.SetDescription(&HSTRING::from(t.tip))?;
    sl.SetIconLocation(&HSTRING::from(exe), 0)?;
    let ps: IPropertyStore = sl.cast()?;
    crate::setup::set_string_prop(&ps, &key_title(), &osutil::wide(t.title))?;
    ps.Commit()?;
    Ok(sl)
}

/// Debug builds on the disposable test identity only: stretch every shell call
/// by BLINK_TEST_JUMPLIST_DELAY_MS before its commit gate, to exercise the
/// switch / uninstall races against the real list.
fn test_delay() {
    #[cfg(debug_assertions)]
    if crate::notify::test_aumid().is_some() {
        if let Some(ms) = std::env::var("BLINK_TEST_JUMPLIST_DELAY_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(Duration::from_millis(ms.min(60_000)));
        }
    }
}

/// Replace the app's user tasks. Runs on an STA thread; `gate` is asked right
/// before the only call that persists anything (`CommitList`).
unsafe fn write_list(exe: &str, aumid: &str, gate: &dyn Fn() -> bool) -> Result<Step> {
    let dl: ICustomDestinationList = CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
    dl.SetAppID(&HSTRING::from(aumid))?;
    let mut slots = 0u32;
    // tasks only: no custom category, so there are no user-removed destinations to honour
    let _removed: IObjectArray = dl.BeginList(&mut slots)?;
    let built = (|| -> Result<IObjectArray> {
        let col: IObjectCollection = CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
        for t in &TASKS {
            col.AddObject(&task_link(exe, t)?)?;
        }
        col.cast()
    })();
    if let Err(e) = built.and_then(|arr| dl.AddUserTasks(&arr)) {
        let _ = dl.AbortList();
        return Err(e);
    }
    test_delay();
    if !gate() {
        let _ = dl.AbortList();
        return Ok(Step::Refused);
    }
    match dl.CommitList() {
        Ok(()) => Ok(Step::Persisted),
        Err(e) => {
            let _ = dl.AbortList();
            Err(e)
        }
    }
}

unsafe fn delete_list(aumid: &str, gate: &dyn Fn() -> bool) -> Result<Step> {
    let dl: ICustomDestinationList = CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
    test_delay();
    if !gate() {
        return Ok(Step::Refused);
    }
    dl.DeleteList(&HSTRING::from(aumid)).map(|_| Step::Persisted)
}

/// The real shell, behind the coordinator.
struct Shell;

impl Backend for Shell {
    fn inputs(&self) -> Inputs {
        let st = crate::setup::status();
        Inputs {
            at: st.at.clone(),
            exe: st.exe.clone(),
            aumid: crate::notify::aumid(),
            enabled: enabled(),
            multi: osutil::multi(),
            test: crate::notify::test_aumid().is_some(),
        }
    }
    fn read_state(&self) -> State {
        std::fs::read_to_string(state_file()).map(|t| parse_state(&t)).unwrap_or_default()
    }
    fn write_state(&self, s: &State) {
        if let Err(e) = osutil::write_atomic(&state_file(), write_state(s).as_bytes()) {
            log(&format!("jumplist: cannot write jumplist.ini: {e}"));
        }
    }
    fn clear_state(&self) {
        let _ = std::fs::remove_file(state_file());
    }
    fn write_list(&self, exe: &str, aumid: &str, gate: &dyn Fn() -> bool) -> std::result::Result<Step, String> {
        let _sta = Sta::enter();
        unsafe { write_list(exe, aumid, gate) }.map_err(|e| e.to_string())
    }
    fn delete_list(&self, aumid: &str, gate: &dyn Fn() -> bool) -> std::result::Result<Step, String> {
        let _sta = Sta::enter();
        unsafe { delete_list(aumid, gate) }.map_err(|e| e.to_string())
    }
    fn now(&self) -> u64 {
        osutil::now_unix()
    }
    fn log(&self, line: &str) {
        log(line);
    }
}

static COORD: OnceLock<Arc<Coordinator<Shell>>> = OnceLock::new();

fn coord() -> &'static Arc<Coordinator<Shell>> {
    COORD.get_or_init(|| Coordinator::new(Shell, Duration::from_secs(20), Duration::from_secs(10)))
}

/// From the window start: reconcile the list once, a few seconds in.
pub fn start() {
    let _ = std::thread::Builder::new().name("jumplist-start".into()).spawn(|| {
        std::thread::sleep(Duration::from_secs(8));
        coord().request("start");
    });
}

/// Settings toggle changed: apply the latest position now, off the UI thread.
pub fn changed() {
    coord().request("setting changed");
}

/// Settings > Uninstall: take the list out for good (a list left behind points at
/// a deleted exe) and let no write reach the shell afterwards. Waits at most
/// `wait`; the caller is the UI thread. Nothing happens for a copy that never
/// owned the global list (portable, BLINK_MULTI).
pub fn remove_for_uninstall(wait: Duration) {
    let c = coord();
    if !c.remove_for_good("uninstall") {
        return;
    }
    log(&format!("jumplist: uninstall: {}", describe(c.wait_final(wait))));
}

fn describe(f: Final) -> &'static str {
    match f {
        Final::Removed => "removed",
        Final::GaveUp => "not removed (the shell refused or did not answer); no write can reach it any more",
        Final::Pending => "still being removed in the background; no write can reach the shell any more",
    }
}

/// After the window closed: give an uninstall's removal that is still running a
/// little more time before the process ends.
pub fn drain(wait: Duration) {
    if let Some(c) = COORD.get() {
        if c.is_closed() {
            log(&format!("jumplist: exit: {}", describe(c.wait_final(wait))));
        }
    }
}

/// `blink.exe --remove-jumplist` (install.ps1 -Uninstall): delete the list and
/// exit, no window. True when this process was started that way. A portable or
/// BLINK_MULTI copy removes nothing.
pub fn remove_cli() -> bool {
    if !std::env::args().skip(1).any(|a| a == ARG_REMOVE) {
        return false;
    }
    remove_for_uninstall(Duration::from_secs(25));
    true
}

// ---- Nilesoft Shell ----------------------------------------------------------------

static NOTE: Mutex<Option<(String, bool)>> = Mutex::new(None);

/// Status line under the Nilesoft buttons in Settings.
pub fn set_note_text(text: String, error: bool) {
    set_note(text, error)
}

fn set_note(text: String, error: bool) {
    *NOTE.lock().unwrap_or_else(|e| e.into_inner()) = Some((text, error));
}

/// Last result of Copy / Save in Settings: (text, is_error).
pub fn note() -> Option<(String, bool)> {
    NOTE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn clear_note() {
    *NOTE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Where Save puts the snippet: Blink's own data dir, never Nilesoft's.
pub fn snippet_path() -> PathBuf {
    osutil::data_dir().join("nilesoft").join("blink-taskbar.nss")
}

/// The snippet text for the exe this Blink runs from (installed copies only).
pub fn snippet() -> std::result::Result<String, String> {
    if !allowed() {
        return Err("Install Hayate first: a portable copy would leave a menu pointing at this folder".into());
    }
    nilesoft_snippet(&crate::setup::status().exe)
}

/// Save the snippet to `snippet_path()` on the I/O worker. BOTH paths are
/// checked first (blink.exe, which goes into the menu, and the file's own path,
/// which the user must write into `import '...'`); on a problem nothing is
/// created or written, and the note says what to change.
pub fn save_snippet() {
    if !allowed() {
        return set_note("Install Hayate first: a portable copy would leave a menu pointing at this folder".into(), true);
    }
    let path = snippet_path();
    let text = match nilesoft_save(&crate::setup::status().exe, &path.to_string_lossy()) {
        Ok(t) => t,
        Err(e) => return set_note(e, true),
    };
    set_note("Saving…".into(), false);
    crate::bgio::submit("nilesoft-snippet", move || {
        let r = path.parent().map(std::fs::create_dir_all).unwrap_or(Ok(())).and_then(|_| osutil::write_atomic(&path, text.as_bytes()));
        match r {
            Ok(()) => {
                log(&format!("taskbar: Nilesoft snippet saved to {}", path.display()));
                set_note(format!("Saved. Add this line to shell.nss: import '{}'", path.display()), false);
            }
            Err(e) => set_note(format!("Cannot save {}: {e}", path.display()), true),
        }
    });
}
