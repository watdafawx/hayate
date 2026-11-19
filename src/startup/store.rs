//! Durable storage of the managed list (`startup.ini` in Hayate's data dir).
//!
//! Disk work is serialized behind an epoch gate (a slow older write can never
//! replace a newer acknowledged one) and a file Hayate cannot read or does not
//! understand is NEVER overwritten.
//!
//! There is ONE way to change the list: [`Store::update_sync`]. It writes in the
//! calling thread (durably, see `osutil::write_durable`), reads the file back,
//! and only returns `Ok` once the file on disk IS the new list; on any failure
//! the in-memory list is put back, so the screen never shows a change that is
//! not saved. Multi-step operations that also touch the registry (adoption,
//! release) additionally hold [`Store::ops`], so a worker the watchdog gave up on
//! and its replacement can never interleave their steps.
//!
//! A list with entries Hayate could not read (`Snapshot::dropped`) is never
//! rewritten silently: the first save keeps the old file as `startup.ini.bak`
//! and the UI shows a warning while any entry is dropped.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::model::{self, Config};


#[derive(Default)]
struct Writes {
    epoch: AtomicU64,
    gate: Mutex<()>,
}

impl Writes {
    fn next(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn current(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn write(&self, epoch: u64, file: &std::path::Path, data: &[u8], backup_first: bool) -> Result<(), String> {
        let _g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.current() != epoch {
            return Err("superseded by a newer save".into());
        }
        if let Some(d) = file.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        if backup_first && file.is_file() {
            // entries that could not be read are not in `data`: keep the old file next to it
            let old = crate::osutil::read_bounded(file, model::MAX_FILE_BYTES).map_err(|e| format!("backup: {e}"))?;
            crate::osutil::write_durable(&file.with_extension("ini.bak"), &old).map_err(|e| format!("backup: {e}"))?;
        }
        // temp write + FlushFileBuffers + write-through replace, then read back:
        // `Ok` means the file on disk IS `data` (adoption relies on this before it
        // touches the registry)
        crate::osutil::write_durable(file, data).map_err(|e| format!("could not write the startup list: {e}"))?;
        match std::fs::read(file) {
            Ok(b) if b == data => Ok(()),
            Ok(_) => Err("the startup list on disk is not what was written".into()),
            Err(e) => Err(format!("the startup list could not be read back: {e}")),
        }
    }
}

struct Inner {
    cfg: Config,
    loaded: bool,
    locked: Option<String>,
    dropped: Vec<String>,
    dropped_owned: usize,
    backed_up: bool,
}

#[derive(Clone)]
pub struct Snapshot {
    pub cfg: Config,
    pub loaded: bool,
    pub locked: Option<String>,
    pub dropped: Vec<String>,
    /// dropped sections that held an ownership record
    pub dropped_owned: usize,
}

pub struct Store {
    file: PathBuf,
    inner: Mutex<Inner>,
    writes: Arc<Writes>,
    /// held for a whole registry + ownership-record operation, so two of them
    /// (e.g. one on a worker the watchdog abandoned and one on its replacement)
    /// never interleave their steps
    ops: Mutex<()>,
}

impl Store {
    pub fn new(file: PathBuf) -> Store {
        Store { file, inner: Mutex::new(Inner { cfg: Config::default(), loaded: false, locked: None, dropped: Vec::new(), dropped_owned: 0, backed_up: false }), writes: Arc::new(Writes::default()), ops: Mutex::new(()) }
    }

    /// Read the file (blocking; call from the I/O worker). A missing file is an
    /// empty list; anything unreadable locks the store so nothing overwrites it.
    pub fn load(&self) {
        let r = crate::osutil::read_bounded(&self.file, model::MAX_FILE_BYTES);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match r {
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => g.locked = Some("startup list file is too large to be one Hayate wrote".into()),
            Ok(b) => match String::from_utf8(b) {
                Ok(t) => {
                    let l = model::parse(&t);
                    g.cfg = l.cfg;
                    g.dropped = l.dropped;
                    g.dropped_owned = l.dropped_owned;
                    g.locked = l.locked;
                }
                Err(_) => g.locked = Some("startup list file is not valid text".into()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => g.cfg = Config::default(),
            Err(e) => g.locked = Some(format!("cannot read the startup list: {e}")),
        }
        g.loaded = true;
    }

    /// Serialise one whole registry + record operation (see the `ops` field).
    pub fn ops(&self) -> std::sync::MutexGuard<'_, ()> {
        self.ops.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Snapshot {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Snapshot { cfg: g.cfg.clone(), loaded: g.loaded, locked: g.locked.clone(), dropped: g.dropped.clone(), dropped_owned: g.dropped_owned }
    }

    fn prepare(&self, f: impl FnOnce(&mut Config)) -> Result<(Config, Config, u64, Vec<u8>, bool), Vec<String>> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !g.loaded {
            return Err(vec!["the startup list is still loading".into()]);
        }
        if let Some(l) = &g.locked {
            return Err(vec![format!("not saved: {l}")]);
        }
        let before = g.cfg.clone();
        let mut next = before.clone();
        f(&mut next);
        let problems = model::validate(&next);
        if !problems.is_empty() {
            return Err(problems);
        }
        let epoch = self.writes.next();
        let data = model::write(&next).into_bytes();
        let backup = !g.dropped.is_empty() && !g.backed_up;
        g.cfg = next.clone();
        Ok((before, next, epoch, data, backup))
    }

    /// Apply `f` and return only once the file is replaced. On failure the list
    /// is put back as it was (unless a newer save already superseded it).
    pub fn update_sync(&self, f: impl FnOnce(&mut Config)) -> Result<(), String> {
        let (before, _next, epoch, data, backup) = self.prepare(f).map_err(|p| p.join("; "))?;
        let r = self.writes.write(epoch, &self.file, &data, backup);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match &r {
            Ok(()) => {
                if backup {
                    g.backed_up = true;
                }
            }
            Err(_) => {
                if self.writes.current() == epoch {
                    g.cfg = before;
                }
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::model::App;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("HayateStartupFixture-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn add(c: &mut Config, name: &str) {
        let id = c.fresh_id();
        c.apps.push(App::new(id, name.into(), format!(r"C:\nope\{name}.exe")));
    }

    #[test]
    fn nothing_is_written_before_the_file_was_read() {
        let d = tmp("unloaded");
        let s = Arc::new(Store::new(d.join("startup.ini")));
        assert!(s.update_sync(|c| add(c, "A")).is_err());
        assert!(!d.join("startup.ini").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saved_only_after_the_write_and_reloads_identically() {
        let d = tmp("roundtrip");
        let s = Arc::new(Store::new(d.join("startup.ini")));
        s.load();
        assert!(!s.snapshot().cfg.manage, "default off");
        s.update_sync(|c| {
            add(c, "A");
            c.manage = true;
        })
        .unwrap();
        let s2 = Store::new(d.join("startup.ini"));
        s2.load();
        assert_eq!(s2.snapshot().cfg, s.snapshot().cfg);
        assert!(s2.snapshot().cfg.manage);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn invalid_edits_are_refused_and_change_nothing() {
        let d = tmp("invalid");
        let s = Arc::new(Store::new(d.join("startup.ini")));
        s.load();
        s.update_sync(|c| add(c, "A")).unwrap();
        let before = s.snapshot().cfg;
        let e = s.update_sync(|c| c.apps[0].after = Some("zz".into())).unwrap_err();
        assert!(e.contains("no longer exists"), "{e:?}");
        assert_eq!(s.snapshot().cfg, before);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failed_write_is_reported_and_a_sync_update_rolls_back() {
        let d = tmp("fail");
        let file = d.join("startup.ini");
        let s = Arc::new(Store::new(file.clone()));
        s.load();
        std::fs::create_dir_all(&file).unwrap(); // the target can no longer be replaced
        assert!(s.update_sync(|c| add(c, "A")).is_err(), "a failed write must not look saved");
        assert!(s.snapshot().cfg.apps.is_empty(), "and the list in memory is put back to what is on disk");
        let s2 = Store::new(file.clone());
        s2.load();
        assert!(s2.snapshot().locked.is_some(), "a file that cannot be read is never treated as empty");

        let s3 = Store::new(d.join("other").join("startup.ini"));
        s3.load();
        let _ = std::fs::write(d.join("other"), b"a file, not a folder"); // parent is a file: create_dir_all fails
        let e = s3.update_sync(|c| add(c, "B"));
        assert!(e.is_err());
        assert!(s3.snapshot().cfg.apps.is_empty(), "adoption records are rolled back when the write failed");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_locked_store_never_overwrites_the_file() {
        let d = tmp("locked");
        let file = d.join("startup.ini");
        std::fs::write(&file, "hayate-startup 7\nmanage=1\n").unwrap();
        let s = Arc::new(Store::new(file.clone()));
        s.load();
        assert!(s.snapshot().locked.is_some());
        assert!(s.update_sync(|c| add(c, "A")).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hayate-startup 7\nmanage=1\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dropped_entries_are_kept_in_a_backup_before_the_first_rewrite() {
        let d = tmp("backup");
        let file = d.join("startup.ini");
        let original = "hayate-startup 1\nmanage=0\n\n[app]\nid=a1\nname=ok\ncommand=C:\\a.exe\n\n[app]\nid=a2\nname=bad\ncommand=x.exe\ncpu_below=250\n";
        std::fs::write(&file, original).unwrap();
        let s = Arc::new(Store::new(file.clone()));
        s.load();
        assert_eq!(s.snapshot().dropped.len(), 1);
        s.update_sync(|c| c.apps[0].delay_ms = 1000).unwrap();
        assert_eq!(std::fs::read_to_string(file.with_extension("ini.bak")).unwrap(), original);
        assert!(!std::fs::read_to_string(&file).unwrap().contains("bad"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_older_write_cannot_replace_a_newer_one() {
        let d = tmp("epoch");
        let w = Writes::default();
        let (e1, e2) = (w.next(), w.next());
        let f = d.join("x.ini");
        assert!(w.write(e1, &f, b"old", false).is_err());
        assert!(w.write(e2, &f, b"new", false).is_ok());
        assert_eq!(std::fs::read(&f).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&d);
    }
}
