//! Startup timing: ms since process creation for each launch milestone.
//! One `startup:` summary line per launch always goes to blink.log (when the
//! first paint with real Vitals data happens); `BLINK_TIMING=1` also logs
//! each milestone as its own line as it happens.
//! Cost when quiet: one Mutex push per milestone, ~25 per launch.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

static MARKS: Mutex<Vec<(String, f32)>> = Mutex::new(Vec::new());
static DONE: AtomicBool = AtomicBool::new(false);

pub fn detail() -> bool {
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var_os("BLINK_TIMING").is_some_and(|v| v != "0"))
}

/// Milliseconds since this process was created (includes loader time).
pub fn ms() -> f32 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    static CREATE: OnceLock<u64> = OnceLock::new();
    let ft = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
    let c = *CREATE.get_or_init(|| unsafe {
        let (mut c, mut e, mut k, mut u) = Default::default();
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        ft(c)
    });
    let now = ft(unsafe { GetSystemTimePreciseAsFileTime() });
    now.saturating_sub(c) as f32 / 10_000.0
}

/// True until the summary line was written (callers can skip building names).
pub fn active() -> bool {
    !DONE.load(Ordering::Relaxed) || detail()
}

/// Record a milestone. After the summary only detail mode logs anything.
pub fn mark(what: &str) {
    if !active() {
        return;
    }
    let t = ms();
    if detail() {
        crate::osutil::log(&format!("startup: {t:8.1} ms  {what}"));
    }
    if !DONE.load(Ordering::Relaxed) {
        if let Ok(mut m) = MARKS.lock() {
            m.push((what.to_string(), t));
        }
    }
}

/// Run `f`, recording how long it took as `what (x ms)`.
pub fn time<R>(what: &str, f: impl FnOnce() -> R) -> R {
    if !active() {
        return f();
    }
    let t0 = std::time::Instant::now();
    let r = f();
    mark(&format!("{what} ({:.1} ms)", t0.elapsed().as_secs_f32() * 1000.0));
    r
}

/// The first paint that shows real Vitals numbers: write the summary line once.
pub fn finish(what: &str) {
    if DONE.load(Ordering::Relaxed) {
        return;
    }
    mark(what);
    DONE.store(true, Ordering::Relaxed);
    let m = MARKS.lock().map(|mut m| std::mem::take(&mut *m)).unwrap_or_default();
    let total = m.last().map(|x| x.1).unwrap_or(0.0);
    let elev = if crate::osutil::is_elevated() { " elevated" } else { "" };
    let parts: Vec<String> = m.iter().map(|(w, t)| format!("{t:.0} {w}")).collect();
    crate::osutil::log(&format!("startup: data shown at {total:.0} ms{elev} | {}", parts.join(" | ")));
}

/// CPU cycles the calling thread has used so far (exact, unlike the 15.6 ms
/// tick-sampled thread times). For `BLINK_TIMING` cost logs.
pub fn thread_cycles() -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryThreadCycleTime(h: isize, c: *mut u64) -> i32;
    }
    let mut c = 0u64;
    // -2 = GetCurrentThread() pseudo handle
    unsafe { QueryThreadCycleTime(-2, &mut c) };
    c
}

// ---------- paint stats (BLINK_TIMING only) ----------

/// The current 30 s window: (total ms, EndDraw ms) per paint, its start (µs),
/// process CPU ms at its start.
type Frames = (Vec<(f32, f32)>, u64, f64);
static FRAMES: Mutex<Frames> = Mutex::new((Vec::new(), 0, 0.0));

/// When the window was last shown (detail mode), for the open-latency line.
static SHOWN_AT: AtomicU64 = AtomicU64::new(0);
pub fn shown() {
    if detail() {
        SHOWN_AT.store(now_us(), Ordering::Relaxed);
    }
}

/// Detail mode: collect one paint's total and EndDraw time; every 30 s log
/// count, avg / p95 / max and Blink's own CPU % over the window. The first
/// paint after a show also logs how long the open took.
pub fn paint_sample(ms: f32, end_ms: f32, create_ms: f32, what: impl FnOnce() -> String) {
    if !detail() {
        return;
    }
    let now = now_us();
    let shown = SHOWN_AT.swap(0, Ordering::Relaxed);
    if shown != 0 {
        crate::osutil::log(&format!(
            "show: first paint done {:.1} ms after show (paint {ms:.1} ms = target creation {create_ms:.1} + EndDraw {end_ms:.1} + draw)",
            (now - shown) as f64 / 1000.0
        ));
    }
    let Ok(mut f) = FRAMES.lock() else { return };
    if f.1 == 0 {
        *f = (Vec::new(), now, process_cpu_ms());
    }
    f.0.push((ms, end_ms));
    if now - f.1 < 30_000_000 {
        return;
    }
    let (mut v, t0, c0) = std::mem::take(&mut *f);
    drop(f);
    let wall = (now - t0) as f64 / 1000.0;
    let cpu = (process_cpu_ms() - c0) / wall * 100.0;
    let n = v.len();
    let end_avg = v.iter().map(|x| x.1).sum::<f32>() / n as f32;
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let avg = v.iter().map(|x| x.0).sum::<f32>() / n as f32;
    let p95 = v[(n * 95 / 100).min(n - 1)].0;
    crate::osutil::log(&format!(
        "paintstats: {n} frames in {:.0} s, paint avg {avg:.2} p95 {p95:.2} max {:.2} ms, EndDraw avg {end_avg:.2} ms; process CPU {cpu:.2}% of one core; {}",
        wall / 1000.0,
        v[n - 1].0,
        what()
    ));
}

/// `private X MB, working set Y MB` of this process.
pub fn mem_line() -> String {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
    unsafe {
        let _ = GetProcessMemoryInfo(windows::Win32::System::Threading::GetCurrentProcess(), &mut pmc as *mut _ as *mut _, std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32);
    }
    format!("private {:.1} MB, working set {:.1} MB", pmc.PrivateUsage as f64 / 1048576.0, pmc.WorkingSetSize as f64 / 1048576.0)
}

/// Kernel+user ms the whole process has used.
pub fn process_cpu_ms() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    let (mut a, mut b, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        let _ = windows::Win32::System::Threading::GetProcessTimes(windows::Win32::System::Threading::GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u);
    }
    let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) as f64 / 10_000.0;
    f(k) + f(u)
}

// ---------- stall detection (always on) ----------
//
// `stall:` lines in blink.log, so a freeze on a user's machine can be
// diagnosed from the log alone. Every check is a clock read and a compare on
// the hot path; formatting and the (rate-limited) log write happen only when a
// threshold was crossed. Kinds:
//   sampler  tick period > 1.5 s: tick body by phase (wall, and the sampler
//            thread's own CPU) plus how late the timed wait woke up
//   ui-hung  seen by the sampler: the UI thread has been inside one message
//            for > 1 s (its CPU over that time tells spinning from blocked)
//   ui-msg   one dispatched message took > 250 ms (logged when it returns)
//   ui-queue a data message waited > 500 ms in the queue (UI was busy)
//   paint    WM_PAINT > 100 ms, split into draw / EndDraw (present) / target
//   tray     Shell_NotifyIcon (a cross-process call into Explorer) > 100 ms
//   device   render target lost (D2DERR_RECREATE_TARGET)

use std::sync::atomic::{AtomicU32, AtomicU64};

/// Monotonic microseconds since the first call (QPC; ~25 ns).
pub fn now_us() -> u64 {
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64 + 1
}

#[derive(Clone, Copy)]
pub enum Stall {
    Sampler,
    UiHung,
    UiMsg,
    UiQueue,
    Paint,
    Tray,
    Device,
}
const KINDS: usize = 7;

/// Per kind: last logged (µs), lines held back since then, worst held-back
/// ms, current gap (µs).
static RL: Mutex<[(u64, u32, f32, u64); KINDS]> = Mutex::new([(0, 0, 0.0, 0); KINDS]);
/// At most one line per kind per gap; the rest are counted into the next
/// line. The gap starts at 10 s and doubles while stalls keep coming (up to
/// 5 min), so a machine that stalls for hours cannot flush the log (it is
/// cut at 256 KB); a quiet spell of twice the gap resets it.
const RL_MIN_US: u64 = 10_000_000;
const RL_MAX_US: u64 = 300_000_000;

/// Log one stall line (`ms` = the stall's size, for the held-back summary).
/// `what` is only called when the line is actually written.
pub fn stall(kind: Stall, ms: f32, what: impl FnOnce() -> String) {
    let now = now_us();
    let held = {
        let Ok(mut rl) = RL.lock() else { return };
        let Some(h) = admit(&mut rl[kind as usize], now, ms) else { return };
        h
    };
    let more = if held.0 > 0 { format!(" (+{} more of this kind in the last {:.0} s, worst {:.0} ms)", held.0, held.2 as f64 / 1e6, held.1) } else { String::new() };
    crate::osutil::log(&format!("stall: {} at {:.3} s{more} | {}", ctx(), now as f64 / 1e6, what()));
}

/// Rate limiter step: `Some((held back, worst ms, µs since last line))` when
/// this stall gets a line, `None` when it is only counted.
fn admit(e: &mut (u64, u32, f32, u64), now: u64, ms: f32) -> Option<(u32, f32, u64)> {
    let gap = e.3.max(RL_MIN_US);
    let since = now - e.0;
    if e.0 != 0 && since < gap {
        e.1 += 1;
        e.2 = e.2.max(ms);
        return None;
    }
    let held = (e.1, e.2, since);
    let next = if e.0 != 0 && since < 2 * gap { (gap * 2).min(RL_MAX_US) } else { RL_MIN_US };
    *e = (now, 0, 0.0, next);
    Some(held)
}

/// Machine context for stall lines: set by the UI and the sampler.
pub static CTX_CPU10: AtomicU32 = AtomicU32::new(0);
pub static CTX_PROCS: AtomicU32 = AtomicU32::new(0);
fn ctx() -> String {
    format!(
        "{}{} cpu {:.0}% procs {}",
        if crate::osutil::is_elevated() { "elevated" } else { "user" },
        if crate::sample::VISIBLE.load(Ordering::Relaxed) { " shown" } else { " hidden" },
        CTX_CPU10.load(Ordering::Relaxed) as f32 / 10.0,
        CTX_PROCS.load(Ordering::Relaxed)
    )
}

/// What the UI thread is doing: set around every dispatched message by the
/// message loop, read by the sampler's watchdog.
pub static UI_BUSY_SINCE: AtomicU64 = AtomicU64::new(0);
pub static UI_MSG: AtomicU32 = AtomicU32::new(0);
/// Set while a modal loop (menu, window move/size) runs inside a dispatch:
/// that time is the user's, not a stall.
pub static UI_MODAL: AtomicBool = AtomicBool::new(false);

/// Kernel+user ms a thread has used (tick-granular, ~16 ms; fine for stalls).
pub fn thread_cpu_ms(h: windows::Win32::Foundation::HANDLE) -> f64 {
    use windows::Win32::Foundation::FILETIME;
    let (mut a, mut b, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        let _ = windows::Win32::System::Threading::GetThreadTimes(h, &mut a, &mut b, &mut k, &mut u);
    }
    let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) as f64 / 10_000.0;
    f(k) + f(u)
}

/// Real handle to the calling thread (for another thread to read its times).
pub fn own_thread_handle() -> isize {
    use windows::Win32::System::Threading::*;
    unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, GetCurrentThreadId()).map(|h| h.0 as isize).unwrap_or(0) }
}

pub static UI_THREAD: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Sampler side, once per tick: report a UI thread stuck in one message.
/// One line per stuck episode (the UI side logs the total when it returns).
/// The first tick that sees a dispatch only takes the UI thread's CPU time as
/// a baseline, so the report says how much of the stuck time it computed.
pub fn ui_watchdog() {
    const DONE: u64 = 1 << 63;
    static EPISODE: AtomicU64 = AtomicU64::new(0);
    static BASE: Mutex<(f64, u64)> = Mutex::new((0.0, 0));
    let since = UI_BUSY_SINCE.load(Ordering::Relaxed);
    if since == 0 || UI_MODAL.load(Ordering::Relaxed) {
        return;
    }
    let h = windows::Win32::Foundation::HANDLE(UI_THREAD.load(Ordering::Relaxed) as *mut _);
    let now = now_us();
    let ep = EPISODE.load(Ordering::Relaxed);
    if ep & !DONE != since {
        EPISODE.store(since, Ordering::Relaxed);
        if let Ok(mut b) = BASE.lock() {
            *b = (if h.0.is_null() { 0.0 } else { thread_cpu_ms(h) }, now);
        }
        return;
    }
    let busy_ms = now.saturating_sub(since) as f32 / 1000.0;
    if ep & DONE != 0 || busy_ms < 1000.0 {
        return;
    }
    EPISODE.store(since | DONE, Ordering::Relaxed);
    let (c0, t0) = BASE.lock().map(|b| *b).unwrap_or((0.0, now));
    let win_ms = now.saturating_sub(t0) as f32 / 1000.0;
    let cpu = if h.0.is_null() { -1.0 } else { (thread_cpu_ms(h) - c0) as f32 };
    let msg = UI_MSG.load(Ordering::Relaxed);
    stall(Stall::UiHung, busy_ms, || {
        format!(
            "ui-hung: UI thread inside {} for {busy_ms:.0} ms so far; its CPU over the last {win_ms:.0} ms: {cpu:.0} ms ({})",
            msg_name(msg),
            if cpu < 0.0 { "unknown" } else if cpu < win_ms * 0.3 { "waiting: blocked on a call, or starved of CPU" } else { "computing" }
        )
    });
}

pub fn msg_name(m: u32) -> String {
    let n = match m {
        0x000F => "WM_PAINT",
        0x0005 => "WM_SIZE",
        0x0113 => "WM_TIMER",
        0x0200 => "WM_MOUSEMOVE",
        0x0201 => "WM_LBUTTONDOWN",
        0x0202 => "WM_LBUTTONUP",
        0x0205 => "WM_RBUTTONUP",
        0x020A => "WM_MOUSEWHEEL",
        0x0100 => "WM_KEYDOWN",
        0x0102 => "WM_CHAR",
        0x8001 => "WM_APP_DATA",
        0x8002 => "WM_APP_TOAST",
        0x8003 => "WM_APP_WATCH",
        0x8004 => "WM_APP_TRAY",
        0x8006 => "WM_APP_TRAYTICK",
        0x8009 => "WM_APP_ICONS",
        _ => "",
    };
    if n.is_empty() { format!("msg 0x{m:04X}") } else { n.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_lines_back_off_while_stalls_continue_and_reset_when_quiet() {
        let s = 1_000_000u64;
        let mut e = (0, 0, 0.0, 0);
        assert!(admit(&mut e, 5 * s, 100.0).is_some());
        assert!(admit(&mut e, 8 * s, 900.0).is_none()); // inside 10 s
        let h = admit(&mut e, 16 * s, 100.0).unwrap();
        assert_eq!((h.0, h.1), (1, 900.0));
        assert_eq!(e.3, 20 * s); // came again soon: gap doubled
        assert!(admit(&mut e, 30 * s, 1.0).is_none());
        // a stall every 15 s for an hour: few lines, gap capped at 5 min
        let mut lines = 0;
        for t in 1..240u64 {
            lines += admit(&mut e, 16 * s + t * 15 * s, 1.0).is_some() as u32;
        }
        assert!(lines <= 20, "{lines}");
        assert_eq!(e.3, RL_MAX_US);
        // quiet for long: back to 10 s
        let last = e.0;
        assert!(admit(&mut e, last + 2 * RL_MAX_US, 1.0).is_some());
        assert_eq!(e.3, RL_MIN_US);
    }
}
