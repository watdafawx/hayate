//! Process start/stop feed for Rules and Watch.
//!
//! Primary: ETW real-time session "Blink-Kernel-Process" on
//! Microsoft-Windows-Kernel-Process (keyword 0x10). One thread blocked in
//! ProcessTrace, 2-4 x 64 KB buffers, ~0 CPU. Needs admin.
//! Fallback (session cannot start / dies): 2 s process-list diff on a
//! background thread; misses processes that live < 2 s. `status()` says which.
//!
//! Keeps a 60 s ring of starts for Watch's "who installed this" heuristic.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Diagnostics::Etw::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;

use crate::osutil::{self, wide};
use crate::sample::nt;

pub const SESSION: &str = "Blink-Kernel-Process";
/// Session name; a BLINK_MULTI test instance must not stop the user's session.
fn session() -> &'static str {
    if crate::osutil::multi() { "Blink-Kernel-Process-Multi" } else { SESSION }
}
const PROVIDER: GUID = GUID::from_u128(0x22fb2cd6_0e7b_422b_a0c7_2fad1fd0e716);
const KEYWORD_PROCESS: u64 = 0x10;
const RING_MS: u64 = 60_000;
const RING_MAX: usize = 512;

#[derive(Clone, Debug, PartialEq)]
pub struct ProcStart {
    pub pid: u32,
    pub ppid: u32,
    pub session: u32,
    /// Win32 path (`C:\...`), or bare exe name if the path was unknown.
    pub path: String,
    /// unix ms
    pub time: u64,
}

impl ProcStart {
    pub fn name(&self) -> &str {
        osutil::file_name(&self.path)
    }
}

#[derive(Clone, Debug)]
pub enum ProcEvent {
    Start(ProcStart),
    Stop { pid: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Off,
    Etw,
    Snapshot,
}

#[derive(Clone, Debug)]
pub struct Status {
    pub source: Source,
    /// Why ETW is not in use (None when it is).
    pub error: Option<String>,
    pub events: u64,
}

type Sub = Arc<dyn Fn(&ProcEvent) + Send + Sync>;

struct St {
    subs: RwLock<Vec<Sub>>,
    ring: Mutex<VecDeque<ProcStart>>,
    status: Mutex<Status>,
    stopping: AtomicBool,
    events: AtomicU64,
    ctl: AtomicU64,
}

fn st() -> &'static St {
    static S: OnceLock<St> = OnceLock::new();
    S.get_or_init(|| St {
        subs: RwLock::new(Vec::new()),
        ring: Mutex::new(VecDeque::new()),
        status: Mutex::new(Status { source: Source::Off, error: None, events: 0 }),
        stopping: AtomicBool::new(false),
        events: AtomicU64::new(0),
        ctl: AtomicU64::new(0),
    })
}

/// Register a callback. Runs on the ETW (or fallback) thread: keep it short.
pub fn subscribe(f: impl Fn(&ProcEvent) + Send + Sync + 'static) {
    st().subs.write().unwrap().push(Arc::new(f));
}

pub fn status() -> Status {
    let mut s = st().status.lock().unwrap().clone();
    s.events = st().events.load(Ordering::Relaxed);
    s
}

/// Starts seen in the last 60 s, oldest first.
pub fn recent_starts() -> Vec<ProcStart> {
    let now = osutil::now_ms();
    st().ring.lock().unwrap().iter().filter(|p| now.saturating_sub(p.time) <= RING_MS).cloned().collect()
}

fn dispatch(ev: ProcEvent) {
    st().events.fetch_add(1, Ordering::Relaxed);
    if let ProcEvent::Start(p) = &ev {
        let mut r = st().ring.lock().unwrap();
        let now = p.time;
        while r.front().map_or(false, |f| now.saturating_sub(f.time) > RING_MS) || r.len() >= RING_MAX {
            r.pop_front();
        }
        r.push_back(p.clone());
    }
    let subs: Vec<Sub> = st().subs.read().unwrap().clone();
    for s in subs {
        s(&ev);
    }
}

fn set_status(source: Source, error: Option<String>) {
    let mut s = st().status.lock().unwrap();
    s.source = source;
    s.error = error;
}

/// Start the feed. Tries ETW; on failure starts the snapshot fallback and
/// returns `Snapshot` (the reason is in `status().error`). Call once.
pub fn start() -> Source {
    st().stopping.store(false, Ordering::SeqCst);
    let r = start_etw();
    crate::osutil::log(&format!("etw start: {r:?}"));
    match r {
        Ok(()) => {
            set_status(Source::Etw, None);
            Source::Etw
        }
        Err(e) => {
            set_status(Source::Snapshot, Some(e));
            start_snapshot();
            Source::Snapshot
        }
    }
}

/// Stop the ETW session (ProcessTrace returns) or the fallback poller.
pub fn stop() {
    st().stopping.store(true, Ordering::SeqCst);
    let mut p = Props::new();
    unsafe {
        let _ = ControlTraceW(CONTROLTRACE_HANDLE { Value: st().ctl.load(Ordering::SeqCst) }, PCWSTR(p.name.as_ptr()), p.props(), EVENT_TRACE_CONTROL_STOP);
    }
    set_status(Source::Off, None);
}

// ---- ETW ---------------------------------------------------------------------

/// EVENT_TRACE_PROPERTIES followed by room for the logger name.
struct Props {
    buf: Vec<u64>,
    name: Vec<u16>,
}

impl Props {
    fn new() -> Props {
        let name = wide(session());
        let base = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let total = base + 1024; // logger name + log file name room
        let mut buf = vec![0u64; total / 8 + 1];
        let p = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        unsafe {
            (*p).Wnode.BufferSize = total as u32;
            (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*p).Wnode.ClientContext = 1; // QPC, converted to FILETIME by OpenTrace
            (*p).LoggerNameOffset = base as u32;
        }
        Props { buf, name }
    }
    fn props(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
        self.buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES
    }
}

fn start_etw() -> Result<(), String> {
    unsafe {
        // A session outlives a crashed owner: stop any stale one first.
        let mut p = Props::new();
        let _ = ControlTraceW(CONTROLTRACE_HANDLE { Value: 0 }, PCWSTR(p.name.as_ptr()), p.props(), EVENT_TRACE_CONTROL_STOP);

        let mut p = Props::new();
        let pp = p.props();
        (*pp).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*pp).BufferSize = 64; // KB
        (*pp).MinimumBuffers = 2;
        (*pp).MaximumBuffers = 4;
        (*pp).FlushTimer = 1; // s
        let mut h = CONTROLTRACE_HANDLE::default();
        let r = StartTraceW(&mut h, PCWSTR(p.name.as_ptr()), pp);
        if r != ERROR_SUCCESS {
            // 183 = a session left by an elevated Blink that we may not stop unelevated
            let why = match r.0 {
                5 => "needs admin",
                183 => "session busy; needs admin",
                _ => "error",
            };
            return Err(format!("StartTrace failed: {} ({why})", r.0));
        }
        st().ctl.store(h.Value, Ordering::SeqCst);
        let r = EnableTraceEx2(h, &PROVIDER, EVENT_CONTROL_CODE_ENABLE_PROVIDER.0, 4, KEYWORD_PROCESS, 0, 0, None);
        if r != ERROR_SUCCESS {
            let _ = ControlTraceW(h, PCWSTR::null(), pp, EVENT_TRACE_CONTROL_STOP);
            return Err(format!("EnableTraceEx2 failed: {}{}", r.0, if r == ERROR_ACCESS_DENIED { " (access denied: needs admin)" } else { "" }));
        }

        let mut name = wide(session());
        let mut lf = EVENT_TRACE_LOGFILEW::default();
        lf.LoggerName = PWSTR(name.as_mut_ptr());
        lf.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        lf.Anonymous2.EventRecordCallback = Some(on_event);
        let th = OpenTraceW(&mut lf);
        if th.Value == u64::MAX || th.Value == 0 {
            let e = GetLastError();
            let _ = ControlTraceW(h, PCWSTR::null(), pp, EVENT_TRACE_CONTROL_STOP);
            return Err(format!("OpenTrace failed: {}", e.0));
        }
        let thv = th.Value;
        std::thread::Builder::new()
            .name("blink-etw".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let _keep = name;
                let th = PROCESSTRACE_HANDLE { Value: thv };
                let r = ProcessTrace(&[th], None, None);
                let _ = CloseTrace(th);
                if !st().stopping.load(Ordering::SeqCst) {
                    // session killed from outside (logman stop, etc.): keep feeding
                    set_status(Source::Snapshot, Some(format!("ETW session ended: {}", r.0)));
                    start_snapshot();
                }
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn tdh_bytes(rec: *const EVENT_RECORD, name: &[u16], buf: &mut [u8]) -> Option<usize> {
    let d = [PROPERTY_DATA_DESCRIPTOR { PropertyName: name.as_ptr() as u64, ArrayIndex: u32::MAX, Reserved: 0 }];
    let mut size = 0u32;
    unsafe {
        if TdhGetPropertySize(rec, None, &d, &mut size) != 0 || size as usize > buf.len() {
            return None;
        }
        if TdhGetProperty(rec, None, &d, &mut buf[..size as usize]) != 0 {
            return None;
        }
    }
    Some(size as usize)
}

fn tdh_u32(rec: *const EVENT_RECORD, name: &[u16]) -> Option<u32> {
    let mut b = [0u8; 8];
    let n = tdh_bytes(rec, name, &mut b)?;
    (n >= 4).then(|| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn tdh_wstr(rec: *const EVENT_RECORD, name: &[u16]) -> Option<String> {
    let mut b = [0u8; 2048];
    let n = tdh_bytes(rec, name, &mut b)?;
    let w: Vec<u16> = b[..n].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(osutil::from_wide(&w))
}

struct Names {
    pid: Vec<u16>,
    ppid: Vec<u16>,
    session: Vec<u16>,
    image: Vec<u16>,
}

fn names() -> &'static Names {
    static N: OnceLock<Names> = OnceLock::new();
    N.get_or_init(|| Names { pid: wide("ProcessID"), ppid: wide("ParentProcessID"), session: wide("SessionID"), image: wide("ImageName") })
}

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let r = &*rec;
    if r.EventHeader.ProviderId != PROVIDER {
        return;
    }
    match r.EventHeader.EventDescriptor.Id {
        1 => {
            let n = names();
            let Some(pid) = tdh_u32(rec, &n.pid) else { return };
            let ppid = tdh_u32(rec, &n.ppid).unwrap_or(0);
            let session = tdh_u32(rec, &n.session).unwrap_or(0);
            let image = tdh_wstr(rec, &n.image).unwrap_or_default();
            let path = if image.is_empty() { osutil::image_path(pid).unwrap_or_default() } else { osutil::nt_to_dos(&image) };
            let t = osutil::filetime_to_ms(r.EventHeader.TimeStamp);
            dispatch(ProcEvent::Start(ProcStart { pid, ppid, session, path, time: if t == 0 { osutil::now_ms() } else { t } }));
        }
        2 => {
            // ProcessID is the first field in every ProcessStop version
            if r.UserDataLength >= 4 && !r.UserData.is_null() {
                let pid = *(r.UserData as *const u32);
                dispatch(ProcEvent::Stop { pid });
            }
        }
        _ => {}
    }
}

// ---- snapshot fallback ---------------------------------------------------------

/// One process in a snapshot: parent, identity (sequence number or create
/// time, whichever the source gives; a reused pid gets a new one) and where
/// its record sits in the list buffer (the name is read only for new processes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    pub ppid: u32,
    pub id: u64,
    off: u32,
}

/// Name of a process-list entry ("System" for the nameless kernel entries).
fn entry_name(pid: u32, name: &nt::UStr) -> String {
    if pid == 0 {
        "System Idle Process".to_string()
    } else if name.buf.is_null() {
        "System".to_string()
    } else {
        let s = unsafe { std::slice::from_raw_parts(name.buf, name.len as usize / 2) };
        String::from_utf16_lossy(s)
    }
}

/// Process list in a reused buffer and map (no allocation per poll).
#[derive(Default)]
struct List {
    buf: Vec<u64>,
    /// buf holds SystemBasicProcessInformation records (else SystemProcessInformation)
    basic: bool,
    map: HashMap<u32, Entry>,
}

static BASIC: AtomicBool = AtomicBool::new(true);

impl List {
    /// Refill from the kernel. SystemBasicProcessInformation (25H2+) when the
    /// kernel has it: it skips the per-thread records, which is what makes
    /// SystemProcessInformation cost ~10-20 us per process (5-13 ms a walk at
    /// 350-700 processes, every 2 s while unelevated). Older Windows: one
    /// SystemProcessInformation call (still a quarter of a Toolhelp snapshot).
    fn refresh(&mut self) -> bool {
        self.map.clear();
        self.basic = BASIC.load(Ordering::Relaxed);
        if self.basic && !nt::query(nt::SYSTEM_BASIC_PROCESS_INFORMATION, &mut self.buf) {
            // unknown class on this build: the full walk from now on
            BASIC.store(false, Ordering::Relaxed);
            self.basic = false;
        }
        if !self.basic && !nt::query(nt::SYSTEM_PROCESS_INFORMATION, &mut self.buf) {
            return false;
        }
        let base = self.buf.as_ptr() as *const u8;
        let mut off = 0usize;
        loop {
            let (next, pid, e) = if self.basic {
                let p = unsafe { &*(base.add(off) as *const nt::Bpi) };
                (p.next, p.pid as u32, Entry { ppid: p.ppid as u32, id: p.seq, off: off as u32 })
            } else {
                let p = unsafe { &*(base.add(off) as *const nt::Spi) };
                (p.next, p.pid as u32, Entry { ppid: p.ppid as u32, id: p.create as u64, off: off as u32 })
            };
            self.map.insert(pid, e);
            if next == 0 {
                break;
            }
            off += next as usize;
        }
        true
    }

    /// Exe name of a process in the current list.
    fn name(&self, pid: u32) -> String {
        let Some(e) = self.map.get(&pid) else { return String::new() };
        let at = unsafe { (self.buf.as_ptr() as *const u8).add(e.off as usize) };
        let name = if self.basic { unsafe { &(*(at as *const nt::Bpi)).name } } else { unsafe { &(*(at as *const nt::Spi)).name } };
        entry_name(pid, name)
    }
}

/// Pure diff step (testable): returns (started, stopped) pids. A pid whose
/// identity changed (pid reuse) is in both.
pub fn diff(old: &HashMap<u32, Entry>, new: &HashMap<u32, Entry>) -> (Vec<u32>, Vec<u32>) {
    let mut started = Vec::new();
    let mut stopped = Vec::new();
    for (pid, e) in new {
        match old.get(pid) {
            Some(o) if o.id == e.id => {}
            Some(_) => {
                stopped.push(*pid);
                started.push(*pid);
            }
            None => started.push(*pid),
        }
    }
    for pid in old.keys() {
        if !new.contains_key(pid) {
            stopped.push(*pid);
        }
    }
    (started, stopped)
}

fn start_snapshot() {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("blink-procpoll".into()).stack_size(128 * 1024).spawn(|| {
        let (mut prev, mut cur) = (List::default(), List::default());
        prev.refresh();
        // BLINK_TIMING: cycles per poll (list+diff, events), logged every 30 polls
        let timing = crate::timing::detail();
        let (mut polls, mut c_list, mut c_ev, mut n_ev) = (0u64, 0u64, 0u64, 0usize);
        while !st().stopping.load(Ordering::SeqCst) {
            // the fallback polls every 2 s; only elevated Blink gets exact ETW events
            std::thread::sleep(Duration::from_secs(2));
            let c0 = if timing { crate::timing::thread_cycles() } else { 0 };
            if !cur.refresh() {
                continue;
            }
            let (started, stopped) = diff(&prev.map, &cur.map);
            let c1 = if timing { crate::timing::thread_cycles() } else { 0 };
            n_ev += started.len() + stopped.len();
            for pid in stopped {
                dispatch(ProcEvent::Stop { pid });
            }
            for pid in started {
                let ppid = cur.map[&pid].ppid;
                let path = osutil::image_path(pid).unwrap_or_else(|| cur.name(pid));
                dispatch(ProcEvent::Start(ProcStart { pid, ppid, session: 0, path, time: osutil::now_ms() }));
            }
            std::mem::swap(&mut prev, &mut cur);
            if timing {
                polls += 1;
                c_list += c1 - c0;
                c_ev += crate::timing::thread_cycles() - c1;
                if polls % 30 == 0 {
                    osutil::log(&format!(
                        "procpoll: {} procs, per poll {:.2} Mcycles list+diff, {:.2} Mcycles events ({n_ev} events in 30 polls)",
                        prev.map.len(),
                        c_list as f64 / 30e6,
                        c_ev as f64 / 30e6
                    ));
                    (c_list, c_ev, n_ev) = (0, 0, 0);
                }
            }
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(ppid: u32, id: u64) -> Entry {
        Entry { ppid, id, off: 0 }
    }

    #[test]
    fn snapshot_diff() {
        let mut a = HashMap::new();
        a.insert(1, e(0, 10));
        a.insert(2, e(1, 20));
        let mut b = a.clone();
        b.remove(&1);
        b.insert(2, e(1, 21)); // reused pid: new identity
        b.insert(3, e(2, 30));
        let (mut s, mut t) = diff(&a, &b);
        s.sort();
        t.sort();
        assert_eq!(s, vec![2, 3]);
        assert_eq!(t, vec![1, 2]);
    }

    /// The cheap class lists the same processes, parents and names as the
    /// full walk, and stays stable between polls (skipped on Windows builds
    /// without it).
    #[test]
    fn basic_process_list_matches_full_walk() {
        let mut basic = List::default();
        assert!(basic.refresh());
        if !basic.basic {
            eprintln!("SystemBasicProcessInformation not supported here: skipped");
            return;
        }
        let mut full = List::default();
        assert!(nt::query(nt::SYSTEM_PROCESS_INFORMATION, &mut full.buf));
        let base = full.buf.as_ptr() as *const u8;
        let mut off = 0usize;
        loop {
            let p = unsafe { &*(base.add(off) as *const nt::Spi) };
            full.map.insert(p.pid as u32, Entry { ppid: p.ppid as u32, id: p.create as u64, off: off as u32 });
            if p.next == 0 {
                break;
            }
            off += p.next as usize;
        }
        // processes come and go between the two calls: compare the common ones
        let common: Vec<u32> = basic.map.keys().filter(|p| full.map.contains_key(p)).copied().collect();
        assert!(common.len() >= basic.map.len() * 9 / 10 && common.len() > 20, "{} of {}", common.len(), basic.map.len());
        for pid in common {
            assert_eq!(basic.map[&pid].ppid, full.map[&pid].ppid, "pid {pid}");
            assert_eq!(basic.name(pid), full.name(pid), "pid {pid}");
        }
        assert!(basic.map.iter().all(|(pid, e)| *pid == 0 || e.id != 0), "every process has a sequence number");
        assert_eq!(basic.name(4), "System");
        let mut again = List::default();
        assert!(again.refresh());
        let (s, t) = diff(&basic.map, &again.map);
        assert!(s.len() + t.len() < 10, "stable between polls: {s:?} {t:?}");
    }

    /// `cargo test poll_cost -- --ignored --nocapture`: one fallback poll (list + diff)
    #[test]
    #[ignore]
    fn poll_cost() {
        let (mut prev, mut cur) = (List::default(), List::default());
        prev.refresh();
        let (c0, t0) = (crate::timing::thread_cycles(), std::time::Instant::now());
        let mut churn = 0;
        for _ in 0..50 {
            cur.refresh();
            let (a, b) = diff(&prev.map, &cur.map);
            churn += a.len() + b.len();
            std::mem::swap(&mut prev, &mut cur);
        }
        eprintln!("churn over 50 polls: {churn}");
        eprintln!(
            "poll: {:.1} us, {:.2} Mcycles ({} procs)",
            t0.elapsed().as_secs_f64() * 1e6 / 50.0,
            (crate::timing::thread_cycles() - c0) as f64 / 50.0 / 1e6,
            prev.map.len()
        );
    }

    /// Elevated smoke test: `cargo test etw_live -- --ignored` from an admin shell,
    /// then start any process within 5 s.
    #[test]
    #[ignore]
    fn etw_live() {
        let got = Arc::new(AtomicU64::new(0));
        let g = got.clone();
        subscribe(move |e| {
            if let ProcEvent::Start(p) = e {
                eprintln!("start {} {} {}", p.pid, p.ppid, p.path);
                g.fetch_add(1, Ordering::SeqCst);
            }
        });
        let src = start();
        eprintln!("source {:?} {:?}", src, status().error);
        let _ = crate::osutil::quiet_cmd("cmd").args(["/c", "exit"]).status();
        std::thread::sleep(Duration::from_secs(3));
        stop();
        assert_eq!(src, Source::Etw);
        assert!(got.load(Ordering::SeqCst) > 0);
    }
}
