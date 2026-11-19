//! Window: custom chrome (no system caption), message loop, menus and the
//! process actions they trigger. UI state lives in a thread-local and is
//! never borrowed across a modal menu loop.

use crate::gfx::Gfx;
use crate::winplace;
use crate::sample::{self, Shared, WM_APP_DATA, WM_APP_TRAYTICK};
use crate::ui::procmenu::{build, Entry, Env, Target, ID_COPY_CMD, ID_COPY_PATH, ID_COPY_PID, ID_CPU_AFF, ID_CPU_SETS, ID_DISALLOW, ID_EFF_ONCE, ID_END, ID_END_TREE, ID_LOCATE, ID_PRIO, ID_PROPS, ID_RESTART, ID_RESTART_ADMIN, ID_R_AFF, ID_R_AWAKE, ID_R_BOOST, ID_R_EFF, ID_R_IO, ID_R_POWER, ID_R_PRIO, ID_R_REMOVE, ID_R_SETS, ID_R_SHOW, ID_R_VIEW, ID_SHOW, ID_TRIM};
use crate::ui::{Action, App};
use std::cell::RefCell;
use std::sync::{Arc, OnceLock};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_MOUSELEAVE: u32 = 0x02A3;
const CLASS: PCWSTR = w!("BlinkMain");
const CLASS_MULTI: PCWSTR = w!("BlinkMainMulti");

/// Window class; a BLINK_MULTI test instance gets its own so it never finds
/// (or is found by) the user's running Blink.
fn class() -> PCWSTR {
    if crate::osutil::multi() { CLASS_MULTI } else { CLASS }
}
const WM_APP_TOAST: u32 = WM_APP + 2;
const WM_APP_WATCH: u32 = WM_APP + 3;
/// the Task Manager switch's helper run (worker thread) is done: show its result
const WM_APP_TASKMGR: u32 = WM_APP + 5;
const WM_APP_INSTALLALL: u32 = WM_APP + 0x21;
const WM_APP_TRAY: u32 = WM_APP + 4;
/// Windows toast clicked (notify.rs, WinRT thread); WPARAM = Watch event id
const WM_APP_TOASTCLICK: u32 = WM_APP + 0x20;
/// hover dwell of the Processes list (ui/cellhover.rs): one-shot repaint
const TIMER_DWELL: usize = 0xB10F;
/// the transient "Zoom 125%" indicator expires: one-shot repaint
const TIMER_ZOOM: usize = 0xB110;
const NIN_SELECT: u32 = WM_USER;
const NIN_KEYSELECT: u32 = WM_USER + 1;
const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;

static TOASTS: std::sync::Mutex<std::collections::VecDeque<crate::watch::Toast>> = std::sync::Mutex::new(std::collections::VecDeque::new());
static LAST_TOAST_EVENT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

thread_local! {
    static STATE: RefCell<Option<(App, Gfx)>> = const { RefCell::new(None) };
}
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

/// The sampler's published state (None before the window exists).
pub fn shared() -> Option<&'static Arc<Shared>> {
    SHARED.get()
}
static SHOW_MSG: OnceLock<u32> = OnceLock::new();
static RESIDENT: OnceLock<bool> = OnceLock::new();

fn with<R>(f: impl FnOnce(&mut App, &mut Gfx) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.as_mut().map(|(a, g)| f(a, g))))
}

thread_local! {
    /// A modal loop (menu, move/size, UAC prompt) ran inside the current dispatch.
    static MODAL_SEEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Mark the start/end of a modal loop on the UI thread: that time belongs to
/// the user, not to a stall.
fn modal(on: bool) {
    crate::timing::UI_MODAL.store(on, std::sync::atomic::Ordering::Relaxed);
    if on {
        MODAL_SEEN.with(|m| m.set(true));
    }
}

/// Dispatch one message, timing it for `stall: ui-msg` (and for the
/// sampler's `ui-hung` watchdog while it runs).
unsafe fn dispatch(msg: &MSG) {
    use std::sync::atomic::Ordering::Relaxed;
    use crate::timing::{now_us, UI_BUSY_SINCE, UI_MSG};
    let t0 = now_us();
    UI_MSG.store(msg.message, Relaxed);
    UI_BUSY_SINCE.store(t0, Relaxed);
    MODAL_SEEN.with(|m| m.set(false));
    let _ = TranslateMessage(msg);
    DispatchMessageW(msg);
    UI_BUSY_SINCE.store(0, Relaxed);
    let ms = now_us().saturating_sub(t0) as f32 / 1000.0;
    if ms > 250.0 && !MODAL_SEEN.with(|m| m.get()) {
        let m = msg.message;
        crate::timing::stall(crate::timing::Stall::UiMsg, ms, || format!("ui-msg: {} took {ms:.0} ms on the UI thread", crate::timing::msg_name(m)));
    }
}

/// Posted by an ELEVATED copy that replaces this unelevated Blink after UAC was
/// accepted. Not let through the integrity filter (unlike `show_msg`): only a
/// process of this integrity or higher can post it, which could end this process
/// anyway.
pub fn quit_msg() -> u32 {
    static Q: OnceLock<u32> = OnceLock::new();
    *Q.get_or_init(|| unsafe { RegisterWindowMessageW(w!("BlinkQuitForAdminRestart")) })
}

pub fn show_msg() -> u32 {
    *SHOW_MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("BlinkShowWindow")) })
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    let resident = args.iter().any(|a| a == "--resident");
    RESIDENT.set(resident).ok();
    if resident {
        crate::timing::mark("resident (starts hidden; data shown on first open)");
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // elevated relaunch: wait for the instance that started us to exit
        if let Some(i) = args.iter().position(|a| a == "--wait-pid") {
            if let Some(pid) = args.get(i + 1).and_then(|v| v.parse::<u32>().ok()) {
                // an elevated copy that replaces an unelevated Blink (Jump List "Restart as administrator", UAC accepted) asks it to quit
                if args.iter().any(|a| a == "--replace-for-admin") {
                    crate::taskbar::quit_replaced(pid);
                }
                if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                    WaitForSingleObject(h, 5000);
                    let _ = CloseHandle(h);
                }
            }
        }
        // notification-centre click (-ToastActivated) with Blink running: open Watch there
        if crate::setup::forward(show_msg()) {
            return;
        }
        // taskbar launch (Jump List task, Nilesoft item): the running Blink gets it
        let intent = crate::taskbar::intent();
        if let Some(i) = intent {
            use crate::taskbar::logic::Intent;
            if i == Intent::RestartAdmin && !crate::osutil::is_elevated() {
                // asked here, by this process, never by a message to the running Blink
                // (anything could post one): UAC, and a No changes nothing
                crate::taskbar::restart_admin();
                return;
            }
            // already elevated: nothing to restart, it is just a launch that shows the window
            let i = if i == Intent::RestartAdmin { Intent::Open } else { i };
            if crate::taskbar::forward(show_msg(), i) {
                return;
            }
        }
        // second instance (or taskmgr hijack): wake the running one and exit
        if let Ok(h) = FindWindowW(class(), None) {
            if !h.is_invalid() && !crate::osutil::multi() {
                let _ = PostMessageW(Some(h), show_msg(), WPARAM(0), LPARAM(0));
                return;
            }
        }
        dark_menus();
        // before the first window: taskbar button, toasts and the Start-menu
        // shortcut (install.ps1) share this identity
        crate::notify::set_app_id();
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap(),
            hIcon: app_icon(hinst, SM_CXICON, GetDpiForSystem()),
            hIconSm: app_icon(hinst, SM_CXSMICON, GetDpiForSystem()),
            lpszClassName: class(),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        // size: remembered (window.ini) or 1120×720 DIPs centred on the primary work area
        crate::zoom::load();
        let mons = monitors();
        let saved = std::fs::read_to_string(crate::osutil::data_dir().join("window.ini")).ok().and_then(|t| winplace::parse_ini(&t));
        let res = winplace::resolve(saved.as_ref(), &mons, (1120, 720), (640, 420)).unwrap_or_else(|| {
            let dpi = GetDpiForSystem();
            let s = dpi as f32 / 96.0 * crate::zoom::factor();
            let r = winplace::Rect { left: 0, top: 0, right: (1120.0 * s) as i32, bottom: (720.0 * s) as i32 };
            winplace::Resolved { rect: r, maximized: false, from_saved: false }
        });
        PENDING_MAX.store(res.maximized, std::sync::atomic::Ordering::Relaxed);
        let (x, y, w, h) = (res.rect.left, res.rect.top, res.rect.w(), res.rect.h());
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class(),
            w!("Hayate"),
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        )
        .unwrap();
        crate::timing::mark("window created");

        set_window_icons(hwnd);
        let dark: BOOL = TRUE;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
        let border: u32 = 0x00312823; // COLORREF 0x00BBGGRR of #232831
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as *const _, 4);
        // let a non-elevated launcher (taskmgr hijack) poke this elevated window
        let _ = ChangeWindowMessageFilterEx(hwnd, show_msg(), MSGFLT_ALLOW, None);
        // force WM_NCCALCSIZE with our handler
        let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER);

        crate::ui::icons::set_hwnd(hwnd);
        let shared = Shared::new();
        shared.hwnd.store(hwnd.0 as isize, std::sync::atomic::Ordering::Relaxed);
        SHARED.set(shared.clone()).ok();

        let mut gfx = crate::timing::time("gfx init", || Gfx::new(hwnd).unwrap());
        gfx.set_dpi(GetDpiForWindow(hwnd) as f32);
        gfx.set_zoom(crate::zoom::factor());
        STATE.with(|s| *s.borrow_mut() = Some((App::new(), gfx)));
        crate::timing::mark("app state ready");

        sample::spawn(shared.clone());

        // Rules + Watch backend; callbacks run on worker threads → post to UI
        let hw = hwnd.0 as isize;
        crate::timing::mark("features init start");
        crate::features::init(
            move |t| {
                if let Ok(mut q) = TOASTS.lock() {
                    q.push_back(t);
                }
                let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_TOAST, WPARAM(0), LPARAM(0));
            },
            move || {
                let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_WATCH, WPARAM(0), LPARAM(0));
            },
        );
        crate::notify::on_click(move |id| {
            let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_TOASTCLICK, WPARAM(id as usize), LPARAM(0));
        });
        crate::timing::mark("features init done");
        // startup manager: loads its list and applies the logon rules (starts nothing unless every rule allows it)
        crate::startup::run::init(args.clone());
        TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated"))).ok();
        crate::timing::time("tray added", || tray_add(hwnd));

        // self-update: timer, and window state / tab of the instance we replaced
        crate::update::start(hwnd);
        // an ELEVATED installed Blink keeps the protected Task Manager launcher current (no extra prompt)
        {
            let hw = hwnd.0 as isize;
            crate::taskmgr::set_repaint(move || {
                let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_TASKMGR, WPARAM(0), LPARAM(0));
            });
            crate::taskmgr::auto_refresh(move || {
                let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_TASKMGR, WPARAM(0), LPARAM(0));
            });
        }
        if let Some(t) = crate::update::restore_tab().and_then(crate::ui::Tab::from_restore_id) {
            with(|a, _| a.switch_tab(t));
        }
        // an update relaunch brings its own window state; otherwise the remembered maximized flag applies on first show
        if crate::update::restore_tab().is_some() {
            PENDING_MAX.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        let shown = crate::update::restore_window(hwnd, resident, show, show_quiet);
        PLACE_READY.store(true, std::sync::atomic::Ordering::Relaxed);
        if shown != crate::update::logic::Show::Hidden {
            crate::timing::mark("window shown");
        }
        if let Some(id) = crate::setup::start_watch() {
            open_watch(hwnd, id);
        }
        if let Some(i) = intent {
            apply_intent(hwnd, i);
        }
        // installed copies write the taskbar Jump List a few seconds in (own thread)
        crate::taskbar::start();

        crate::timing::UI_THREAD.store(crate::timing::own_thread_handle(), std::sync::atomic::Ordering::Relaxed);
        // Input and paint must not queue behind normal-priority work on a busy
        // machine (Task Manager runs at high priority for the same reason).
        // The UI thread sleeps in GetMessage when idle, so this costs nothing.
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            dispatch(&msg);
        }
        // WM_DESTROY normally did this; covers a destroy that ran while STATE was borrowed
        drop_state();
        crate::osutil::log(&format!("exit: message loop ended ({})", EXIT_WHY.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| "no reason recorded".into())));
    }
}

/// Drop the App and its Direct2D/D3D objects now, on the UI thread, while the
/// process is fully alive. Left to the thread-local destructor they would be
/// released inside ExitProcess (LdrShutdownProcess → FLS cleanup), after the
/// other threads and the thread pool are gone: D3D's device teardown then
/// schedules work on the dead pool and ntdll raises STATUS_INVALID_PARAMETER
/// (c000000d, TppRaiseInvalidParameter) — the crash on closing a non-resident
/// Blink.
fn drop_state() {
    let t = std::time::Instant::now();
    let taken = STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.take()));
    if let Some(st) = taken {
        drop(st);
        crate::osutil::log(&format!("exit: ui state released in {} ms", t.elapsed().as_millis()));
    }
}

/// Startup timing for paints: the first one, the first with any snapshot, the
/// first with real (delta-based) Vitals numbers (that writes the summary line).
fn startup_paint(seq: u64, ready: bool, vitals: bool) {
    use std::sync::atomic::{AtomicU8, Ordering};
    static STAGE: AtomicU8 = AtomicU8::new(0);
    let st = STAGE.load(Ordering::Relaxed);
    if st == 0 {
        STAGE.store(1, Ordering::Relaxed);
        crate::timing::mark(&format!("first paint (snapshot seq {seq})"));
    }
    if st <= 1 && seq > 0 && !ready {
        STAGE.store(2, Ordering::Relaxed);
        crate::timing::mark(&format!("first paint with partial snapshot (seq {seq})"));
    }
    if ready {
        STAGE.store(3, Ordering::Relaxed);
        crate::timing::finish(&format!("first paint with real data (seq {seq}{})", if vitals { "" } else { ", not on Vitals" }));
    }
}

static PENDING_MAX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PLACE_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static IN_SIZEMOVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAST_PLACE: std::sync::Mutex<Option<winplace::Saved>> = std::sync::Mutex::new(None);

/// Monitors with work area + effective DPI, primary first.
fn monitors() -> Vec<winplace::Mon> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        let v = &mut *(lp.0 as *mut Vec<(bool, winplace::Mon)>);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(h, &mut mi).as_bool() {
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let w = mi.rcWork;
            v.push((
                mi.dwFlags & MONITORINFOF_PRIMARY != 0,
                winplace::Mon { work: winplace::Rect { left: w.left, top: w.top, right: w.right, bottom: w.bottom }, dpi: dx.max(48) },
            ));
        }
        TRUE
    }
    let mut v: Vec<(bool, winplace::Mon)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v.sort_by_key(|(p, _)| !*p);
    v.into_iter().map(|(_, m)| m).collect()
}

/// Write the main window's normal rect + maximized flag to window.ini when it changed.
/// A minimized window keeps its restore rect; nothing is written before startup finished.
fn save_placement(hwnd: HWND) {
    use std::sync::atomic::Ordering::Relaxed;
    // the cold-started elevated launcher child never writes the medium-writable data dir (see osutil::cold_started)
    if !PLACE_READY.load(Relaxed) || crate::osutil::cold_started() {
        return;
    }
    unsafe {
        let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        if GetWindowPlacement(hwnd, &mut wp).is_err() {
            return;
        }
        let maximized = IsZoomed(hwnd).as_bool() || (IsIconic(hwnd).as_bool() && wp.flags.0 & WPF_RESTORETOMAXIMIZED.0 != 0);
        let rect = if maximized || IsIconic(hwnd).as_bool() {
            // rcNormalPosition is in workspace coordinates: offset by the primary taskbar insets
            let pm = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(pm, &mut mi);
            let (dx, dy) = (mi.rcWork.left - mi.rcMonitor.left, mi.rcWork.top - mi.rcMonitor.top);
            let n = wp.rcNormalPosition;
            winplace::Rect { left: n.left + dx, top: n.top + dy, right: n.right + dx, bottom: n.bottom + dy }
        } else {
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() {
                return;
            }
            winplace::Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
        };
        let s = winplace::Saved { rect, dpi: GetDpiForWindow(hwnd).max(48), maximized };
        let Ok(mut last) = LAST_PLACE.lock() else { return };
        if *last == Some(s) {
            return;
        }
        *last = Some(s);
        let path = crate::osutil::data_dir().join("window.ini");
        if let Err(e) = crate::osutil::write_atomic(&path, winplace::render_ini(&s).as_bytes()) {
            crate::osutil::log(&format!("window.ini not saved: {e}"));
        }
    }
}

fn show(hwnd: HWND) {
    crate::timing::shown();
    unsafe {
        if let Some(s) = SHARED.get() {
            s.set_visible(true);
            with(|a, _| a.set_snapshot(s.get()));
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else if PENDING_MAX.swap(false, std::sync::atomic::Ordering::Relaxed) {
            let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

/// ShowWindow commands that leave the window minimized (not visible to the sampler).
fn is_min_cmd(cmd: SHOW_WINDOW_CMD) -> bool {
    matches!(cmd, SW_MINIMIZE | SW_SHOWMINIMIZED | SW_SHOWMINNOACTIVE | SW_FORCEMINIMIZE)
}

/// What a WM_SIZE does to the sampler's `minimized` flag (a minimized window is not on screen,
/// so the probes do not send). None = leave it alone.
fn minimized_after_size(kind: u32) -> Option<bool> {
    match kind {
        SIZE_MINIMIZED => Some(true),
        SIZE_RESTORED | SIZE_MAXIMIZED => Some(false),
        _ => None,
    }
}

/// Visible without activating (update relaunch behind other windows / minimized).
fn show_quiet(hwnd: HWND, cmd: SHOW_WINDOW_CMD) {
    crate::timing::shown();
    sample::set_minimized(is_min_cmd(cmd));
    if let Some(s) = SHARED.get() {
        s.set_visible(true);
        with(|a, _| a.set_snapshot(s.get()));
    }
    unsafe {
        let _ = ShowWindow(hwnd, cmd);
    }
}

/// Window close (X): hide to the tray when resident, else quit.
static LAST_DEACTIVATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tray_toggle(hwnd: HWND) {
    unsafe {
        let since = windows::Win32::System::SystemInformation::GetTickCount64().saturating_sub(LAST_DEACTIVATED.load(std::sync::atomic::Ordering::Relaxed));
        let in_front = GetForegroundWindow() == hwnd || since < 500;
        crate::osutil::log(&format!("tray toggle: visible={} in_front={in_front} (deactivated {since} ms ago)", IsWindowVisible(hwnd).as_bool()));
        if IsWindowVisible(hwnd).as_bool() && in_front {
            hide_to_tray(hwnd);
        } else {
            show(hwnd);
        }
    }
}

/// Why the window is being destroyed, set by whoever asks for it (None at WM_DESTROY: nobody
/// in this program did, so the log says so and a quit like that is never called "clean").
static EXIT_WHY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Close the window for a stated reason, which goes into blink.log (`exit: ...`). `user`: the
/// user closed it on purpose (X, Esc, tray Quit); an update trial then knows this is not a crash.
fn destroy(hwnd: HWND, why: &str, user: bool) {
    crate::osutil::log(&format!("exit: closing the window: {why}{}", if user { " (user)" } else { "" }));
    *EXIT_WHY.lock().unwrap_or_else(|e| e.into_inner()) = Some(why.to_string());
    if user {
        crate::update::note_user_quit();
    }
    save_placement(hwnd);
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn hide(hwnd: HWND, src: &str) {
    if *RESIDENT.get().unwrap_or(&false) {
        hide_to_tray(hwnd);
    } else {
        destroy(hwnd, &format!("{src}, not resident so it quits"), true);
    }
}

/// Hide the window and free its GPU/icon state; the process keeps running
/// (tray icon left-click toggle, resident close).
fn hide_to_tray(hwnd: HWND) {
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        if let Some(s) = SHARED.get() {
            s.set_visible(false);
        }
        with(|a, g| {
            g.release();
            crate::osutil::log(&format!("hide: icon cache {} icons, {} KB", a.icons.len(), a.icons.bytes() / 1024));
        });
        // drop working set: we are idle until shown again
        let _ = windows::Win32::System::ProcessStatus::EmptyWorkingSet(GetCurrentProcess());
    }
}

/// Undocumented uxtheme ordinals that make native popup menus dark.
fn dark_menus() {
    unsafe {
        if let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) {
            if let Some(p) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
                let f: extern "system" fn(i32) -> i32 = std::mem::transmute(p);
                f(2); // ForceDark
            }
            if let Some(p) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
                let f: extern "system" fn() = std::mem::transmute(p);
                f();
            }
        }
    }
}

/// Blink's icon (resource 1, multi-size) at the `metric` size for `dpi`.
fn app_icon(hinst: HINSTANCE, metric: SYSTEM_METRICS_INDEX, dpi: u32) -> HICON {
    unsafe {
        let sz = GetSystemMetricsForDpi(metric, dpi);
        LoadImageW(Some(hinst), PCWSTR(1 as *const u16), IMAGE_ICON, sz, sz, LR_DEFAULTCOLOR)
            .map(|h| HICON(h.0))
            .unwrap_or_default()
    }
}

/// Taskbar / Alt-Tab icons at the window's current DPI; frees the ones
/// they replace (always ours: class icons are not returned by WM_SETICON).
fn set_window_icons(hwnd: HWND) {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let dpi = GetDpiForWindow(hwnd).max(96);
        for (which, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
            let icon = app_icon(hinst, metric, dpi);
            if icon.is_invalid() {
                continue;
            }
            let old = SendMessageW(hwnd, WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(icon.0 as isize)));
            if old.0 != 0 {
                let _ = DestroyIcon(HICON(old.0 as *mut _));
            }
        }
    }
}

/// System DPI scale of the window (physical pixels per DIP, no user zoom):
/// resize borders and other OS-sized things.
fn dpi_scale(hwnd: HWND) -> f32 {
    unsafe { GetDpiForWindow(hwnd) as f32 / 96.0 }
}

/// Pixels per logical unit of the UI: system DPI x user zoom.
fn ui_scale(hwnd: HWND) -> f32 {
    dpi_scale(hwnd) * crate::zoom::factor()
}

fn pt(hwnd: HWND, l: LPARAM) -> (f32, f32) {
    let x = (l.0 & 0xffff) as i16 as f32;
    let y = ((l.0 >> 16) & 0xffff) as i16 as f32;
    let s = ui_scale(hwnd);
    (crate::zoom::to_logical(x, s), crate::zoom::to_logical(y, s))
}

/// Smallest outer window size (px): 640x420 logical, kept inside the work area.
fn min_track(hwnd: HWND) -> (i32, i32) {
    let s = ui_scale(hwnd);
    let (mut w, mut h) = ((640.0 * s) as i32, (420.0 * s) as i32);
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            w = w.min(mi.rcWork.right - mi.rcWork.left);
            h = h.min(mi.rcWork.bottom - mi.rcWork.top);
        }
    }
    (w, h)
}

/// Apply a zoom command: new scale, window grown if it would clip, toast, save.
fn apply_zoom(hwnd: HWND, cmd: crate::zoom::Cmd) {
    let new = crate::zoom::apply(cmd, crate::zoom::percent());
    let changed = new != crate::zoom::percent();
    crate::zoom::set(new);
    with(|a, g| {
        g.set_zoom(crate::zoom::factor());
        a.zoom_shown = Some(std::time::Instant::now());
    });
    unsafe {
        if changed {
            crate::zoom::save();
            // same pixel size, re-laid-out: only grow when the window is now below its minimum
            if !IsZoomed(hwnd).as_bool() && !IsIconic(hwnd).as_bool() {
                let (mw, mh) = min_track(hwnd);
                let mut rc = RECT::default();
                if GetWindowRect(hwnd, &mut rc).is_ok() {
                    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
                    if w < mw || h < mh {
                        let _ = SetWindowPos(hwnd, None, rc.left, rc.top, w.max(mw), h.max(mh), SWP_NOZORDER | SWP_NOACTIVATE);
                    }
                }
            }
        }
        SetTimer(Some(hwnd), TIMER_ZOOM, 1500, None);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == show_msg() {
        // wp 1: a toast click forwarded by `setup::forward`, lp = Watch event id;
        // wp 2: a taskbar launch (`taskbar::logic::to_wire`); show or tab only, never UAC
        match wp.0 {
            1 => open_watch(hwnd, lp.0 as u64),
            // 3: the Task Manager redirect's launcher (src/taskmgr): show in front, nothing else
            3 => show(hwnd),
            _ => match crate::taskbar::logic::from_wire(wp.0, lp.0) {
                Some(i) => apply_intent(hwnd, i),
                None => show(hwnd),
            },
        }
        return LRESULT(0);
    }
    if msg == quit_msg() {
        if !crate::osutil::is_elevated() {
            crate::osutil::log("restart as administrator: the elevated copy asked this one to quit");
            destroy(hwnd, "the elevated copy asked this one to quit", false);
        }
        return LRESULT(0);
    }
    match msg {
        WM_NCCALCSIZE if wp.0 != 0 => {
            let p = &mut *(lp.0 as *mut NCCALCSIZE_PARAMS);
            let top = p.rgrc[0].top;
            let r = DefWindowProcW(hwnd, msg, wp, lp);
            p.rgrc[0].top = top;
            if IsZoomed(hwnd).as_bool() {
                let dpi = GetDpiForWindow(hwnd);
                p.rgrc[0].top += GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
            }
            r
        }
        WM_NCHITTEST => {
            let r = DefWindowProcW(hwnd, msg, wp, lp);
            if r.0 as u32 != HTCLIENT {
                return r;
            }
            let mut p = POINT { x: (lp.0 & 0xffff) as i16 as i32, y: ((lp.0 >> 16) & 0xffff) as i16 as i32 };
            let _ = ScreenToClient(hwnd, &mut p);
            let s = dpi_scale(hwnd);
            if !IsZoomed(hwnd).as_bool() && (p.y as f32) < 5.0 * s {
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                return LRESULT(if (p.x as f32) < 12.0 * s {
                    HTTOPLEFT
                } else if (p.x as f32) > rc.right as f32 - 12.0 * s {
                    HTTOPRIGHT
                } else {
                    HTTOP
                } as isize);
            }
            let us = ui_scale(hwnd);
            let cap = with(|a, _| a.is_caption(crate::zoom::to_logical(p.x as f32, us), crate::zoom::to_logical(p.y as f32, us))).unwrap_or(false);
            LRESULT(if cap { HTCAPTION } else { HTCLIENT } as isize)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            BeginPaint(hwnd, &mut ps);
            let t0 = std::time::Instant::now();
            let seq = with(|a, g| {
                a.paint(g);
                let ms = t0.elapsed().as_secs_f32() * 1000.0;
                crate::timing::paint_sample(ms, g.end_ms, g.create_ms, || {
                    format!("{:?}, {} target, {:.0}x{:.0} px, {}", a.tab, g.path(), a.w * g.dpi / 96.0, a.h * g.dpi / 96.0, crate::timing::mem_line())
                });
                if ms > 100.0 {
                    let (end, create, tab, dpi) = (g.end_ms, g.create_ms, a.tab, g.dpi);
                    let (w, h) = (a.w, a.h);
                    crate::timing::stall(crate::timing::Stall::Paint, ms, || {
                        format!(
                            "paint: {ms:.0} ms on {tab:?} = draw calls {:.0} ms + EndDraw (rasterise, present) {end:.0} ms + target creation {create:.0} ms; {} target, {:.0}x{:.0} px",
                            (ms - end - create).max(0.0),
                            g.path(),
                            w * dpi / 96.0,
                            h * dpi / 96.0
                        )
                    });
                }
                (a.snap.seq, a.snap.ready, a.tab == crate::ui::Tab::Vitals, a.timer_ms.take())
            });
            let _ = EndPaint(hwnd, &ps);
            if let Some((_, _, _, Some(ms))) = seq {
                SetTimer(Some(hwnd), TIMER_DWELL, ms.max(15), None);
            }
            if crate::timing::active() {
                if let Some((seq, ready, vitals, _)) = seq {
                    startup_paint(seq, ready, vitals);
                }
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            let w = (lp.0 & 0xffff) as u32;
            let h = ((lp.0 >> 16) & 0xffff) as u32;
            with(|a, g| {
                g.resize(w, h);
                a.maximized = wp.0 as u32 == SIZE_MAXIMIZED;
            });
            // maximize / restore by button, key or double click (a drag-resize saves at its end)
            if matches!(wp.0 as u32, SIZE_MAXIMIZED | SIZE_RESTORED) && !IN_SIZEMOVE.load(std::sync::atomic::Ordering::Relaxed) {
                save_placement(hwnd);
            }
            // minimized is not on screen: the opt-in network probes go quiet (the rest of the sampler is unchanged)
            if let Some(m) = minimized_after_size(wp.0 as u32) {
                sample::set_minimized(m);
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let dpi = (wp.0 & 0xffff) as f32;
            with(|_, g| g.set_dpi(dpi));
            set_window_icons(hwnd);
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mi = &mut *(lp.0 as *mut MINMAXINFO);
            let (w, h) = min_track(hwnd);
            mi.ptMinTrackSize = POINT { x: w, y: h };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            let _ = TrackMouseEvent(&mut tme);
            let (x, y) = pt(hwnd, lp);
            act(hwnd, with(|a, _| a.mouse_move(x, y)));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            act(hwnd, with(|a, _| a.mouse_leave()));
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = pt(hwnd, lp);
            act(hwnd, with(|a, _| a.mouse_down(x, y)));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = ReleaseCapture();
            let (x, y) = pt(hwnd, lp);
            act(hwnd, with(|a, _| a.mouse_up(x, y)));
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = pt(hwnd, lp);
            act(hwnd, with(|a, _| a.double_click(x, y)));
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let (x, y) = pt(hwnd, lp);
            act(hwnd, with(|a, _| a.right_click(x, y)));
            LRESULT(0)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let d = ((wp.0 >> 16) & 0xffff) as i16 as f32;
            let shift = msg == WM_MOUSEHWHEEL || (wp.0 & 0x0004) != 0;
            // Ctrl + wheel zooms the whole interface
            if msg == WM_MOUSEWHEEL && (wp.0 & 0x0008) != 0 {
                if let Some(c) = crate::zoom::wheel_cmd(d) {
                    apply_zoom(hwnd, c);
                }
                return LRESULT(0);
            }
            let d = if msg == WM_MOUSEHWHEEL { -d } else { d };
            act(hwnd, with(|a, _| a.wheel(d, shift)));
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
            let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
            match with(|a, _| a.key(wp.0 as u16, ctrl, shift)).flatten() {
                Some(action) => {
                    act(hwnd, Some(action));
                    LRESULT(0)
                }
                None => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
        // Alt+1..4 on the Services tab: the quick startup types (never while a text field, menu or
        // panel has the keyboard; ui/mod.rs `alt_digit`). Anything else is the system's.
        WM_SYSKEYDOWN => {
            let vk = wp.0 as u16;
            // bit 30: the key was already down (auto-repeat): swallowed, never a second change
            let repeat = (lp.0 >> 30) & 1 != 0;
            let taken = if (0x31..=0x34).contains(&vk) { with(|a, _| a.alt_digit((vk - 0x31) as usize, repeat)).flatten() } else { None };
            match taken {
                Some(action) => {
                    act(hwnd, Some(action));
                    LRESULT(0)
                }
                None => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
        WM_CHAR => {
            if let Some(c) = char::from_u32(wp.0 as u32) {
                act(hwnd, with(|a, _| a.char(c)));
            }
            LRESULT(0)
        }
        WM_APP_DATA => {
            // the sampler posts again only after this: one message however long we were busy
            let posted = sample::DATA_POSTED.swap(0, std::sync::atomic::Ordering::SeqCst);
            let folded = sample::COALESCED.swap(0, std::sync::atomic::Ordering::Relaxed);
            let waited = crate::timing::now_us().saturating_sub(posted) as f32 / 1000.0;
            if posted != 0 && waited > 500.0 {
                crate::timing::stall(crate::timing::Stall::UiQueue, waited, || {
                    format!("ui-queue: data message waited {waited:.0} ms for the UI thread; {folded} newer sampler ticks folded into it (painted once)")
                });
            }
            if let Some(s) = SHARED.get() {
                let snap = s.get();
                tray_feed(hwnd, Some(snap.clone()), false);
                if with(|a, _| a.set_snapshot(snap)).unwrap_or(true) {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        crate::ui::icons::WM_APP_ICONS => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_APP_TRAYTICK => {
            sample::TRAY_POSTED.store(0, std::sync::atomic::Ordering::SeqCst);
            sample::COALESCED.store(0, std::sync::atomic::Ordering::Relaxed);
            if let Some(s) = SHARED.get() {
                tray_feed(hwnd, Some(s.get()), false);
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == crate::update::TIMER_CHECK || wp.0 == crate::update::TIMER_READY => {
            let tab = with(|a, _| a.tab.restore_id()).unwrap_or(0);
            if crate::update::on_timer(hwnd, wp.0, tab) {
                destroy(hwnd, "self-update swapped the files and relaunches", false);
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_ZOOM => {
            let _ = KillTimer(Some(hwnd), TIMER_ZOOM);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_DWELL => {
            let _ = KillTimer(Some(hwnd), TIMER_DWELL);
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_ENTERSIZEMOVE | WM_ENTERMENULOOP => {
            if msg == WM_ENTERSIZEMOVE {
                IN_SIZEMOVE.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            modal(true);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_EXITSIZEMOVE | WM_EXITMENULOOP => {
            modal(false);
            let r = DefWindowProcW(hwnd, msg, wp, lp);
            if msg == WM_EXITSIZEMOVE {
                IN_SIZEMOVE.store(false, std::sync::atomic::Ordering::Relaxed);
                save_placement(hwnd);
            }
            r
        }
        WM_ACTIVATE => {
            if (wp.0 & 0xffff) == 0 {
                // WA_INACTIVE: remember when, so a tray click (the taskbar takes focus first) still counts as "was in front"
                LAST_DEACTIVATED.store(windows::Win32::System::SystemInformation::GetTickCount64(), std::sync::atomic::Ordering::Relaxed);
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_CLOSE => {
            // the X of Blink's own caption is `Action::Hide`; this is a WM_CLOSE sent by
            // Windows (ghost window, Alt+F4, taskbar Close window) or by another program
            hide(hwnd, "WM_CLOSE (Alt+F4, taskbar, or another program)");
            LRESULT(0)
        }
        WM_ENDSESSION => {
            crate::osutil::log(&format!("exit: Windows is ending the session (ending={})", wp.0 != 0));
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_APP_INSTALLALL => {
            // "Install for everyone" finished (UAC + copy ran on a worker thread)
            match crate::setup::take_install_all_result() {
                Some(Ok(true)) => {
                    destroy(hwnd, "installed for everyone: the installed copy was started", false);
                    return LRESULT(0);
                }
                Some(Ok(false)) => {
                    with(|a, _| a.setup.error = None);
                }
                Some(Err(e)) => {
                    with(|a, _| a.setup.error = Some(e));
                }
                None => {}
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_APP_TASKMGR => {
            // the Task Manager switch's helper finished (it ran on a worker thread)
            if let Some(r) = crate::taskmgr::take_result() {
                with(|a, _| a.setup.error = r.err());
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        WM_DESTROY => {
            if EXIT_WHY.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                crate::osutil::log("exit: the window was destroyed and nothing in Blink asked for it (an outside DestroyWindow, or a quit path that logs no reason)");
            }
            // quit paths that skip `destroy` (tray menu, update, Alt+F4 on a hidden-then-shown window)
            if IsWindowVisible(hwnd).as_bool() {
                save_placement(hwnd);
            }
            tray_delete(hwnd);
            if let Some(f) = crate::features::get() {
                f.shutdown();
            }
            // release D2D/D3D while the window and the thread pool still exist
            drop_state();
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_APP_TOAST => {
            tray_feed(hwnd, None, true);
            LRESULT(0)
        }
        WM_APP_WATCH => {
            if IsWindowVisible(hwnd).as_bool() {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_APP_TRAY => {
            let ev = (lp.0 & 0xffff) as u32;
            match ev {
                NIN_SELECT | NIN_KEYSELECT => tray_toggle(hwnd),
                WM_LBUTTONUP if !TRAY_V4.load(std::sync::atomic::Ordering::Relaxed) => tray_toggle(hwnd),
                NIN_BALLOONUSERCLICK => open_watch(hwnd, LAST_TOAST_EVENT.load(std::sync::atomic::Ordering::Relaxed)),
                WM_CONTEXTMENU | WM_RBUTTONUP => tray_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_TOASTCLICK => {
            open_watch(hwnd, wp.0 as u64);
            LRESULT(0)
        }
        m if Some(&m) == TASKBAR_CREATED.get() => {
            tray_add(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn act(hwnd: HWND, a: Option<Action>) {
    let Some(a) = a else { return };
    unsafe {
        match a {
            Action::None => return,
            Action::Repaint => {}
            Action::Capture => {
                SetCapture(hwnd);
            }
            Action::TestToast => {
                LAST_TOAST_EVENT.store(0, std::sync::atomic::Ordering::Relaxed);
                // Shell_NotifyIcon can block on a slow Explorer: never on the UI thread
                let hw = hwnd.0 as isize;
                std::thread::spawn(move || {
                    let (title, text) = ("Hayate test notification", "Watch alerts show up like this.");
                    if !crate::notify::toast(crate::notify::Note { title: title.into(), text: text.into(), kind: "test", event_id: None }) {
                        notify_balloon(HWND(hw as *mut _), title, text, false);
                    }
                });
            }
            Action::ApplyUpdate(tab) => {
                // elevated self-update: swap now, `finish` asks UAC for the new build
                if crate::update::click(hwnd, tab) {
                    destroy(hwnd, "self-update (restart clicked) relaunches through UAC", false);
                    return;
                }
            }
            Action::Install(c) => {
                let tab = with(|a, _| a.tab.restore_id()).unwrap_or(0);
                match crate::setup::install(c, crate::setup::restore_of(hwnd, tab)) {
                    Ok(()) => {
                        // `setup::finish` starts the installed copy once this window is gone
                        destroy(hwnd, "install: the installed copy starts next", false);
                        return;
                    }
                    Err(e) => {
                        with(|a, _| a.setup.error = Some(e));
                    }
                }
            }
            Action::InstallAll(c) => {
                // UAC, then the elevated copy installs and starts the installed Blink; this window just closes
                with(|a, _| a.setup.error = Some("Waiting for the administrator step (UAC, then the copy): a few seconds...".into()));
                let hw = hwnd.0 as isize;
                crate::setup::start_install_all(c, move || {
                    let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_INSTALLALL, WPARAM(0), LPARAM(0));
                });
            }
            Action::TaskMgrRefresh => {
                with(|a, _| a.setup.error = None);
                let hw = hwnd.0 as isize;
                crate::taskmgr::start_refresh(move || {
                    let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_TASKMGR, WPARAM(0), LPARAM(0));
                });
            }
            Action::TaskMgr(on) => {
                // the helper takes seconds (copy, Defender, probe, UAC): never on the UI thread
                with(|a, _| a.setup.error = None);
                let hw = hwnd.0 as isize;
                crate::taskmgr::start_toggle(on, move || {
                    let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_TASKMGR, WPARAM(0), LPARAM(0));
                });
            }
            Action::Uninstall(data) => match {
                // a Task Manager redirect that is on goes off first (UAC); declined: nothing is removed
                modal(true);
                let gate = crate::taskmgr::uninstall_gate();
                modal(false);
                gate.and_then(|()| crate::setup::uninstall(data))
            } {
                Ok(()) => {
                    destroy(hwnd, "uninstall finished", false);
                    return;
                }
                Err(e) => {
                    with(|a, _| a.setup.error = Some(e));
                }
            },
            Action::UninstallAll => {
                // native confirm boxes; not elevated: UAC first, the boxes then come from the elevated copy
                with(|a, _| a.setup.error = None);
                modal(true);
                let flow = crate::setup::uninstall_flow(Some(hwnd));
                modal(false);
                match flow {
                    crate::setup::Flow::Done(_) => {
                        destroy(hwnd, "uninstall finished", false);
                        return;
                    }
                    crate::setup::Flow::Failed(e) => {
                        with(|a, _| a.setup.error = Some(e));
                    }
                    crate::setup::Flow::Elevated => {
                        // the elevated copy asks and removes; this window must not stay (it would recreate the data dir)
                        destroy(hwnd, "uninstall continues in the elevated copy", false);
                        return;
                    }
                    crate::setup::Flow::Cancelled => {}
                }
            }
            Action::RestartAdmin => {
                restart_as_admin(hwnd);
            }
            Action::InstallPawnIO if crate::osutil::cold_started() => {
                with(|a, _| a.setup.error = Some("Run Hayate normally to install PawnIO (this Hayate was started by Task Manager with a minimal PATH).".into()));
            }
            Action::InstallPawnIO => {
                // official signed installer from winget; it needs admin, so UAC asks
                let args: Vec<u16> = "install -e --id namazso.PawnIO --accept-package-agreements --accept-source-agreements".encode_utf16().chain(Some(0)).collect();
                modal(true);
                let r = ShellExecuteW(None, w!("runas"), w!("winget.exe"), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL);
                modal(false);
                if (r.0 as isize) <= 32 {
                    with(|a, _| a.setup.error = Some(format!("Could not start winget (ShellExecute {}). Run it yourself: winget install -e --id namazso.PawnIO", r.0 as isize)));
                }
            }
            Action::Minimize => {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            }
            Action::ToggleMax => {
                let _ = ShowWindow(hwnd, if IsZoomed(hwnd).as_bool() { SW_RESTORE } else { SW_MAXIMIZE });
            }
            Action::Hide => {
                hide(hwnd, "close button or Esc");
                return;
            }
            Action::PowerMenu(x, y) => power_menu(hwnd, x, y),
            Action::ColumnMenu(x, y) => column_menu(hwnd, x, y),
            Action::RowMenu(x, y, t) => {
                row_menu(hwnd, x, y, *t);
                with(|a, _| a.vit.pick = None);
            }
            Action::RuleMenu(x, y) => rule_menu(hwnd, x, y),
            Action::EventMenu(x, y, i) => event_menu(hwnd, x, y, i),
            Action::SvcMenu(x, y, t) => svc_menu(hwnd, x, y, *t),
            Action::StartupMenu(x, y, c) => startup_menu(hwnd, x, y, *c),
            Action::EndSelected => end_selected(false, None),
            Action::Clip(s) => clip(hwnd, &s),
            Action::Zoom(c) => apply_zoom(hwnd, c),
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// UAC prompt; the elevated copy waits for this process to exit (--wait-pid)
/// so the single-instance check does not wake us. A No on the prompt leaves
/// this window as it is.
fn restart_as_admin(hwnd: HWND) {
    unsafe {
        if let Ok(exe) = std::env::current_exe() {
            let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain(Some(0)).collect();
            // UAC starts it through the shell, which drops our environment: carry
            // BLINK_DATA_DIR / BLINK_MULTI and the current tab as the private args
            let mut restore = crate::setup::env_restore();
            restore.tab = with(|a, _| a.tab.restore_id());
            let line = crate::update::logic::join_args(&crate::update::logic::admin_relaunch_args(&restore, std::process::id()));
            let args: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
            modal(true);
            let r = ShellExecuteW(None, w!("runas"), PCWSTR(exe_w.as_ptr()), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL);
            modal(false);
            if r.0 as isize > 32 {
                destroy(hwnd, "restart as administrator (UAC accepted)", false);
            } else {
                crate::osutil::log(&format!("restart as administrator: not started (ShellExecute {}), window kept", r.0 as isize));
            }
        }
    }
}

/// A taskbar launch reached this Blink (Jump List task, Nilesoft item):
/// bring the window up on the asked tab, or start the admin restart.
fn apply_intent(hwnd: HWND, i: crate::taskbar::logic::Intent) {
    use crate::taskbar::logic::Intent;
    match i {
        Intent::Open => show(hwnd),
        Intent::Tab(id) => {
            if let Some(t) = crate::ui::Tab::from_restore_id(id) {
                with(|a, _| a.switch_tab(t));
            }
            show(hwnd);
        }
        // never starts UAC from here (see `taskbar::restart_admin`); only a launch that is already elevated gets here
        Intent::RestartAdmin => show(hwnd),
    }
}

fn screen_pt(hwnd: HWND, x: f32, y: f32) -> POINT {
    let s = ui_scale(hwnd);
    let mut p = POINT { x: (x * s) as i32, y: (y * s) as i32 };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut p);
    }
    p
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn popup(hwnd: HWND, menu: HMENU, x: f32, y: f32) -> u32 {
    let p = screen_pt(hwnd, x, y);
    unsafe {
        modal(true);
        // list orders hold under an open menu (and 2 s after it closes)
        with(|a, _| a.set_menu(true));
        // paint the row the menu is on (selected / marked) before the menu loop holds the window
        let _ = InvalidateRect(Some(hwnd), None, false);
        let r = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, p.x, p.y, Some(0), hwnd, None);
        with(|a, _| a.set_menu(false));
        modal(false);
        let _ = DestroyMenu(menu);
        r.0 as u32
    }
}

fn add(menu: HMENU, id: usize, text: &str, checked: bool, enabled: bool) {
    let w = wide(text);
    let mut flags = MF_STRING;
    if checked {
        flags |= MF_CHECKED;
    }
    if !enabled {
        flags |= MF_GRAYED;
    }
    unsafe {
        let _ = AppendMenuW(menu, flags, id, PCWSTR(w.as_ptr()));
    }
}
fn sep(menu: HMENU) {
    unsafe {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    }
}

fn power_menu(hwnd: HWND, x: f32, y: f32) {
    let Some(items) = with(|a, _| a.plans.iter().map(|p| (p.name.clone(), Some(p.guid) == a.plan_active)).collect::<Vec<_>>()) else { return };
    unsafe {
        let m = CreatePopupMenu().unwrap();
        for (i, (name, on)) in items.iter().enumerate() {
            add(m, 1 + i, name, *on, true);
        }
        sep(m);
        add(m, 99, "Ctrl+Shift+P cycles (while Hayate is focused)", false, false);
        let id = popup(hwnd, m, x, y);
        if id >= 1 && (id as usize) <= items.len() {
            with(|a, _| a.set_plan(id as usize - 1));
        }
    }
}

fn column_menu(hwnd: HWND, x: f32, y: f32) {
    let Some(cols) = with(|a, _| a.procs.cols.iter().map(|c| (c.label, c.on, c.col)).collect::<Vec<_>>()) else { return };
    unsafe {
        let m = CreatePopupMenu().unwrap();
        for (i, (label, on, col)) in cols.iter().enumerate() {
            if *col == crate::ui::procs::Col::Name {
                continue;
            }
            add(m, 1 + i, label, *on, true);
        }
        let id = popup(hwnd, m, x, y);
        if id >= 1 {
            with(|a, _| {
                a.procs.toggle_col(id as usize - 1);
                a.procs.dirty = true;
            });
        }
    }
}

fn submenu(m: HMENU, sub: HMENU, text: &str) {
    let w = wide(text);
    unsafe {
        let _ = AppendMenuW(m, MF_POPUP, sub.0 as usize, PCWSTR(w.as_ptr()));
    }
}

/// The saved rule whose pattern is exactly this exe name.
fn rule_by_name(name: &str) -> Option<crate::rules::Rule> {
    crate::features::get()?.rules.rules().into_iter().find(|r| r.pattern.eq_ignore_ascii_case(name))
}

/// Toggle/modify the per-exe rule, then apply it to running matches now.
fn edit_rule(name: &str, f: impl FnOnce(&mut crate::rules::Actions)) {
    let Some(ft) = crate::features::get() else { return };
    let mut r = rule_by_name(name).unwrap_or_else(|| crate::rules::Rule::new(name, Default::default()));
    f(&mut r.actions);
    if r.actions.is_empty() {
        if r.id != 0 {
            ft.rules.remove(r.id);
        }
        return;
    }
    let id = if r.id == 0 {
        ft.rules.add(r)
    } else {
        let id = r.id;
        ft.rules.update(r);
        id
    };
    let e = ft.rules.clone();
    std::thread::spawn(move || e.apply_running(Some(id)));
}

/// Draw a menu description (`procmenu::build`) into a native menu.
fn render(menu: HMENU, entries: &[Entry]) {
    for e in entries {
        match e {
            Entry::Item { id, text, checked, enabled } => add(menu, *id, text, *checked, *enabled),
            Entry::Sep => sep(menu),
            Entry::Sub { text, items } => unsafe {
                let sub = CreatePopupMenu().unwrap();
                render(sub, items);
                submenu(menu, sub, text);
            },
        }
    }
}

/// What the menu reads from this machine: the saved rule of the representative's exe, presets,
/// power plans, CPU topology, elevation.
fn menu_env(t: &Target) -> Env {
    let ft = crate::features::get();
    let presets = ft.map(|f| f.rules.cpu_presets()).unwrap_or_default();
    let topo = crate::rules::cpu::topology();
    Env {
        rules: ft.is_some(),
        rule: t.rep.as_ref().and_then(|r| rule_by_name(&r.name)).map(|r| r.actions),
        ccd_hint: presets.is_empty() && crate::rules::cpu::ccds().len() >= 2 && !ft.is_some_and(|f| f.rules.settings().ccd_presets_enabled),
        presets,
        plans: crate::power::plans(),
        cpu: topo.supported().map_err(|_| topo.groups),
        elevated: crate::osutil::is_elevated(),
        // cheap static layer only (name list, Windows folder, Blink itself); the panel re-checks live, with the OS critical-process query
        disallow: match t.rep.as_ref() {
            Some(r) => crate::rules::protect::static_check(crate::rules::protect::Ctx::system(), r.pid, r.path.as_deref())
                .map_err(|why| why.text().trim_start_matches("refused: ").to_string()),
            None => Ok(()),
        },
    }
}

/// The process menu of Processes and of every Vitals Top list. `t` was captured when the menu was
/// asked for (by identity: pid + create time); whatever the lists do while it is up, the chosen
/// command goes to these processes, and each is re-verified at the OS before it is touched.
fn row_menu(hwnd: HWND, x: f32, y: f32, t: Target) {
    let env = menu_env(&t);
    let entries = build(&t, &env);
    let rep = t.rep.clone();
    let (pid, create) = rep.as_ref().map(|r| (Some(r.pid), r.create)).unwrap_or((None, 0));
    let name = rep.as_ref().map(|r| r.name.clone());
    let (path, cmd) = (rep.as_ref().and_then(|r| r.path.clone()), rep.as_ref().and_then(|r| r.cmd.clone()));
    let ft = crate::features::get();
    unsafe {
        let m = CreatePopupMenu().unwrap();
        render(m, &entries);
        let id = popup(hwnd, m, x, y) as usize;
        match id {
            ID_END => end_selected(false, Some(t.members.clone())),
            ID_END_TREE => end_selected(true, Some(t.members.clone())),
            ID_LOCATE => {
                if let Some(p) = path {
                    let args = wide(&format!("/select,\"{p}\""));
                    if let Ok(wd) = crate::osutil::windows_dir() {
                        let ex = wide(&format!("{wd}\\explorer.exe"));
                        ShellExecuteW(None, w!("open"), PCWSTR(ex.as_ptr()), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL);
                    }
                }
            }
            ID_PROPS => {
                if let Some(p) = path {
                    let wp = wide(&p);
                    let _ = SHObjectProperties(Some(hwnd), SHOP_FILEPATH, PCWSTR(wp.as_ptr()), None);
                }
            }
            ID_COPY_CMD => clip(hwnd, cmd.as_deref().unwrap_or("")),
            ID_COPY_PATH => clip(hwnd, path.as_deref().unwrap_or("")),
            ID_COPY_PID => clip(hwnd, &pid.map(|p| p.to_string()).unwrap_or_default()),
            ID_R_VIEW => {
                with(|a, _| crate::ui::rulepeek::open_target(a, &t));
            }
            ID_SHOW => {
                if let Some(sh) = t.show {
                    with(|a, _| {
                        a.search.clear();
                        a.search_focus = false;
                        a.switch_tab(crate::ui::Tab::Processes);
                        a.procs.show(sh.select, sh.expand);
                    });
                }
            }
            ID_DISALLOW => {
                if let Some(p) = pid {
                    with(|a, _| crate::ui::disallow::open_process(a, p, create));
                }
            }
            ID_CPU_AFF | ID_CPU_SETS => {
                if let (Some(p), Some(nm)) = (pid, name.as_deref()) {
                    let kind = if id == ID_CPU_AFF { crate::rules::cpu::Kind::Affinity } else { crate::rules::cpu::Kind::CpuSets };
                    with(|a, _| crate::ui::cpu::open(a, kind, p, create, nm));
                }
            }
            ID_EFF_ONCE => {
                if let (Some(f), Some(p)) = (ft, pid) {
                    let a = crate::rules::Actions { efficiency: Some(true), ..Default::default() };
                    f.rules.apply_once(p, create, &a);
                }
            }
            ID_TRIM => {
                if let Some(p) = pid {
                    let _ = crate::rules::actions::trim(p, create);
                }
            }
            ID_RESTART | ID_RESTART_ADMIN => {
                if let Some(p) = pid {
                    let admin = id == ID_RESTART_ADMIN;
                    std::thread::spawn(move || {
                        match crate::rules::actions::restart(p, create, admin) {
                            Ok(np) => crate::osutil::log(&format!("restart pid={p} admin={admin} -> pid={np}")),
                            Err(e) => crate::osutil::log(&format!("restart pid={p} admin={admin} failed: {e}")),
                        }
                    });
                }
            }
            ID_R_EFF | ID_R_IO | ID_R_AWAKE | ID_R_BOOST | ID_R_REMOVE | ID_R_SHOW => {
                let nm = name.clone().unwrap_or_default();
                match id {
                    ID_R_EFF => edit_rule(&nm, |a| a.efficiency = if a.efficiency == Some(true) { None } else { Some(true) }),
                    ID_R_IO => edit_rule(&nm, |a| a.io = if a.io.is_some() { None } else { Some(crate::rules::IoPriority::Low) }),
                    ID_R_BOOST => edit_rule(&nm, |a| a.boost = if a.boost == Some(false) { None } else { Some(false) }),
                    ID_R_AWAKE => edit_rule(&nm, |a| a.keep_awake = if a.keep_awake.is_some() { None } else { Some(crate::rules::Awake::Display) }),
                    ID_R_REMOVE => edit_rule(&nm, |a| *a = Default::default()),
                    _ => {
                        let rid = rule_by_name(&nm).map(|r| r.id);
                        with(|a, _| {
                            a.switch_tab(crate::ui::Tab::Rules);
                            a.rules.sel = rid;
                            a.rules.reveal = rid;
                        });
                    }
                }
            }
            i if (ID_R_PRIO..ID_R_PRIO + 6).contains(&i) => {
                let p = crate::rules::Priority::ALL[i - ID_R_PRIO];
                edit_rule(name.as_deref().unwrap_or(""), |a| a.priority = if a.priority == Some(p) { None } else { Some(p) });
            }
            i if (ID_R_SETS..ID_R_SETS + 8).contains(&i) => {
                if let Some(p) = env.presets.get(i - ID_R_SETS) {
                    let m = p.mask;
                    edit_rule(name.as_deref().unwrap_or(""), |a| a.cpu_sets = if a.cpu_sets == Some(m) { None } else { Some(m) });
                }
            }
            i if (ID_R_AFF..ID_R_AFF + 8).contains(&i) => {
                if let Some(p) = env.presets.get(i - ID_R_AFF) {
                    let m = p.mask;
                    edit_rule(name.as_deref().unwrap_or(""), |a| a.affinity = if a.affinity == Some(m) { None } else { Some(m) });
                }
            }
            i if (ID_R_POWER..ID_R_POWER + 10).contains(&i) => {
                if let Some(p) = env.plans.get(i - ID_R_POWER) {
                    let gd = p.guid;
                    edit_rule(name.as_deref().unwrap_or(""), |a| a.power_plan = if a.power_plan == Some(gd) { None } else { Some(gd) });
                }
            }
            i if (ID_PRIO..ID_PRIO + 6).contains(&i) => {
                let classes = [REALTIME_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, ABOVE_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, IDLE_PRIORITY_CLASS];
                for (pid, create, _) in &t.members {
                    // same pid AND create time at the OS, or it is not the process that was picked
                    if let Some(h) = crate::osutil::open_same(*pid, *create, PROCESS_SET_INFORMATION) {
                        let _ = SetPriorityClass(h.0, classes[i - ID_PRIO]);
                    }
                }
            }
            _ => {}
        }
    }
}

fn end_selected(tree: bool, picked: Option<Vec<(u32, i64, String)>>) {
    // `picked`: the menu's targets (fixed when it opened); None: the selection now (Delete key)
    let Some(mut v) = with(|a, _| {
        let snap = a.snap.clone();
        let mut v: Vec<(u32, i64)> = picked.unwrap_or_else(|| a.procs.selected_pids(&snap)).into_iter().map(|p| (p.0, p.1)).collect();
        if tree {
            v = crate::ui::procs::with_descendants(&snap.procs, v);
        }
        v
    }) else {
        return;
    };
    let me = std::process::id();
    v.retain(|&(p, _)| p > 4 && p != me);
    for (pid, create) in v {
        // pid + create time must still match at the OS: a reused PID is never terminated
        if let Some(h) = crate::osutil::open_same(pid, create, PROCESS_TERMINATE) {
            unsafe {
                let _ = TerminateProcess(h.0, 1);
            }
        }
    }
}

fn clip(hwnd: HWND, s: &str) {
    let w = wide(s);
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(13, Some(HANDLE(h.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

// ---------- Rules / Watch menus ----------

fn rule_menu(hwnd: HWND, x: f32, y: f32) {
    let Some(ft) = crate::features::get() else { return };
    let Some(sel) = with(|a, _| a.rules.sel).flatten() else { return };
    let Some(r) = ft.rules.get(sel) else { return };
    unsafe {
        let m = CreatePopupMenu().unwrap();
        add(m, 1, if r.enabled { "Disable" } else { "Enable" }, false, true);
        add(m, 2, "Apply to running now", false, r.enabled);
        sep(m);
        add(m, 3, "Copy rule", false, true);
        add(m, 4, "Paste rules", false, true);
        sep(m);
        add(m, 5, "Remove", false, true);
        if r.actions.disallow {
            sep(m);
            let others = r.actions.disallow && !crate::rules::Actions { disallow: false, ..r.actions.clone() }.is_empty();
            add(m, 6, if others { "Allow running again (remove Disallow; its other settings apply again)" } else { "Allow running again (remove Disallow)" }, false, true);
        }
        match popup(hwnd, m, x, y) {
            1 => {
                if r.actions.disallow && !r.enabled {
                    // turning on a Disallow rule ends programs: ask first
                    with(|a, _| crate::ui::disallow::open_rule(a, sel));
                } else {
                    let mut r = r;
                    r.enabled = !r.enabled;
                    ft.rules.update(r);
                }
            }
            2 => {
                let e = ft.rules.clone();
                std::thread::spawn(move || e.apply_running(Some(sel)));
            }
            3 => {
                if let Some(t) = ft.rules.copy(sel) {
                    clip(hwnd, &t);
                }
            }
            4 => {
                if let Some(t) = clip_get(hwnd) {
                    let _ = ft.rules.paste(&t, None);
                }
            }
            5 => {
                ft.rules.remove(sel);
                with(|a, _| a.rules.sel = None);
            }
            6 => {
                ft.rules.clear_disallow(sel, None);
            }
            _ => {}
        }
    }
}

fn event_menu(hwnd: HWND, x: f32, y: f32, i: usize) {
    let Some(e) = with(|a, _| crate::ui::watch::event_at(a, i)).flatten() else { return };
    let Some(ft) = crate::features::get() else { return };
    unsafe {
        let m = CreatePopupMenu().unwrap();
        add(m, 1, if e.unread { "Mark read" } else { "Mark all read" }, false, true);
        add(m, 2, "Open file location", false, !e.path.is_empty());
        add(m, 3, "Copy path", false, !e.path.is_empty());
        add(m, 4, "Copy details", false, true);
        if e.kind().is_service() && !e.key.is_empty() {
            sep(m);
            add(m, 5, "Open Services (services.msc)", false, true);
        }
        let leak = e.kind() == crate::watch::Kind::MemoryLeak && !e.name.is_empty();
        let builtin = leak && crate::leak::is_builtin(&e.name);
        let user = leak && !builtin && crate::leak::is_ignored(&e.name);
        if leak {
            sep(m);
            if builtin {
                add(m, 6, "Leaks ignored for this app (built-in list)", false, false);
            } else if user {
                add(m, 7, &format!("Stop ignoring leaks for {}", e.name), false, true);
            } else {
                add(m, 6, &format!("Ignore leaks for {}", e.name), false, true);
            }
        }
        match popup(hwnd, m, x, y) {
            1 => {
                if e.unread {
                    ft.watch.mark_read(e.id)
                } else {
                    ft.watch.mark_all_read()
                }
            }
            2 => {
                let _ = crate::rules::actions::locate(&e.path);
            }
            3 => clip(hwnd, &e.path),
            4 => clip(
                hwnd,
                &format!("{} · {}\n{}\n{}\nby {}{}\n{} ({})", e.kind().label(), e.name, e.path, e.detail, e.actor, if e.actor_likely { " (likely)" } else { "" }, e.publisher, e.sig.key()),
            ),
            5 => {
                let _ = crate::rules::actions::launch_unelevated("mmc.exe services.msc", None);
            }
            6 => crate::leak::set_user_ignored(&e.name, true),
            7 => crate::leak::set_user_ignored(&e.name, false),
            _ => {}
        }
    }
}

/// The Services right-click menu. `t` was captured (by service name) when the menu was asked for;
/// every choice re-reads the live service before it acts (svcctl), so a list that moved, or a
/// service that was restarted, deleted or changed meanwhile, is refused rather than guessed.
/// Startup tab row menu: items and pick logic live in `ui::startup` (pure + shared handlers); this
/// only draws, tracks and runs the shell "open file location". Native menus follow the system DPI,
/// not the Ctrl +/- zoom (src/zoom.rs).
fn startup_menu(hwnd: HWND, x: f32, y: f32, ctx: crate::ui::startup::MenuCtx) {
    let id = unsafe {
        let m = CreatePopupMenu().unwrap();
        render(m, ctx.entries());
        popup(hwnd, m, x, y) as usize
    };
    if id == crate::ui::startup::M_LOCATE {
        if let Some(p) = ctx.locate() {
            let args = wide(&format!("/select,\"{p}\""));
            if let Ok(wd) = crate::osutil::windows_dir() {
                let ex = wide(&format!("{wd}\\explorer.exe"));
                unsafe {
                    ShellExecuteW(None, w!("open"), PCWSTR(ex.as_ptr()), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL);
                }
            }
        }
        return;
    }
    if let Some(Action::Clip(s)) = with(|a, _| crate::ui::startup::menu_pick(a, &ctx, id)) {
        clip(hwnd, &s);
    }
}

fn svc_menu(hwnd: HWND, x: f32, y: f32, t: crate::svcctl::Target) {
    use crate::svcctl::{Op, StartKind};
    use crate::ui::services as sv;
    let entries = sv::menu(&t, crate::osutil::is_elevated());
    let id = unsafe {
        let m = CreatePopupMenu().unwrap();
        render(m, &entries);
        popup(hwnd, m, x, y) as usize
    };
    match id {
        sv::ID_START => sv::run(t, Op::Start),
        sv::ID_STOP => sv::run(t, Op::Stop),
        sv::ID_RESTART => sv::run(t, Op::Restart),
        sv::ID_PROPS => {
            with(|a, _| crate::ui::svcprops::open(a, t));
        }
        sv::ID_CANCEL => crate::svcctl::cancel_wait(&t.key()),
        sv::ID_COPY => clip(hwnd, &t.name),
        sv::ID_ADMIN => restart_as_admin(hwnd),
        n if (sv::ID_KIND..sv::ID_KIND + 4).contains(&n) => {
            // same guarded path as the toolbar control and Alt+1..4; the type it already has says so
            sv::quick_set(t, StartKind::ALL[n - sv::ID_KIND]);
        }
        _ => {}
    }
}

/// Text on the clipboard (Ctrl+V into a Startup field).
pub fn paste_text() -> Option<String> {
    clip_get(HWND::default())
}

fn clipboard_text(units: &[u16]) -> Option<String> {
    let units = &units[..units.len().min(32 * 1024)];
    let end = units.iter().position(|&u| u == 0)?;
    Some(String::from_utf16_lossy(&units[..end]))
}

fn clip_get(hwnd: HWND) -> Option<String> {
    unsafe {
        OpenClipboard(Some(hwnd)).ok()?;
        let mut out = None;
        if let Ok(h) = GetClipboardData(13) {
            // Clipboard owners are untrusted: a CF_UNICODETEXT block need not be
            // terminated. Never walk past its allocation or accept more than 64 KiB.
            let units = (GlobalSize(HGLOBAL(h.0)) / std::mem::size_of::<u16>()).min(32 * 1024);
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if !p.is_null() {
                out = clipboard_text(std::slice::from_raw_parts(p, units));
                let _ = GlobalUnlock(HGLOBAL(h.0));
            }
        }
        let _ = CloseClipboard();
        out
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::clipboard_text;

    #[test]
    fn clipboard_text_requires_a_terminator_inside_the_bounded_block() {
        assert_eq!(clipboard_text(&[]), None);
        assert_eq!(clipboard_text(&[65, 66]), None);
        assert_eq!(clipboard_text(&[65, 0, 66]), Some("A".into()));
        let mut large = vec![65; 32 * 1024 + 1];
        large[32 * 1024] = 0;
        assert_eq!(clipboard_text(&large), None);
        large[32 * 1024 - 1] = 0;
        assert_eq!(clipboard_text(&large).unwrap().len(), 32 * 1024 - 1);
    }
}

// ---------- tray ----------

fn copy_w(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
}

/// Version 4 tray callbacks: one left click sends NIN_SELECT *and* WM_LBUTTONUP,
/// so only NIN_SELECT may toggle (else the second event undoes the first).
static TRAY_V4: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn tray_add(hwnd: HWND) {
    unsafe {
        let mut n = nid(hwnd);
        n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        n.uCallbackMessage = WM_APP_TRAY;
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        n.hIcon = LoadIconW(Some(hinst), PCWSTR(1 as *const u16)).or_else(|_| LoadIconW(None, IDI_APPLICATION)).unwrap_or_default();
        copy_w(&mut n.szTip, crate::osutil::APP_NAME);
        let _ = Shell_NotifyIconW(NIM_DELETE, &n);
        let added = Shell_NotifyIconW(NIM_ADD, &n).as_bool();
        n.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let ver = Shell_NotifyIconW(NIM_SETVERSION, &n).as_bool();
        TRAY_V4.store(ver, std::sync::atomic::Ordering::Relaxed);
        crate::osutil::log(&format!("tray add={added} version4={ver}"));
    }
}

/// Work for the tray thread: the newest snapshot to show, and whether toasts
/// are queued in TOASTS.
struct TrayWork {
    snap: Option<Arc<sample::Snapshot>>,
    toasts: bool,
}
static TRAY_Q: (std::sync::Mutex<TrayWork>, std::sync::Condvar) = (std::sync::Mutex::new(TrayWork { snap: None, toasts: false }), std::sync::Condvar::new());

/// Hand tray icon / tooltip / balloon updates to the "tray" thread.
/// Shell_NotifyIcon is a synchronous call into Explorer: on a busy machine it
/// took seconds, and on the UI thread that froze the window (then the queued
/// work ran at once). Only the newest snapshot is kept.
fn tray_feed(hwnd: HWND, snap: Option<Arc<sample::Snapshot>>, toasts: bool) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let hw = hwnd.0 as isize;
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("tray".into()).stack_size(256 * 1024).spawn(move || tray_thread(HWND(hw as *mut _)));
    });
    let (m, c) = &TRAY_Q;
    if let Ok(mut w) = m.lock() {
        if snap.is_some() {
            w.snap = snap;
        }
        w.toasts |= toasts;
        c.notify_one();
    }
}

fn tray_thread(hwnd: HWND) {
    let (m, c) = &TRAY_Q;
    loop {
        let (snap, toasts) = {
            let Ok(g) = m.lock() else { return };
            let Ok(mut g) = c.wait_while(g, |w| w.snap.is_none() && !w.toasts) else { return };
            let t = g.toasts;
            g.toasts = false;
            (g.snap.take(), t)
        };
        if let Some(s) = snap {
            tray_update(hwnd, &s);
        }
        if toasts {
            // Windows shows one at a time and queues the rest itself
            while let Some(t) = TOASTS.lock().ok().and_then(|mut q| q.pop_front()) {
                balloon(hwnd, &t);
            }
        }
    }
}

/// Swap the live CPU graph and the CPU/RAM/GPU tooltip into the tray icon.
/// Runs on the tray thread.
fn tray_update(hwnd: HWND, s: &sample::Snapshot) {
    static LAST_ICON: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
    if s.cpu_hist.is_empty() {
        return;
    }
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96);
        let size = GetSystemMetricsForDpi(SM_CXSMICON, dpi).clamp(16, 64) as usize;
        let Some(icon) = crate::trayicon::make_icon(&s.cpu_hist, size) else { return };
        let mut n = nid(hwnd);
        n.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        n.hIcon = icon;
        let tip = crate::trayicon::tooltip(s);
        copy_w(&mut n.szTip, &match crate::update::tip_note() {
            Some(note) => format!("{note}
{tip}"),
            None => tip,
        });
        let t0 = std::time::Instant::now();
        let ok = Shell_NotifyIconW(NIM_MODIFY, &n).as_bool();
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        if ms > 100.0 {
            crate::timing::stall(crate::timing::Stall::Tray, ms, || format!("tray: Shell_NotifyIcon (Explorer) took {ms:.0} ms (tray thread; the window is not held up), ok={ok}"));
        }
        // the shell copied the icon: free the previous one, keep this for next time
        let old = LAST_ICON.swap(if ok { icon.0 as isize } else { 0 }, std::sync::atomic::Ordering::Relaxed);
        if old != 0 {
            let _ = DestroyIcon(HICON(old as *mut _));
        }
        if !ok {
            let _ = DestroyIcon(icon);
        }
    }
}

fn tray_delete(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid(hwnd));
    }
}

/// Open Blink on Watch with `id` selected (0: just Watch): a toast or
/// balloon click.
fn open_watch(hwnd: HWND, id: u64) {
    with(|a, _| {
        a.tab = crate::ui::Tab::Watch;
        a.watch.chip = 0;
        a.watch.scroll = 0.0;
        if id != 0 {
            a.watch.sel = Some(id);
            if let Some(f) = crate::features::get() {
                f.watch.mark_read(id);
            }
        }
    });
    show(hwnd);
}

/// A Watch alert: a Windows toast when Blink has its Start-menu shortcut,
/// else a tray balloon. Runs on the tray thread.
fn balloon(hwnd: HWND, t: &crate::watch::Toast) {
    if crate::notify::toast(crate::notify::Note { title: t.title.clone(), text: t.text.clone(), kind: t.alert.key(), event_id: t.event_id }) {
        return;
    }
    LAST_TOAST_EVENT.store(t.event_id.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    notify_balloon(hwnd, &t.title, &t.text, true);
}

/// Tray balloon (Windows shows it as a toast) with Blink's own large icon,
/// not the live CPU graph of the tray icon nor the generic info "i".
fn notify_balloon(hwnd: HWND, title: &str, text: &str, respect_quiet: bool) {
    static ICON: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
    unsafe {
        let mut icon = ICON.load(std::sync::atomic::Ordering::Relaxed);
        if icon == 0 {
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
            let sz = GetSystemMetricsForDpi(SM_CXICON, GetDpiForWindow(hwnd).max(96));
            // kept for the process lifetime: one icon, reused by every toast
            icon = LoadImageW(Some(hinst), PCWSTR(1 as *const u16), IMAGE_ICON, sz, sz, LR_DEFAULTCOLOR).map(|h| h.0 as isize).unwrap_or(0);
            ICON.store(icon, std::sync::atomic::Ordering::Relaxed);
        }
        let mut n = nid(hwnd);
        n.uFlags = NIF_INFO;
        copy_w(&mut n.szInfoTitle, title);
        copy_w(&mut n.szInfo, text);
        n.dwInfoFlags = if icon != 0 {
            n.hBalloonIcon = HICON(icon as *mut _);
            NIIF_USER | NIIF_LARGE_ICON
        } else {
            NIIF_INFO
        };
        if respect_quiet {
            n.dwInfoFlags |= NIIF_RESPECT_QUIET_TIME;
        }
        let ok = Shell_NotifyIconW(NIM_MODIFY, &n).as_bool();
        crate::osutil::log(&format!("toast ok={ok} title={title:?} text={text:?}"));
    }
}

fn tray_menu(hwnd: HWND) {
    let plans = crate::power::plans();
    let active = crate::power::active();
    let ft = crate::features::get();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let snoozed = ft.map(|f| f.watch.dnd_until() > now).unwrap_or(false);
    let unread = ft.map(|f| f.watch.unread()).unwrap_or(0);
    unsafe {
        let m = CreatePopupMenu().unwrap();
        add(m, 1, "Open Hayate", false, true);
        add(m, 2, &if unread > 0 { format!("Watch ({unread} new)") } else { "Watch".to_string() }, false, true);
        sep(m);
        for (i, p) in plans.iter().enumerate().take(10) {
            add(m, 10 + i, &p.name, Some(p.guid) == active, true);
        }
        sep(m);
        add(m, 3, if snoozed { "Resume alerts" } else { "Snooze alerts 3 h" }, snoozed, true);
        sep(m);
        add(m, 4, "Exit Hayate", false, true);
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let _ = SetForegroundWindow(hwnd);
        modal(true);
        let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY | TPM_BOTTOMALIGN, p.x, p.y, Some(0), hwnd, None).0 as usize;
        modal(false);
        let _ = DestroyMenu(m);
        match id {
            1 => show(hwnd),
            2 => {
                with(|a, _| a.tab = crate::ui::Tab::Watch);
                show(hwnd);
            }
            3 => {
                if let Some(f) = ft {
                    f.watch.set_dnd(if snoozed { 0 } else { 3 * 3600 });
                }
            }
            4 => destroy(hwnd, "tray menu Quit", true),
            i if (10..20).contains(&i) => {
                if let Some(pl) = plans.get(i - 10) {
                    crate::power::set(&pl.guid);
                    with(|a, _| a.plan_active = Some(pl.guid));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod size_gate_tests {
    use super::*;

    #[test]
    fn minimized_gates_probes_restored_opens_them() {
        assert_eq!(minimized_after_size(SIZE_MINIMIZED), Some(true));
        assert_eq!(minimized_after_size(SIZE_RESTORED), Some(false));
        assert_eq!(minimized_after_size(SIZE_MAXIMIZED), Some(false));
        assert_eq!(minimized_after_size(3), None);
        assert!(is_min_cmd(SW_SHOWMINNOACTIVE) && is_min_cmd(SW_MINIMIZE));
        assert!(!is_min_cmd(SW_SHOWNOACTIVATE) && !is_min_cmd(SW_SHOW) && !is_min_cmd(SW_RESTORE));
    }
}
