//! Per-process details that don't change over a process's life (path, command
//! line, description, publisher, bitness). Fetched once per (pid, create time).

use super::nt;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::SystemInformation::IMAGE_FILE_MACHINE;
use windows::Win32::System::Threading::*;

#[derive(Default, Clone)]
pub struct Meta {
    pub path: Option<Arc<str>>,
    pub cmd: Option<Arc<str>>,
    pub desc: Option<Arc<str>>,
    pub publisher: Option<Arc<str>>,
    pub wow64: bool,
    /// what a runtime process (java, node, bun, python, dotnet) is running, read from `cmd` only
    pub workload: Option<Arc<super::workload::Workload>>,
}

#[derive(Default, Clone)]
struct FileInfo {
    desc: Option<Arc<str>>,
    publisher: Option<Arc<str>>,
}

type Files = Arc<Mutex<HashMap<Arc<str>, FileInfo>>>;
type Batch = Vec<((u32, i64), Arc<Meta>)>;

/// Process details, resolved on the background I/O worker: opening a process
/// can page its memory in and the version resource is a read of the exe, so
/// on a busy, sleeping or network disk either can take seconds. The sampler
/// never waits: a new process shows its name at once and its details when
/// they arrive (usually the next tick).
pub struct MetaCache {
    procs: HashMap<(u32, i64), Arc<Meta>>,
    /// exe path -> version info; only the worker reads and fills it
    files: Files,
    /// keys asked for and not answered yet (dedup), with when they were asked
    pending: HashMap<(u32, i64), Instant>,
    tx: Sender<Batch>,
    rx: Receiver<Batch>,
    empty: Arc<Meta>,
    /// when the batch on the worker started (one at a time: a slow disk then
    /// holds one worker, not one per tick of new processes)
    running: Arc<Mutex<Option<Instant>>>,
}

/// Clears the "batch running" mark when the batch ends (even by panic), unless
/// a newer batch took over meanwhile.
struct Done(Arc<Mutex<Option<Instant>>>, Instant);

impl Drop for Done {
    fn drop(&mut self) {
        let mut r = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if *r == Some(self.1) {
            *r = None;
        }
    }
}

/// Results go back in chunks of this many, so a big first batch fills in progressively.
const CHUNK: usize = 64;
/// A pending lookup older than this no longer counts as "busy" (hung worker).
const PENDING_FOR: Duration = Duration::from_secs(10);

impl Default for MetaCache {
    fn default() -> Self {
        let (tx, rx) = channel();
        MetaCache { procs: HashMap::new(), files: Files::default(), pending: HashMap::new(), tx, rx, empty: Arc::default(), running: Arc::default() }
    }
}

impl MetaCache {
    /// Cached details, or `None` when they still have to be fetched.
    pub fn lookup(&self, pid: u32, create: i64) -> Option<Arc<Meta>> {
        self.procs.get(&(pid, create)).cloned()
    }

    /// Shared empty details for processes whose lookup is still running.
    pub fn placeholder(&self) -> Arc<Meta> {
        self.empty.clone()
    }

    /// True while recent lookups are outstanding.
    pub fn busy(&self) -> bool {
        let now = Instant::now();
        self.pending.values().any(|t| now.duration_since(*t) < PENDING_FOR)
    }

    /// Queue the keys that are neither cached nor already asked for.
    pub fn fetch(&mut self, keys: Vec<(u32, i64)>) {
        let now = Instant::now();
        let keys: Vec<_> = keys.into_iter().filter(|k| !self.procs.contains_key(k) && !self.pending.contains_key(k)).collect();
        if keys.is_empty() {
            return;
        }
        {
            // one batch at a time; the keys are asked again next tick. A batch
            // stuck past PENDING_FOR (dead disk) no longer holds the others up.
            let mut r = self.running.lock().unwrap_or_else(|e| e.into_inner());
            if r.is_some_and(|t| now.duration_since(t) < PENDING_FOR) {
                return;
            }
            *r = Some(now);
        }
        for k in &keys {
            self.pending.insert(*k, now);
        }
        let (tx, files, running) = (self.tx.clone(), self.files.clone(), self.running.clone());
        crate::bgio::submit("process details", move || {
            let t0 = Instant::now();
            crate::bgio::fake_stall("disk");
            let _done = Done(running, now);
            let n = keys.len();
            for chunk in keys.chunks(CHUNK) {
                let out: Batch = chunk.iter().map(|&(pid, c)| ((pid, c), Arc::new(resolve(pid, &files)))).collect();
                if tx.send(out).is_err() {
                    return;
                }
            }
            if n > 16 {
                crate::timing::mark(&format!("process details batch ({n} procs, {:.1} ms)", t0.elapsed().as_secs_f32() * 1000.0));
            }
        });
    }

    /// Merge what the worker has finished. True when new details arrived.
    pub fn poll(&mut self) -> bool {
        let mut any = false;
        while let Ok(b) = self.rx.try_recv() {
            for (k, m) in b {
                self.pending.remove(&k);
                self.procs.insert(k, m);
                any = true;
            }
        }
        any
    }

    /// Drops entries for processes that no longer exist.
    pub fn retain(&mut self, alive: &std::collections::HashSet<(u32, i64)>) {
        self.procs.retain(|k, _| alive.contains(k));
        let now = Instant::now();
        // a dead process's answer may still come (it is dropped next time); forget
        // its request once it is clearly not coming
        self.pending.retain(|k, t| alive.contains(k) || now.duration_since(*t) < Duration::from_secs(60));
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Open the process once for path, command line and bitness; description and
/// publisher come from the exe's version resource (cached per path).
fn resolve(pid: u32, files: &Files) -> Meta {
    let mut m = Meta::default();
    if pid > 4 {
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                let mut buf = [0u16; 1024];
                let mut len = buf.len() as u32;
                if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
                    m.path = Some(Arc::from(String::from_utf16_lossy(&buf[..len as usize])));
                }
                m.cmd = nt::command_line(h.0).map(Arc::from);
                let mut pm = IMAGE_FILE_MACHINE::default();
                let mut nm = IMAGE_FILE_MACHINE::default();
                if IsWow64Process2(h, &mut pm, Some(&mut nm)).is_ok() {
                    m.wow64 = pm.0 != 0;
                }
                let _ = CloseHandle(h);
            }
        }
    }
    // text only: parsed here, on the worker, never on the UI thread
    if let (Some(p), Some(c)) = (&m.path, &m.cmd) {
        m.workload = super::workload::infer(p, c).map(Arc::new);
    }
    if let Some(p) = &m.path {
        // never hold the cache lock across the file read
        let cached = files.lock().unwrap_or_else(|e| e.into_inner()).get(p).cloned();
        let fi = cached.unwrap_or_else(|| {
            let fi = file_info(p);
            files.lock().unwrap_or_else(|e| e.into_inner()).insert(p.clone(), fi.clone());
            fi
        });
        m.desc = fi.desc;
        m.publisher = fi.publisher;
    }
    m
}

/// CompanyName from an image's version resource (a file read: worker threads only).
pub fn company_name(path: &str) -> Option<Arc<str>> {
    file_info(path).publisher
}

fn file_info(path: &str) -> FileInfo {
    let wpath: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wpath.as_ptr()), None);
        if size == 0 {
            return FileInfo::default();
        }
        let mut data = vec![0u8; size as usize];
        if GetFileVersionInfoW(PCWSTR(wpath.as_ptr()), None, size, data.as_mut_ptr() as *mut _).is_err() {
            return FileInfo::default();
        }
        let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let mut langs: Vec<(u16, u16)> = Vec::new();
        if VerQueryValueW(data.as_ptr() as *const _, w!("\\VarFileInfo\\Translation"), &mut p, &mut len).as_bool() && len >= 4 {
            let t = std::slice::from_raw_parts(p as *const u16, len as usize / 2);
            for c in t.chunks(2) {
                if c.len() == 2 {
                    langs.push((c[0], c[1]));
                }
            }
        }
        langs.push((0x0409, 0x04b0));
        langs.push((0x0409, 0x04e4));
        let q = |key: &str| -> Option<Arc<str>> {
            for (l, c) in &langs {
                let sub = format!("\\StringFileInfo\\{:04x}{:04x}\\{}\0", l, c, key);
                let wsub: Vec<u16> = sub.encode_utf16().collect();
                let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
                let mut len = 0u32;
                if VerQueryValueW(data.as_ptr() as *const _, PCWSTR(wsub.as_ptr()), &mut p, &mut len).as_bool() && len > 1 {
                    let s = std::slice::from_raw_parts(p as *const u16, len as usize);
                    let s = String::from_utf16_lossy(s);
                    let s = s.trim_end_matches('\0').trim();
                    if !s.is_empty() {
                        return Some(Arc::from(s));
                    }
                }
            }
            None
        };
        FileInfo { desc: q("FileDescription"), publisher: q("CompanyName") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_poll(m: &mut MetaCache) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            if m.poll() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn fetch_dedups_and_fills_in_from_the_worker() {
        let me = (std::process::id(), 1i64);
        let mut m = MetaCache::default();
        assert!(m.lookup(me.0, me.1).is_none());
        m.fetch(vec![me, me]);
        assert_eq!(m.pending_len(), 1);
        // asked again while pending: no second job
        m.fetch(vec![me]);
        assert_eq!(m.pending_len(), 1);
        assert!(m.busy());
        assert!(wait_poll(&mut m));
        let got = m.lookup(me.0, me.1).expect("resolved");
        assert!(got.path.as_deref().is_some_and(|p| p.to_ascii_lowercase().ends_with(".exe")));
        assert!(!m.busy());
        // cached: fetching again queues nothing
        m.fetch(vec![me]);
        assert_eq!(m.pending_len(), 0);
    }
}
