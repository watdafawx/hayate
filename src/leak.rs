//! Memory-leak and memory-pressure detection. Fed every 30 s by the sampler
//! (hidden or shown) with each process's private commit, it keeps ~2 h of
//! history per process and flags the ones whose *floor* keeps rising: the
//! rolling minimum per 5-minute bucket must climb steadily, which ignores the
//! sawtooth of a healthy garbage-collected app but catches a real leak.
//! Pure logic (no Win32): unit-tested below.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Seconds between feeds (the sampler's cadence). Debug builds can shorten it
/// with HAYATE_LEAKSTEP=<s> to test end to end without waiting 20 minutes.
pub fn step_s() -> u64 {
    #[cfg(debug_assertions)]
    {
        static STEP: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        return *STEP.get_or_init(|| crate::osutil::env_var("LEAKSTEP").ok().and_then(|v| v.parse().ok()).filter(|v| *v >= 1).unwrap_or(30));
    }
    #[cfg(not(debug_assertions))]
    30
}
const MAX_PTS: usize = 240; // 2 h
const BUCKET: usize = 10; // samples per bucket = 5 min
const MIN_BUCKETS: usize = 4; // >= 20 min of history
const TRACK_MIN_MB: u32 = 64;
const MIN_RISE_MB: u32 = 256;
const MIN_REL_RISE: f32 = 0.20;
const MIN_RATE_MB_H: f32 = 200.0;
const TOLERANCE: f32 = 0.97; // a floor may dip 3 % between buckets
const REALERT_AFTER: Duration = Duration::from_secs(3600);
const PRESSURE_COOLDOWN: Duration = Duration::from_secs(1800);

/// Images whose commit legitimately grows with the machine's workload, or that
/// the user can't sensibly restart: never reported as leaking. Lowercase, no ".exe".
const IGNORED: &[&str] = &[
    // kernel / memory manager / VMs
    "system", "registry", "memory compression", "idle", "vmmem", "vmmemwsl", "vmwp", "vmcompute", "wslservice",
    // core Windows: grows with windows, monitors, sessions, scans
    "dwm", "csrss", "lsass", "services", "smss", "wininit", "winlogon", "fontdrvhost", "audiodg", "svchost",
    "msmpeng", "mpdefendercoreservice", "searchindexer", "searchhost", "nissrv",
    // runtimes and build tools: heaps grow by design (JVM, compilers, linkers)
    "java", "javaw", "rustc", "cargo", "link", "cl", "clang", "rust-analyzer", "msbuild",
    // container / database engines
    "com.docker.backend", "dockerd", "sqlservr",
];

/// User additions: one image name per line in `<data dir>\leak-ignore.txt`
/// ("#" comments, ".exe" optional). Read off-thread every few minutes.
static USER_IGNORED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn norm(name: &str) -> String {
    let n = name.trim().to_lowercase();
    n.strip_suffix(".exe").unwrap_or(&n).to_string()
}

pub fn parse_ignore_file(text: &str) -> Vec<String> {
    text.lines().map(|l| l.split('#').next().unwrap_or("")).map(norm).filter(|l| !l.is_empty()).collect()
}

fn ignored(name: &str) -> bool {
    let n = norm(name);
    IGNORED.contains(&n.as_str()) || USER_IGNORED.lock().map(|u| u.contains(&n)).unwrap_or(false)
}

/// Is this image on the built-in or the user's ignore list (Watch dims its leak rows).
pub fn is_ignored(name: &str) -> bool {
    ignored(name)
}

/// Is this image on the built-in list (cannot be removed)?
pub fn is_builtin(name: &str) -> bool {
    IGNORED.contains(&norm(name).as_str())
}

/// Built-in ignore list (read-only, for Settings).
pub fn builtin_ignored() -> &'static [&'static str] {
    IGNORED
}

/// The user's ignore list as last read or edited (for Settings).
pub fn user_ignored() -> Vec<String> {
    USER_IGNORED.lock().map(|u| u.clone()).unwrap_or_default()
}

/// Serialises reads and edits of leak-ignore.txt (both run off the UI thread).
static FILE_LOCK: Mutex<()> = Mutex::new(());

fn ignore_path() -> std::path::PathBuf {
    crate::osutil::data_dir().join("leak-ignore.txt")
}

/// `text` with `name` appended as a line, or None when it is already listed.
pub fn add_to_ignore_text(text: &str, name: &str) -> Option<String> {
    let n = norm(name);
    if n.is_empty() || parse_ignore_file(text).contains(&n) {
        return None;
    }
    let mut out = text.to_string();
    if out.is_empty() {
        out.push_str("# Hayate: never report these apps as leaking memory (one image name per line)\n");
    } else if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&n);
    out.push_str(".exe\n");
    Some(out)
}

/// `text` without the lines naming `name` (comments and other lines kept), or None when absent.
pub fn remove_from_ignore_text(text: &str, name: &str) -> Option<String> {
    let n = norm(name);
    let keep: Vec<&str> = text.lines().filter(|l| norm(l.split('#').next().unwrap_or("")) != n).collect();
    if keep.len() == text.lines().count() {
        return None;
    }
    let mut out = keep.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Some(out)
}

/// Add (`add`) or remove an image name in leak-ignore.txt. The in-memory list
/// changes at once (the UI dims rows on the next paint); the file is read,
/// edited and replaced atomically on the background I/O worker.
pub fn set_user_ignored(name: &str, add: bool) {
    let n = norm(name);
    if n.is_empty() {
        return;
    }
    if let Ok(mut u) = USER_IGNORED.lock() {
        if add && !u.contains(&n) {
            u.push(n.clone());
        } else if !add {
            u.retain(|x| *x != n);
        }
    }
    crate::bgio::submit_fast("leak-ignore edit", move || {
        let _g = FILE_LOCK.lock();
        let path = ignore_path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let new = if add { add_to_ignore_text(&text, &n) } else { remove_from_ignore_text(&text, &n) };
        let text = match new {
            Some(t) => match crate::osutil::write_atomic(&path, t.as_bytes()) {
                Ok(()) => t,
                Err(e) => {
                    crate::osutil::log(&format!("leak-ignore: write {} failed: {e}", path.display()));
                    text
                }
            },
            None => text,
        };
        // immediate reload: the file is the truth (a hand edit made meanwhile shows too)
        if let Ok(mut u) = USER_IGNORED.lock() {
            *u = parse_ignore_file(&text);
        }
    });
}

/// Reload leak-ignore.txt on a short-lived thread: a busy disk never holds up the sampler.
fn reload_user_ignored() {
    let _ = std::thread::Builder::new().name("leak-ignore".into()).spawn(|| {
        let _g = FILE_LOCK.lock();
        let list = std::fs::read_to_string(ignore_path()).map(|t| parse_ignore_file(&t)).unwrap_or_default();
        if let Ok(mut u) = USER_IGNORED.lock() {
            *u = list;
        }
    });
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub pid: u32,
    pub create: i64,
    pub name: String,
    /// private commit, bytes
    pub commit: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MemInfo {
    pub total: u64,
    pub avail: u64,
    pub commit: u64,
    pub commit_limit: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Suspect {
    pub pid: u32,
    pub name: String,
    pub now_mb: u32,
    pub start_mb: u32,
    pub rate_mb_h: f32,
    /// minutes of history the verdict rests on
    pub minutes: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Alert {
    Leak(Suspect),
    Pressure { free_mb: u64, commit_pct: u32, top: Vec<(String, u32)> },
}

struct Track {
    name: String,
    mb: VecDeque<u32>,
    seen: u64,
}

#[derive(Default)]
pub struct Tracker {
    procs: HashMap<(u32, i64), Track>,
    alerted: HashMap<(u32, i64), (Instant, u32)>,
    feeds: u64,
    pressure_streak: u32,
    pressure_at: Option<Instant>,
    pub suspects: Vec<Suspect>,
}

/// Verdict on one process's history (oldest → newest, one value per feed).
pub fn judge(mb: &VecDeque<u32>) -> Option<(u32, u32, f32, u32)> {
    judge_step(mb, step_s())
}

fn judge_step(mb: &VecDeque<u32>, step: u64) -> Option<(u32, u32, f32, u32)> {
    let n = mb.len();
    if n < MIN_BUCKETS * BUCKET {
        return None;
    }
    // buckets aligned to the newest sample so the last one is always full
    let v: Vec<u32> = mb.iter().copied().collect();
    let nb = (n / BUCKET).min(MAX_PTS / BUCKET);
    let mut mins = Vec::with_capacity(nb);
    for b in 0..nb {
        let end = n - (nb - 1 - b) * BUCKET;
        mins.push(*v[end - BUCKET..end].iter().min().unwrap());
    }
    if mins.windows(2).any(|w| (w[1] as f32) < w[0] as f32 * TOLERANCE) {
        return None;
    }
    let (first, last) = (mins[0], *mins.last().unwrap());
    // must still be growing in the last ~10 min, not a one-off jump that settled
    if mins.len() >= 3 && (last as f32) <= mins[mins.len() - 3] as f32 * 1.01 {
        return None;
    }
    let rise = last.saturating_sub(first);
    let hours = (mins.len() - 1) as f32 * (BUCKET as f32 * step as f32) / 3600.0;
    let rate = rise as f32 / hours;
    if rise >= MIN_RISE_MB && rise as f32 >= first as f32 * MIN_REL_RISE && rate >= MIN_RATE_MB_H {
        Some((first, last, rate, (nb * BUCKET) as u32 * step as u32 / 60))
    } else {
        None
    }
}

impl Tracker {
    /// One 30 s feed. Returns the alerts that fire now.
    pub fn feed(&mut self, samples: &[Sample], mem: MemInfo, now: Instant) -> Vec<Alert> {
        self.feeds += 1;
        if self.feeds % 10 == 1 {
            reload_user_ignored();
        }
        // a name added to the ignore list stops being tracked right away
        self.procs.retain(|_, t| !ignored(&t.name));
        for s in samples {
            let mb = (s.commit >> 20).min(u32::MAX as u64) as u32;
            let key = (s.pid, s.create);
            match self.procs.get_mut(&key) {
                Some(t) => {
                    if t.mb.len() >= MAX_PTS {
                        t.mb.pop_front();
                    }
                    t.mb.push_back(mb);
                    t.seen = self.feeds;
                }
                None if mb >= TRACK_MIN_MB && !ignored(&s.name) => {
                    let mut d = VecDeque::new();
                    d.push_back(mb);
                    self.procs.insert(key, Track { name: s.name.clone(), mb: d, seen: self.feeds });
                }
                None => {}
            }
        }
        let feeds = self.feeds;
        self.procs.retain(|_, t| t.seen == feeds);
        self.alerted.retain(|k, _| self.procs.contains_key(k));

        let mut out = Vec::new();
        let mut suspects = Vec::new();
        for (key, t) in &self.procs {
            if let Some((start, now_mb, rate, minutes)) = judge(&t.mb) {
                let sus = Suspect { pid: key.0, name: t.name.clone(), now_mb, start_mb: start, rate_mb_h: rate, minutes };
                let fire = match self.alerted.get(key) {
                    None => true,
                    Some((at, mb)) => now.duration_since(*at) >= REALERT_AFTER && now_mb >= mb.saturating_mul(2),
                };
                if fire {
                    self.alerted.insert(*key, (now, now_mb));
                    out.push(Alert::Leak(sus.clone()));
                }
                suspects.push(sus);
            }
        }
        suspects.sort_by(|a, b| b.rate_mb_h.total_cmp(&a.rate_mb_h));
        if let Ok(mut g) = SUSPECTS.lock() {
            *g = suspects.clone();
        }
        self.suspects = suspects;

        // memory pressure: physical almost gone or commit near the limit, for 2 feeds in a row
        let commit_pct = if mem.commit_limit > 0 { (mem.commit * 100 / mem.commit_limit) as u32 } else { 0 };
        let free_pct = if mem.total > 0 { (mem.avail * 100 / mem.total) as u32 } else { 100 };
        if commit_pct >= 92 || free_pct <= 5 {
            self.pressure_streak += 1;
        } else {
            self.pressure_streak = 0;
        }
        let cooled = self.pressure_at.map(|t| now.duration_since(t) >= PRESSURE_COOLDOWN).unwrap_or(true);
        if self.pressure_streak >= 2 && cooled {
            self.pressure_at = Some(now);
            // top consumers by name (a browser is many processes)
            let mut by: HashMap<&str, u64> = HashMap::new();
            for s in samples {
                *by.entry(s.name.as_str()).or_default() += s.commit;
            }
            let mut top: Vec<(String, u32)> = by.into_iter().map(|(n, c)| (n.to_string(), (c >> 20) as u32)).collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            top.truncate(3);
            out.push(Alert::Pressure { free_mb: mem.avail >> 20, commit_pct, top });
        }
        out
    }
}

static SUSPECTS: Mutex<Vec<Suspect>> = Mutex::new(Vec::new());

/// Current leak suspects, worst first (for the History tab).
/// One just ignored drops out at once, not at the next 30 s feed.
pub fn suspects() -> Vec<Suspect> {
    let mut v = SUSPECTS.lock().map(|s| s.clone()).unwrap_or_default();
    v.retain(|s| !ignored(&s.name));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(f: impl Fn(usize) -> u32, n: usize) -> VecDeque<u32> {
        (0..n).map(f).collect()
    }

    #[test]
    fn ignore_file_and_defaults() {
        assert_eq!(parse_ignore_file("# mine\nFoo.EXE\n  bar  # note\n\n"), vec!["foo", "bar"]);
        assert!(ignored("dwm.exe"));
        assert!(ignored("JavaW.exe"));
        assert!(!ignored("chrome.exe"));
    }

    #[test]
    fn ignore_file_edits() {
        let t = add_to_ignore_text("", "Hopper.exe").unwrap();
        assert!(t.starts_with('#') && t.ends_with("hopper.exe\n"), "{t}");
        assert_eq!(parse_ignore_file(&t), vec!["hopper"]);
        assert_eq!(add_to_ignore_text(&t, "HOPPER"), None, "already listed");
        let t2 = add_to_ignore_text("foo  # mine", "bar").unwrap();
        assert_eq!(t2, "foo  # mine\nbar.exe\n");
        let t3 = remove_from_ignore_text(&t2, "FOO.exe").unwrap();
        assert_eq!(t3, "bar.exe\n");
        assert_eq!(remove_from_ignore_text(&t3, "baz"), None);
        assert_eq!(remove_from_ignore_text("x.exe\n", "x").unwrap(), "");
        assert!(is_builtin("Dwm.exe") && !is_builtin("hopper"));
    }

    #[test]
    fn steady_growth_is_a_leak() {
        // 500 MB growing 40 MB per 5 min for 40 min => 480 MB/h
        let s = series(|i| 500 + (i as u32 / 10) * 40, 80);
        let (start, now, rate, _) = judge_step(&s, 30).expect("leak");
        assert!(now > start);
        assert!(rate > 400.0, "{rate}");
    }

    #[test]
    fn sawtooth_with_rising_floor_is_a_leak() {
        // GC dips of 100 MB every few minutes on top of +60 MB per 5 min
        let s = series(|i| 800 + (i as u32 / 10) * 60 + if i % 7 == 0 { 0 } else { 100 }, 90);
        assert!(judge_step(&s, 30).is_some());
    }

    #[test]
    fn flat_is_not_a_leak() {
        assert!(judge_step(&series(|i| 1000 + (i as u32 % 5), 120), 30).is_none());
    }

    #[test]
    fn healthy_sawtooth_is_not_a_leak() {
        // heap grows to 2 GB then collects back to 1 GB, repeatedly
        assert!(judge_step(&series(|i| 1000 + (i as u32 % 30) * 35, 120), 30).is_none());
    }

    #[test]
    fn one_off_jump_that_settled_is_not_a_leak() {
        // loaded a big file 25 min ago, flat since
        let s = series(|i| if i < 40 { 300 } else { 2300 }, 100);
        assert!(judge_step(&s, 30).is_none());
    }

    #[test]
    fn too_little_history_is_ignored() {
        assert!(judge_step(&series(|i| 500 + i as u32 * 10, 30), 30).is_none());
    }

    #[test]
    fn small_slow_growth_is_ignored() {
        // +10 MB per 5 min on 2 GB: 120 MB/h and < 256 MB total
        assert!(judge_step(&series(|i| 2000 + (i as u32 / 10) * 10, 60), 30).is_none());
    }

    fn mem(avail_gb: u64, commit_pct: u64) -> MemInfo {
        MemInfo { total: 63 << 30, avail: avail_gb << 30, commit: commit_pct * (100 << 30) / 100, commit_limit: 100 << 30 }
    }

    #[test]
    fn tracker_alerts_once_then_stays_quiet() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let mut fired = 0;
        for i in 0..120u64 {
            let s = [Sample { pid: 7, create: 1, name: "leaky.exe".into(), commit: (500 + (i / 10) * 50) << 20 }];
            for a in t.feed(&s, mem(30, 40), t0 + Duration::from_secs(i * 30)) {
                assert!(matches!(a, Alert::Leak(_)));
                fired += 1;
            }
        }
        assert_eq!(fired, 1);
        assert_eq!(t.suspects.len(), 1);
    }

    #[test]
    fn pressure_needs_two_feeds_and_cools_down() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let big = [Sample { pid: 1, create: 1, name: "a.exe".into(), commit: 15 << 30 }, Sample { pid: 2, create: 1, name: "b.exe".into(), commit: 8 << 30 }];
        assert!(t.feed(&big, mem(3, 93), now).is_empty());
        let a = t.feed(&big, mem(3, 93), now + Duration::from_secs(30));
        assert!(matches!(&a[..], [Alert::Pressure { top, .. }] if top[0].0 == "a.exe"));
        assert!(t.feed(&big, mem(3, 93), now + Duration::from_secs(60)).is_empty());
    }
}
