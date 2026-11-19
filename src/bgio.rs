//! One long-lived background I/O worker. Everything that can block on a disk,
//! a driver, another process, the network, SCM or the registry runs here,
//! never on the sampler or the UI thread: drive temperatures, PawnIO, process
//! details (exe version resources, command lines), the Services and Startup
//! lists, and every file write (log, Watch events, settings).
//!
//! Callers keep a [`Cell`] per result. `refresh` queues a job unless one is
//! already in flight for that cell (no duplicates) or the value is still
//! fresh; `get`/`read` return the latest value with its age and never wait.
//!
//! The worker is one thread with a small stack sleeping on a condvar (no
//! polling). Should a call hang (a dead disk, a stuck driver), the
//! [`watchdog`] the sampler runs each tick hands the queue to a fresh worker
//! after [`STUCK`]; the stuck thread exits once its call returns. At most
//! [`MAX_SPARE`] such threads are ever alive, so a pile of hung calls cannot
//! grow threads without bound. The hung call's cell stays "in flight" until
//! it returns, so the same device is not asked again meanwhile, and after a
//! hang that device is asked only every [`SLOW_RETRY`].
//!
//! File writes and the log go in a fast lane the worker serves first, so they
//! never queue behind slow device queries.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A job running longer than this is considered hung (watchdog).
pub const STUCK: Duration = Duration::from_secs(3);
/// A value whose refresh has been outstanding (or failing) this long shows as stale.
pub const STALE: Duration = Duration::from_secs(10);
/// Hung workers that may be alive at once (besides the live one).
pub const MAX_SPARE: u32 = 3;
/// After a call hung (ran past [`STUCK`]), its cell retries at most this often.
pub const SLOW_RETRY: Duration = Duration::from_secs(60);

type Job = Box<dyn FnOnce() + Send>;

enum Item {
    Run(&'static str, Job),
    /// atomic replace; a newer write of the same path replaces a queued one
    Write(PathBuf, Vec<u8>),
    Append(PathBuf, String),
}

#[derive(Default)]
struct Queue {
    /// file writes and log lines: served before `items`, in order
    fast: VecDeque<Item>,
    /// device queries and other jobs that may be slow
    items: VecDeque<Item>,
    /// generation of the live worker; a worker whose generation is older exits
    gen: u64,
    started: bool,
    /// (since, label) of the job the live worker runs
    busy: Option<(Instant, &'static str)>,
    /// abandoned (hung) workers still alive
    spare: u32,
    /// workers handed over so far (for the log / tests)
    handovers: u32,
}

static Q: Mutex<Queue> = Mutex::new(Queue { fast: VecDeque::new(), items: VecDeque::new(), gen: 0, started: false, busy: None, spare: 0, handovers: 0 });
static CV: Condvar = Condvar::new();

fn q() -> MutexGuard<'static, Queue> {
    Q.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn spawn_worker(gen: u64) -> bool {
    std::thread::Builder::new().name("hayate-io".into()).stack_size(256 * 1024).spawn(move || run(gen)).is_ok()
}

fn push(item: Item, fast: bool) {
    let mut g = q();
    if let Item::Write(p, _) = &item {
        g.fast.retain(|i| !matches!(i, Item::Write(q, _) if q == p));
    }
    if fast {
        g.fast.push_back(item);
    } else {
        g.items.push_back(item);
    }
    if !g.started {
        g.started = true;
        let gen = g.gen;
        drop(g);
        if !spawn_worker(gen) {
            q().started = false;
        }
        return;
    }
    drop(g);
    CV.notify_one();
}

/// Start the worker now (normally it starts with the first job). Callers that
/// log or write files before then do so synchronously.
pub fn start() {
    let mut g = q();
    if !g.started {
        g.started = true;
        let gen = g.gen;
        drop(g);
        if !spawn_worker(gen) {
            q().started = false;
        }
    }
}

/// True once the worker runs (file writes then go through its queue).
pub fn running() -> bool {
    !cfg!(test) && q().started
}

/// Wait up to `timeout` for queued jobs and writes to finish (at exit, so
/// settings and the log are not lost). Never called on a hot path.
pub fn flush(timeout: Duration) {
    let t0 = Instant::now();
    while pending() > 0 && t0.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Queue `job` on the worker. Prefer [`Cell::refresh`], which also dedups.
pub fn submit(label: &'static str, job: impl FnOnce() + Send + 'static) {
    push(Item::Run(label, Box::new(job)), false);
}

/// Queue a short file job (a log line) in the fast lane, ahead of device queries.
pub fn submit_fast(label: &'static str, job: impl FnOnce() + Send + 'static) {
    push(Item::Run(label, Box::new(job)), true);
}

/// Replace `path` atomically with `data` in the background (in queue order;
/// a queued older write of the same path is dropped).
pub fn write_file(path: PathBuf, data: Vec<u8>) {
    if cfg!(test) {
        let _ = crate::osutil::write_atomic(&path, &data);
        return;
    }
    push(Item::Write(path, data), true);
}

/// Append `text` to `path` in the background, in queue order.
pub fn append(path: PathBuf, text: String) {
    if cfg!(test) {
        do_append(&path, &text);
        return;
    }
    push(Item::Append(path, text), true);
}

fn do_append(path: &std::path::Path, text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

fn run(gen: u64) {
    loop {
        let item = {
            let mut g = q();
            loop {
                if g.gen != gen {
                    g.spare = g.spare.saturating_sub(1);
                    return;
                }
                if let Some(i) = g.fast.pop_front().or_else(|| g.items.pop_front()) {
                    let label = match &i {
                        Item::Run(l, _) => *l,
                        Item::Write(..) => "file write",
                        Item::Append(..) => "file append",
                    };
                    g.busy = Some((Instant::now(), label));
                    break i;
                }
                g = CV.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        };
        match item {
            // a panicking job must not take the worker down (the panic hook logs it)
            Item::Run(_, job) => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            }
            Item::Write(p, d) => {
                let _ = crate::osutil::write_atomic(&p, &d);
            }
            Item::Append(p, t) => do_append(&p, &t),
        }
        let mut g = q();
        if g.gen == gen {
            g.busy = None;
        }
    }
}

/// Called once per sampler tick: when the live worker has been stuck in one
/// job for longer than [`STUCK`], a fresh worker takes over the queue.
pub fn watchdog() {
    let mut g = q();
    let Some((since, label)) = g.busy else { return };
    if since.elapsed() < STUCK || g.spare >= MAX_SPARE {
        return;
    }
    g.gen += 1;
    g.spare += 1;
    g.handovers += 1;
    g.busy = None;
    let gen = g.gen;
    let n = g.handovers;
    drop(g);
    if !spawn_worker(gen) {
        // no thread: give the old one its queue back
        let mut g = q();
        g.gen -= 1;
        g.spare -= 1;
        return;
    }
    crate::osutil::log(&format!("io: '{label}' stuck for {:.1} s, new worker took over ({n} so far)", since.elapsed().as_secs_f32()));
}

/// Jobs queued plus the one running (for tests and the debug log).
pub fn pending() -> usize {
    let g = q();
    g.fast.len() + g.items.len() + g.busy.is_some() as usize
}

// ---------- cached results ----------

/// Bookkeeping of one cached result. Pure, so the rules are unit-testable.
#[derive(Debug, Clone)]
pub struct Slot<T> {
    pub val: Option<T>,
    /// when `val` last arrived
    pub at: Option<Instant>,
    /// set while a refresh is queued or running
    pub inflight: Option<Instant>,
    /// when the last refresh finished (good or not)
    pub tried: Option<Instant>,
    /// the last refresh failed (the old value is kept)
    pub failed: bool,
    /// the last refresh hung (ran past [`STUCK`]): retry only every [`SLOW_RETRY`]
    pub slow: bool,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot { val: None, at: None, inflight: None, tried: None, failed: false, slow: false }
    }
}

impl<T> Slot<T> {
    /// A refresh is due: none in flight and the last attempt is `max_age` old
    /// ([`SLOW_RETRY`] when it hung).
    pub fn due(&self, now: Instant, max_age: Duration) -> bool {
        let wait = if self.slow { max_age.max(SLOW_RETRY) } else { max_age };
        self.inflight.is_none() && self.tried.is_none_or(|t| now.saturating_duration_since(t) >= wait)
    }
    pub fn start(&mut self, now: Instant) {
        self.inflight = Some(now);
    }
    /// `None` = the refresh failed: keep the old value, mark it.
    pub fn finish(&mut self, v: Option<T>, now: Instant) {
        self.slow = self.inflight.is_some_and(|t| now.saturating_duration_since(t) > STUCK);
        self.inflight = None;
        self.tried = Some(now);
        match v {
            Some(v) => {
                self.val = Some(v);
                self.at = Some(now);
                self.failed = false;
            }
            None => self.failed = true,
        }
    }
    /// The value is old: its refresh hangs, or keeps failing, for over [`STALE`].
    pub fn stale(&self, now: Instant) -> bool {
        self.val.is_some()
            && (self.inflight.is_some_and(|t| now.saturating_duration_since(t) > STALE)
                || (self.failed && self.at.is_some_and(|t| now.saturating_duration_since(t) > STALE)))
    }
}

/// What a reader sees: the latest value, its age, and whether it is stale.
#[derive(Debug, Clone)]
#[allow(dead_code)] // age/pending: for callers and tests that need them
pub struct Reading<T> {
    pub val: Option<T>,
    pub age: Option<Duration>,
    pub stale: bool,
    pub pending: bool,
}

/// A result produced on the worker and read anywhere without waiting.
pub struct Cell<T>(Arc<Mutex<Slot<T>>>);

impl<T> Clone for Cell<T> {
    fn clone(&self) -> Self {
        Cell(self.0.clone())
    }
}

impl<T: Send + 'static> Default for Cell<T> {
    fn default() -> Self {
        Cell(Arc::new(Mutex::new(Slot::default())))
    }
}

impl<T: Clone + Send + 'static> Cell<T> {
    /// Queue `f` on the worker unless a refresh is in flight or the last one
    /// finished less than `max_age` ago. `f` returns `None` on failure.
    /// True when a job was queued.
    pub fn refresh(&self, label: &'static str, max_age: Duration, f: impl FnOnce() -> Option<T> + Send + 'static) -> bool {
        let now = Instant::now();
        {
            let mut s = lock(&self.0);
            if !s.due(now, max_age) {
                return false;
            }
            s.start(now);
        }
        let c = self.0.clone();
        submit(label, move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok().flatten();
            lock(&c).finish(r, Instant::now());
        });
        true
    }

    pub fn get(&self) -> Option<T> {
        lock(&self.0).val.clone()
    }

    pub fn read(&self) -> Reading<T> {
        let now = Instant::now();
        let s = lock(&self.0);
        Reading { val: s.val.clone(), age: s.at.map(|t| now.saturating_duration_since(t)), stale: s.stale(now), pending: s.inflight.is_some() }
    }

    /// Forget the last attempt so the next `refresh` runs right away (unless
    /// the last one hung: then [`SLOW_RETRY`] still applies).
    pub fn invalidate(&self) {
        let mut s = lock(&self.0);
        if !s.slow {
            s.tried = None;
        }
    }
}

// ---------- fault injection (debug builds) ----------

/// `HAYATE_FAKE_STALL=disk:5000[,pawnio:3000,meta:2000,scm:2000]` makes that
/// worker path sleep the given ms, to prove the sampler and UI never wait.
/// `disk` covers drive temperature queries and process-detail file reads.
pub fn fake_stall(kind: &str) {
    #[cfg(debug_assertions)]
    {
        static CFG: std::sync::OnceLock<Vec<(String, u64)>> = std::sync::OnceLock::new();
        let cfg = CFG.get_or_init(|| crate::osutil::env_var("FAKE_STALL").map(|v| parse_stall(&v)).unwrap_or_default());
        if let Some((_, ms)) = cfg.iter().find(|(k, _)| k == kind) {
            std::thread::sleep(Duration::from_millis(*ms));
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = kind;
}

#[cfg(any(debug_assertions, test))]
fn parse_stall(v: &str) -> Vec<(String, u64)> {
    v.split(',').filter_map(|p| p.split_once(':')).filter_map(|(k, ms)| Some((k.trim().to_ascii_lowercase(), ms.trim().parse().ok()?))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn slot_dedups_in_flight_and_respects_max_age() {
        let b = Instant::now();
        let mut s: Slot<u32> = Slot::default();
        assert!(s.due(b, Duration::from_secs(15)));
        s.start(b);
        // in flight: never due, however old
        assert!(!s.due(t(b, 60_000), Duration::ZERO));
        s.finish(Some(41), t(b, 100));
        assert!(!s.due(t(b, 5_000), Duration::from_secs(15)));
        assert!(s.due(t(b, 15_100), Duration::from_secs(15)));
        assert_eq!(s.val, Some(41));
    }

    #[test]
    fn slot_failure_keeps_value_and_backs_off() {
        let b = Instant::now();
        let mut s: Slot<u32> = Slot::default();
        s.start(b);
        s.finish(Some(40), t(b, 10));
        s.start(t(b, 1_000));
        s.finish(None, t(b, 1_010));
        assert_eq!(s.val, Some(40));
        assert!(s.failed);
        // a failed try counts as a try: no hammering
        assert!(!s.due(t(b, 1_500), Duration::from_secs(1)));
        assert!(!s.stale(t(b, 5_000)));
        assert!(s.stale(t(b, 10_100)));
        s.start(t(b, 11_000));
        s.finish(Some(42), t(b, 11_010));
        assert!(!s.stale(t(b, 11_020)));
    }

    #[test]
    fn slot_hung_refresh_goes_stale() {
        let b = Instant::now();
        let mut s: Slot<u32> = Slot::default();
        s.start(b);
        // never had a value: nothing to mark stale
        assert!(!s.stale(t(b, 30_000)));
        s.finish(Some(1), t(b, 30_000));
        s.start(t(b, 31_000));
        assert!(!s.stale(t(b, 40_000)));
        assert!(s.stale(t(b, 41_500)));
    }

    #[test]
    fn slot_after_a_hang_retries_slowly() {
        let b = Instant::now();
        let mut s: Slot<u32> = Slot::default();
        s.start(b);
        s.finish(Some(1), t(b, 5_000)); // took 5 s: hung
        assert!(s.slow);
        assert!(!s.due(t(b, 20_000), Duration::from_secs(1)));
        assert!(s.due(t(b, 65_000), Duration::from_secs(1)));
        s.start(t(b, 65_000));
        s.finish(Some(2), t(b, 65_010)); // quick again: normal cadence
        assert!(!s.slow);
        assert!(s.due(t(b, 66_100), Duration::from_secs(1)));
    }

    #[test]
    fn fast_lane_goes_first() {
        let mut g = Queue::default();
        g.items.push_back(Item::Run("slow", Box::new(|| {})));
        g.fast.push_back(Item::Append(PathBuf::new(), String::new()));
        let first = g.fast.pop_front().or_else(|| g.items.pop_front());
        assert!(matches!(first, Some(Item::Append(..))));
    }

    #[test]
    fn stall_spec_parses() {
        assert_eq!(parse_stall("disk:5000"), vec![("disk".into(), 5000)]);
        assert_eq!(parse_stall("Disk:5, pawnio:7,bad,x:y"), vec![("disk".into(), 5), ("pawnio".into(), 7)]);
    }

    fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// One test drives the real worker (it is process-global): dedup while a
    /// job blocks, readers never wait, and the watchdog hands over a hung queue.
    #[test]
    fn worker_dedups_never_blocks_and_survives_a_hang() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let hung: Cell<u32> = Cell::default();
        let g2 = gate.clone();
        assert!(hung.refresh("test hang", Duration::ZERO, move || {
            let (m, c) = &*g2;
            let mut open = m.lock().unwrap();
            while !*open {
                open = c.wait(open).unwrap();
            }
            Some(7)
        }));
        // a second request while the first is in flight is dropped
        assert!(!hung.refresh("test hang", Duration::ZERO, || Some(8)));
        // readers return at once
        let t0 = Instant::now();
        let r = hung.read();
        assert!(r.val.is_none() && r.pending);
        assert!(t0.elapsed() < Duration::from_millis(50));

        // another cell queued behind the hung job
        let other: Cell<u32> = Cell::default();
        assert!(other.refresh("test other", Duration::ZERO, || Some(1)));
        assert!(wait_until(|| q().busy.is_some_and(|b| b.1 == "test hang")));
        std::thread::sleep(Duration::from_millis(50));
        assert!(other.get().is_none(), "single worker: queued behind the hang");

        // fake the hang being old, then the watchdog hands over
        q().busy.as_mut().unwrap().0 -= STUCK + Duration::from_millis(10);
        watchdog();
        assert!(wait_until(|| other.get() == Some(1)), "fresh worker ran the queued job");
        assert!(hung.read().pending, "hung call stays in flight: no duplicate");

        // the hung call returns: its value lands, the old thread exits
        {
            let (m, c) = &*gate;
            *m.lock().unwrap() = true;
            c.notify_all();
        }
        assert!(wait_until(|| hung.get() == Some(7)));
        assert!(wait_until(|| q().spare == 0));
        assert!(!hung.read().pending);
    }
}
