//! New / updated app detection. "Seen" table: normalized path → (size,
//! mtime, version, signature). Persisted as `seen.tsv` (append, last line
//! wins, compacted on load). Dev-build noise is filtered by editable globs.
//!
//! The first-run seed of running images stores signature "unknown" (not
//! verified: that cost seconds of CPU). New/updated detection only compares
//! (size, mtime); a verdict is filled in later when an event needs it
//! (`learn_sig`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use super::sig::Sig;
use super::store::SigState;
use crate::rules::glob;

pub const DEFAULT_IGNORE: &[&str] = &["*\\target\\debug\\*", "*\\target\\release\\*", "*\\target\\*\\build\\*", "*\\target\\*\\deps\\*", "%TEMP%\\*"];

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Seen {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
    pub version: String,
    pub sig: Sig,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    New,
    /// old version, new version (may be equal if only size/mtime changed)
    Updated(String, String),
}

/// Expand `%VAR%` in a glob (e.g. `%TEMP%\*`).
pub fn expand(g: &str) -> String {
    let mut out = String::new();
    let mut rest = g;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                match std::env::var(&after[..j]) {
                    Ok(v) => out.push_str(v.trim_end_matches('\\')),
                    Err(_) => out.push_str(&rest[i..i + j + 2]),
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn ignored(path: &str, globs: &[String]) -> bool {
    globs.iter().any(|g| glob::matches(g, path))
}

/// Seen-table key: lowercase path with a trailing `-<hex hash>` on the file
/// stem collapsed (`hopper_core-1a2b3c4d5e6f7a8b.exe` → `hopper_core-#.exe`).
pub fn key(path: &str) -> String {
    let lower = path.to_lowercase();
    let (dir, file) = match lower.rfind('\\') {
        Some(i) => (&lower[..=i], &lower[i + 1..]),
        None => ("", &lower[..]),
    };
    let (stem, ext) = match file.rfind('.') {
        Some(i) => (&file[..i], &file[i..]),
        None => (file, ""),
    };
    if let Some(i) = stem.rfind('-') {
        let h = &stem[i + 1..];
        if h.len() >= 8 && h.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("{}{}-#{}", dir, &stem[..i], ext);
        }
    }
    lower
}

fn esc(s: &str) -> String {
    s.replace('\t', " ").replace(['\n', '\r'], " ")
}

fn encode(s: &Seen) -> String {
    format!("{}\t{}\t{}\t{}\t{}\t{}\n", esc(&s.path), s.size, s.mtime, esc(&s.version), s.sig.state.key(), esc(&s.sig.publisher))
}

fn decode(l: &str) -> Option<Seen> {
    let f: Vec<&str> = l.split('\t').collect();
    if f.len() < 6 {
        return None;
    }
    Some(Seen {
        path: f[0].to_string(),
        size: f[1].parse().ok()?,
        mtime: f[2].parse().ok()?,
        version: f[3].to_string(),
        sig: Sig { state: SigState::parse(f[4]), publisher: f[5].trim_end().to_string() },
    })
}

pub struct Apps {
    file: PathBuf,
    map: HashMap<String, Seen>,
    /// path key → last stat time (ms); skip re-stat within 30 s (build storms)
    recent: HashMap<String, u64>,
    pub first_run: bool,
}

impl Apps {
    pub fn open(file: PathBuf) -> Apps {
        let mut map = HashMap::new();
        let mut lines = 0usize;
        let exists = file.exists();
        if let Ok(f) = std::fs::File::open(&file) {
            for l in BufReader::new(f).lines().map_while(Result::ok) {
                lines += 1;
                if let Some(s) = decode(&l) {
                    map.insert(key(&s.path), s);
                }
            }
        }
        let a = Apps { file, map, recent: HashMap::new(), first_run: !exists };
        if lines > a.map.len() * 2 + 100 {
            a.compact();
        }
        a
    }

    fn compact(&self) {
        let mut out = String::new();
        for s in self.map.values() {
            out.push_str(&encode(s));
        }
        let _ = crate::osutil::write_atomic(&self.file, out.as_bytes());
    }

    fn append(&self, s: &Seen) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.file) {
            let _ = f.write_all(encode(s).as_bytes());
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = &Seen> {
        self.map.values()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Cheap pre-check: is this path worth a stat? (unseen, or not stat'ed in 30 s)
    pub fn should_check(&mut self, path: &str, now_ms: u64) -> bool {
        let k = key(path);
        match self.recent.get(&k) {
            Some(&t) if now_ms.saturating_sub(t) < 30_000 => false,
            _ => {
                if self.recent.len() > 4096 {
                    self.recent.retain(|_, t| now_ms.saturating_sub(*t) < 30_000);
                }
                self.recent.insert(k, now_ms);
                true
            }
        }
    }

    /// Compare a launch with the table. `stat` = (size, mtime) of the file now.
    /// Returns the change (if any) without touching the table.
    pub fn compare(&self, path: &str, stat: (u64, u64)) -> Option<Change> {
        match self.map.get(&key(path)) {
            None => Some(Change::New),
            Some(s) if (s.size, s.mtime) != stat => Some(Change::Updated(s.version.clone(), String::new())),
            Some(_) => None,
        }
    }

    pub fn get(&self, path: &str) -> Option<&Seen> {
        self.map.get(&key(path))
    }

    /// Record (and persist) the current state of a path.
    pub fn record(&mut self, s: Seen) {
        self.append(&s);
        self.map.insert(key(&s.path), s);
    }

    /// Record many in one append (first-run seed).
    pub fn record_all(&mut self, v: Vec<Seen>) {
        if v.is_empty() {
            return;
        }
        let out: String = v.iter().map(encode).collect();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.file) {
            let _ = f.write_all(out.as_bytes());
        }
        for s in v {
            self.map.insert(key(&s.path), s);
        }
    }

    /// Is `path` a seeded entry still waiting for a signature verdict?
    pub fn wants_sig(&self, path: &str) -> bool {
        self.map.get(&key(path)).is_some_and(|e| e.sig.state == SigState::Unknown && e.path.eq_ignore_ascii_case(path))
    }

    /// Store a verdict for an entry seeded without one, if the file is still
    /// the one recorded (`stat` = its size, mtime now). Returns true if stored.
    pub fn learn_sig(&mut self, path: &str, stat: (u64, u64), sig: &Sig) -> bool {
        if sig.state == SigState::Unknown || !self.wants_sig(path) {
            return false;
        }
        let Some(e) = self.map.get_mut(&key(path)) else { return false };
        if (e.size, e.mtime) != stat {
            return false; // changed since: on_proc reports it as an update
        }
        e.sig = sig.clone();
        let e = e.clone();
        self.append(&e);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_collapse_hash() {
        assert_eq!(key("X:\\Dev\\h\\target\\debug\\deps\\hopper_core-1a2b3c4d5e6f7a8b.exe"), "x:\\dev\\h\\target\\debug\\deps\\hopper_core-#.exe");
        assert_eq!(key("C:\\Apps\\my-tool.exe"), "c:\\apps\\my-tool.exe");
        assert_eq!(key("C:\\Apps\\Zen.exe"), "c:\\apps\\zen.exe");
    }

    #[test]
    fn ignore_defaults() {
        std::env::set_var("HAYATE_TEST_TMP", "C:\\Users\\me\\AppData\\Local\\Temp\\");
        let g: Vec<String> = DEFAULT_IGNORE.iter().map(|s| expand(s)).chain(std::iter::once(expand("%HAYATE_TEST_TMP%\\*"))).collect();
        assert!(ignored("X:\\Dev\\hayate\\target\\debug\\hayate.exe", &g));
        assert!(ignored("X:\\Dev\\h\\target\\release\\build\\x-123\\build-script-build.exe", &g));
        assert!(ignored("X:\\Dev\\h\\target\\x86_64-pc-windows-msvc\\deps\\sync_bench-0123abcd.exe", &g));
        assert!(ignored("C:\\Users\\me\\AppData\\Local\\Temp\\setup.exe", &g));
        assert!(!ignored("C:\\Program Files\\Zen\\zen.exe", &g));
        assert_eq!(expand("%NOPE_NOT_SET%\\*"), "%NOPE_NOT_SET%\\*");
    }

    #[test]
    fn seen_roundtrip_and_changes() {
        let d = std::env::temp_dir().join(format!("hayate-apps-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let f = d.join("seen.tsv");
        let _ = std::fs::remove_file(&f);
        let mut a = Apps::open(f.clone());
        assert!(a.first_run);
        let p = "C:\\Program Files\\Zen\\zen.exe";
        assert_eq!(a.compare(p, (10, 20)), Some(Change::New));
        a.record(Seen { path: p.into(), size: 10, mtime: 20, version: "1.0".into(), sig: Sig { state: SigState::Signed, publisher: "Zen".into() } });
        assert_eq!(a.compare(p, (10, 20)), None);
        assert_eq!(a.compare(p, (11, 21)), Some(Change::Updated("1.0".into(), String::new())));
        a.record(Seen { path: p.into(), size: 11, mtime: 21, version: "1.1".into(), sig: Sig::default() });
        let b = Apps::open(f);
        assert!(!b.first_run);
        assert_eq!(b.len(), 1);
        assert_eq!(b.get(p).unwrap().version, "1.1");
        let mut b = b;
        assert!(b.should_check(p, 1000));
        assert!(!b.should_check(p, 2000));
        assert!(b.should_check(p, 40_000));
    }

    #[test]
    fn seeded_unknown_then_learned() {
        let d = std::env::temp_dir().join(format!("hayate-apps-seed-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let f = d.join("seen.tsv");
        let _ = std::fs::remove_file(&f);
        let mut a = Apps::open(f.clone());
        let p = r"C:\Tools\Thing.exe";
        let q = r"C:\Tools\other.exe";
        a.record_all(vec![
            Seen { path: p.into(), size: 10, mtime: 20, version: "2.0".into(), sig: Sig::default() },
            Seen { path: q.into(), size: 1, mtime: 2, ..Default::default() },
        ]);
        // seeded: same file is not a change, a changed one is an update with the old version
        assert_eq!(a.compare(p, (10, 20)), None);
        assert_eq!(a.compare(p, (11, 20)), Some(Change::Updated("2.0".into(), String::new())));
        assert!(a.wants_sig(p) && a.wants_sig(r"c:\tools\thing.exe"));
        let signed = Sig { state: SigState::Signed, publisher: "Thing Co".into() };
        assert!(!a.learn_sig(p, (10, 20), &Sig::default()), "unknown is not a verdict");
        assert!(!a.learn_sig(p, (11, 21), &signed), "file changed since the seed");
        assert!(a.learn_sig(p, (10, 20), &signed));
        assert!(!a.wants_sig(p));
        assert!(!a.learn_sig(p, (10, 20), &Sig { state: SigState::Unsigned, publisher: String::new() }), "only fills unknown");
        let b = Apps::open(f);
        assert_eq!(b.len(), 2);
        assert_eq!(b.get(p).unwrap().sig, signed);
        assert_eq!(b.get(p).unwrap().version, "2.0");
        assert!(b.wants_sig(q));
    }
}
