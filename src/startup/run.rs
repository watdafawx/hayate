//! Running the managed list: when it may start by itself, the once-per-logon
//! guarantee, the background runner and the state the UI shows.
//!
//! The list runs at logon ONLY when every one of these holds (see [`decide`]):
//!   * the user turned the startup manager on (default off) AND pressed Save,
//!     which added the private flag [`LOGON_FLAG`] to Hayate's own Start with
//!     Windows shortcut (`own.rs`, with a rollback if that fails);
//!   * this process was started with `--resident --startup-logon`, and that
//!     shortcut exists, is enabled in Windows (StartupApproved) and carries the
//!     flag. A manual `--resident`, an update, an elevation restart, a taskbar
//!     task or a hand-typed command lacks the flag; an update or restart strips
//!     it (`update::logic::PRIVATE_FLAGS`);
//!   * Hayate is provably not elevated (an unreadable token counts as elevated);
//!   * Windows says this logon happened in the last 10 minutes;
//!   * the list is valid and has something to run;
//!   * this logon has not already run it: a claim file keyed by the logon's
//!     identity (SID, logon-session LUID, session, logon time), created
//!     atomically and flushed before anything starts.
//! The flag is not a secret (any process of the user can type it); the claim
//! limits a typed flag to once per logon and the rules above still apply.
//! Entries marked Admin never start from the logon run: they are reported as
//! needing "Run now". Opening the Startup tab, restarting Hayate, restarting as
//! admin and updating Hayate never start anything.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::approved::{self, Locations};
use super::launch;
use super::model::{validate, Config};
use super::sched::{self, Env, Event, Item, Outcome, Sched};
use super::store::Store;

/// A run started by the logon rules must follow the logon within this long.
pub const FRESH_LOGON_SECS: u64 = 600;
const MAX_LOG: usize = 300;

// ---- when may it start by itself ---------------------------------------------------

/// The private argument Hayate's OWN Start with Windows shortcut carries once the
/// user turned the startup manager on and pressed Save (see [`super::own`]).
/// It is not a secret (any process of the user can type it): it only stops
/// ordinary starts (manual `--resident`, updates, restarts) from running the
/// list. What bounds a typed flag is the once-per-logon claim and the other
/// rules below.
pub const LOGON_FLAG: &str = "--startup-logon";

#[derive(Clone, Debug)]
pub struct LogonCtx {
    pub manage: bool,
    /// the process arguments are `--resident --startup-logon` (either order)
    pub autostart_args: bool,
    /// Hayate's own Startup shortcut exists, is enabled in Windows and carries the flag
    pub autorun_ok: bool,
    /// None = the token could not be read: fail closed
    pub elevated: Option<bool>,
    pub logon_age_secs: Option<u64>,
    pub list_valid: bool,
    pub enabled_items: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Run,
    Skip(&'static str),
}

pub fn decide(c: &LogonCtx) -> Decision {
    if !c.manage {
        return Decision::Skip("the startup manager is off");
    }
    match c.elevated {
        None => return Decision::Skip("Hayate could not tell whether it is running as administrator"),
        Some(true) => return Decision::Skip("Hayate is running as administrator"),
        Some(false) => {}
    }
    if !c.autostart_args {
        return Decision::Skip("Hayate was not started by its Start with Windows shortcut for the startup manager (update, restart or manual start)");
    }
    if !c.autorun_ok {
        return Decision::Skip("Hayate's own Start with Windows shortcut is missing, disabled in Windows, or was not saved for the startup manager");
    }
    match c.logon_age_secs {
        None => return Decision::Skip("Windows did not report when this logon started"),
        Some(a) if a > FRESH_LOGON_SECS => return Decision::Skip("Hayate started long after this logon"),
        Some(_) => {}
    }
    if !c.list_valid {
        return Decision::Skip("the startup list has problems or could not be read");
    }
    if c.enabled_items == 0 {
        return Decision::Skip("no entry is enabled");
    }
    Decision::Run
}

/// `hayate.exe --resident --startup-logon` (either order, nothing else needed):
/// what Hayate's own shortcut starts once the startup manager is saved on.
pub fn is_autostart_args(args: &[String]) -> bool {
    let rest = args.get(1..).unwrap_or(&[]);
    rest.iter().any(|a| a == "--resident") && rest.iter().any(|a| a == LOGON_FLAG)
}

// ---- once per logon ----------------------------------------------------------------

/// This logon as Windows reports it.
pub struct Logon {
    pub session: u32,
    pub filetime: i64,
}

/// This logon session as Windows reports it (None when it cannot be read).
pub fn logon_now() -> Option<Logon> {
    use windows::core::PWSTR;
    use windows::Win32::System::RemoteDesktop::*;
    unsafe {
        let mut p = PWSTR::null();
        let mut n = 0u32;
        WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), WTS_CURRENT_SESSION, WTSSessionInfo, &mut p, &mut n).ok()?;
        let r = if p.0.is_null() || (n as usize) < std::mem::size_of::<WTSINFOW>() {
            None
        } else {
            let i = std::ptr::read_unaligned(p.0 as *const WTSINFOW);
            Some(Logon { session: i.SessionId, filetime: i.LogonTime })
        };
        WTSFreeMemory(p.0 as *mut _);
        r
    }
}

/// Seconds since a logon FILETIME; None for a time that is missing or in the future.
pub fn logon_age_secs(logon_filetime: i64, now_filetime: i64) -> Option<u64> {
    if logon_filetime <= 0 || now_filetime < logon_filetime {
        None
    } else {
        Some(((now_filetime - logon_filetime) / 10_000_000) as u64)
    }
}

/// The identity of one logon: who (SID), which authentication (the token's
/// logon-session LUID), which session, when (the session's logon time). Two
/// different logons of the same user never share it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub sid: String,
    pub auth: u64,
    pub session: u32,
    pub logon: i64,
}

impl Ident {
    pub fn key(&self) -> String {
        format!("{}:{:x}:{}:{}", self.sid.to_lowercase(), self.auth, self.session, self.logon)
    }
}

fn token_identity() -> Option<(String, u64)> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetLengthSid, GetTokenInformation, TokenStatistics, TokenUser, TOKEN_QUERY, TOKEN_STATISTICS, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut t = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t).ok()?;
        let tok = crate::osutil::Handle(t);
        let mut n = 0u32;
        let _ = GetTokenInformation(tok.0, TokenUser, None, 0, &mut n);
        if n == 0 {
            return None;
        }
        let mut buf = vec![0u8; n as usize];
        GetTokenInformation(tok.0, TokenUser, Some(buf.as_mut_ptr() as *mut _), n, &mut n).ok()?;
        let tu = std::ptr::read_unaligned(buf.as_ptr() as *const TOKEN_USER);
        let len = GetLengthSid(tu.User.Sid) as usize;
        if len == 0 || len > 68 {
            return None;
        }
        let sid: String = std::slice::from_raw_parts(tu.User.Sid.0 as *const u8, len).iter().map(|b| format!("{b:02x}")).collect();
        let mut st = TOKEN_STATISTICS::default();
        GetTokenInformation(tok.0, TokenStatistics, Some(&mut st as *mut _ as *mut _), std::mem::size_of::<TOKEN_STATISTICS>() as u32, &mut n).ok()?;
        let luid = ((st.AuthenticationId.HighPart as u32 as u64) << 32) | st.AuthenticationId.LowPart as u64;
        Some((sid, luid))
    }
}

/// The identity of the current logon; None when any part cannot be read (then
/// nothing runs: a claim needs a strong identity).
pub fn logon_ident() -> Option<(Ident, Logon)> {
    let l = logon_now()?;
    let (sid, auth) = token_identity()?;
    Some((Ident { sid, auth, session: l.session, logon: l.filetime }, l))
}

/// What Windows says about a session another claim was made in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// the session id does not exist now
    Gone,
    /// it exists and its logon time is this
    Logon(i64),
    /// could not be determined
    Unknown,
}

pub fn session_state(session: u32) -> SessionState {
    use windows::core::PWSTR;
    use windows::Win32::System::RemoteDesktop::*;
    unsafe {
        let mut p = PWSTR::null();
        let mut n = 0u32;
        if WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), session, WTSSessionInfo, &mut p, &mut n).is_ok() {
            let r = if p.0.is_null() || (n as usize) < std::mem::size_of::<WTSINFOW>() {
                SessionState::Unknown
            } else {
                let i = std::ptr::read_unaligned(p.0 as *const WTSINFOW);
                if i.LogonTime > 0 {
                    SessionState::Logon(i.LogonTime)
                } else {
                    SessionState::Unknown
                }
            };
            WTSFreeMemory(p.0 as *mut _);
            return r;
        }
        let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        if WTSEnumerateSessionsW(Some(WTS_CURRENT_SERVER_HANDLE), 0, 1, &mut info, &mut count).is_ok() {
            let present = !info.is_null() && std::slice::from_raw_parts(info, count as usize).iter().any(|s| s.SessionId == session);
            if !info.is_null() {
                WTSFreeMemory(info as *mut _);
            }
            if !present {
                return SessionState::Gone;
            }
        }
        SessionState::Unknown
    }
}

fn fnv(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// first to ask in this logon: may run (the claim is on disk)
    Won,
    /// this logon already ran (or is running) the list
    AlreadyRan,
    Failed(String),
}

#[cfg(test)]
thread_local! {
    /// test-only: make the claim's content write / flush fail
    pub static CLAIM_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const CLAIM_HEAD: &str = "hayate-startup-claim 2";
/// Written by 0.1.0: still read, so a claim left by the old build is not taken twice.
const LEGACY_CLAIM_HEAD: &str = "blink-startup-claim 2";

/// Take this logon's one run. `create_new` is atomic: of any number of Hayate
/// processes racing for the same logon, exactly one gets `Won`; `Won` is only
/// returned after the claim's content was written and flushed (a claim that
/// could not be recorded is removed again and reported as `Failed`, never as a
/// win). Claims of OTHER logons are never removed on a guess: only when
/// Windows positively says that logon is over (see [`claim_with`]).
pub fn claim(dir: &Path, id: &Ident) -> Claim {
    claim_with(dir, id, &session_state)
}

/// `probe(session)` answers for the session of ANOTHER claim. That claim is
/// deleted only when its session no longer exists, or exists with a different
/// logon time (so it is a different logon that reuses the number). An unreadable
/// file, an unknown state, or a claim of a still-live logon (another session of
/// the same user, for instance) is left alone.
pub fn claim_with(dir: &Path, id: &Ident, probe: &dyn Fn(u32) -> SessionState) -> Claim {
    let name = format!("startup-run-{:016x}.claim", fnv(&id.key()));
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Claim::Failed(e.to_string());
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten().take(256) {
            let n = e.file_name().to_string_lossy().into_owned();
            if !(n.starts_with("startup-run-") && n.ends_with(".claim")) || n == name {
                continue;
            }
            let Some(old) = crate::osutil::read_bounded(&e.path(), 4096).ok().and_then(|b| String::from_utf8(b).ok()).and_then(|t| parse_claim(&t)) else { continue };
            let stale = match probe(old.0) {
                SessionState::Gone => true,
                SessionState::Logon(ft) => ft != old.1,
                SessionState::Unknown => false,
            };
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    use std::io::Write;
    let path = dir.join(&name);
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut f) => {
            let body = format!("{CLAIM_HEAD}\nid={}\nsession={}\nlogon={}\nclaimed={}\n", id.key(), id.session, id.logon, crate::osutil::now_unix());
            #[allow(unused_mut)]
            let mut r = f.write_all(body.as_bytes()).and_then(|_| f.sync_all());
            #[cfg(test)]
            if CLAIM_FAIL.with(|c| c.get()) {
                r = Err(std::io::Error::other("injected claim failure"));
            }
            match r {
                Ok(()) => Claim::Won,
                Err(e) => {
                    drop(f);
                    let _ = std::fs::remove_file(&path);
                    Claim::Failed(format!("could not record the claim: {e}"))
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Claim::AlreadyRan,
        Err(e) => Claim::Failed(e.to_string()),
    }
}

/// (session, logon time) of a claim file written by [`claim_with`].
fn parse_claim(t: &str) -> Option<(u32, i64)> {
    let mut l = t.lines();
    let head = l.next()?;
    if head != CLAIM_HEAD && head != LEGACY_CLAIM_HEAD {
        return None;
    }
    let (mut s, mut lg) = (None, None);
    for line in l {
        if let Some(v) = line.strip_prefix("session=") {
            s = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("logon=") {
            lg = v.parse().ok();
        }
    }
    Some((s?, lg?))
}

// ---- the machine as the scheduler sees it -------------------------------------------

/// CPU from the sampler Hayate already runs (a reading is used once: only a new
/// one counts toward the idle gate); "running" from a process snapshot in this
/// session.
pub struct LiveEnv {
    cpu: Box<dyn FnMut() -> Option<(u64, f32)> + Send>,
    last_seq: u64,
}

impl LiveEnv {
    pub fn new(cpu: Box<dyn FnMut() -> Option<(u64, f32)> + Send>) -> LiveEnv {
        LiveEnv { cpu, last_seq: 0 }
    }

    /// Wired to the real sampler (reads its published snapshot; never starts one).
    pub fn system() -> LiveEnv {
        LiveEnv::new(Box::new(|| {
            let s = crate::win::shared()?;
            let sn = s.get();
            sn.ready.then_some((sn.seq, sn.cpu))
        }))
    }
}

impl Env for LiveEnv {
    fn cpu_percent(&mut self) -> Option<f32> {
        let (seq, v) = (self.cpu)()?;
        if seq == self.last_seq {
            return None;
        }
        self.last_seq = seq;
        Some(v)
    }

    fn is_running(&mut self, name: &str) -> bool {
        process_running(name)
    }
}

/// Is a process with this exe name running in this logon session?
pub fn process_running(name_lower: &str) -> bool {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let want = name_lower.trim().to_lowercase();
    if want.is_empty() {
        return false;
    }
    unsafe {
        let mut mine = 0u32;
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut mine);
        let Ok(h) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return false };
        let h = crate::osutil::Handle(h);
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(h.0, &mut e).is_ok() {
            loop {
                if crate::osutil::from_wide(&e.szExeFile).to_lowercase() == want {
                    let mut s = u32::MAX;
                    if ProcessIdToSessionId(e.th32ProcessID, &mut s).is_ok() && s == mine {
                        return true;
                    }
                }
                if Process32NextW(h.0, &mut e).is_err() {
                    break;
                }
            }
        }
    }
    false
}

// ---- the runner ------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunRow {
    pub id: String,
    pub name: String,
    pub status: String,
    pub finished: bool,
    /// the launch worked (None until it finished)
    pub ok: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct RunView {
    pub active: bool,
    pub label: String,
    pub rows: Vec<RunRow>,
    pub log: Vec<String>,
    /// bumps on every change (the UI repaints when it moves)
    pub seq: u64,
}

enum Msg {
    Launched(usize, Result<String, String>),
    Cancel,
}

pub type Starter = Arc<dyn Fn(&str, bool) -> Result<String, String> + Send + Sync>;

/// What the launch gate reports when the run was cancelled before Windows was asked.
const GATE_CANCELLED: &str = "\u{1}cancelled before it was started";

/// A launch that was handed to Windows and has not come back yet.
struct Flight {
    run: u64,
    idx: usize,
    name: String,
}

#[derive(Default)]
pub struct RunShared {
    view: Mutex<RunView>,
    tx: Mutex<Option<mpsc::Sender<Msg>>>,
    active: AtomicBool,
    /// number of the current/last run: a late answer only touches its own run's rows
    run_no: AtomicU64,
    /// Launches in flight. This is the reservation that OUTLIVES a run: the
    /// scheduler gives up on a launch after 120 s (Indeterminate), but a
    /// ShellExecute call that is already running cannot be recalled, so until
    /// its thread really returns no new run starts, Run now / Retry is refused,
    /// and the entry is never marked failed-and-free. The cancel flag is read and
    /// set under this lock, so "cancelled" and "handed to Windows" cannot both
    /// be true for the same launch.
    flight: Mutex<(bool, Vec<Flight>)>,
}

impl RunShared {
    pub fn view(&self) -> RunView {
        self.view.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Stop everything not yet started. Returns how many launches Windows was
    /// ALREADY asked to start: those cannot be recalled and finish on their own.
    /// A launch that has not reached the gate in its worker yet is refused there.
    pub fn cancel(&self) -> usize {
        let n = {
            let mut g = self.flight.lock().unwrap_or_else(|e| e.into_inner());
            g.0 = true;
            g.1.len()
        };
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(Msg::Cancel);
        }
        n
    }

    /// The gate right before the OS call. False = the run was cancelled: do not start.
    fn begin_launch(&self, run: u64, idx: usize, name: &str) -> bool {
        let mut g = self.flight.lock().unwrap_or_else(|e| e.into_inner());
        if g.0 {
            return false;
        }
        g.1.push(Flight { run, idx, name: name.to_string() });
        true
    }

    fn end_launch(&self, run: u64, idx: usize) {
        self.flight.lock().unwrap_or_else(|e| e.into_inner()).1.retain(|f| !(f.run == run && f.idx == idx));
    }

    /// Names of programs Windows was asked to start whose launch has not returned
    /// (possibly from an earlier run that already reported them as unconfirmed).
    pub fn in_flight(&self) -> Vec<String> {
        self.flight.lock().unwrap_or_else(|e| e.into_inner()).1.iter().map(|f| f.name.clone()).collect()
    }

    /// The launch thread returned after its run had stopped listening.
    fn late_answer(&self, run: u64, idx: usize, name: &str, r: &Result<String, String>) {
        let text = match r {
            Ok(how) => format!("{name}: it did start, after the run had stopped waiting ({how})"),
            Err(e) if e == GATE_CANCELLED => format!("{name}: cancelled before it was started"),
            Err(e) => format!("{name}: failed after the run had stopped waiting: {e}"),
        };
        crate::osutil::log(&format!("startup: {text}"));
        if self.run_no.load(Ordering::SeqCst) == run {
            let (ok, status) = (r.is_ok(), match r {
                Ok(how) => format!("Started late {how}"),
                Err(e) if e == GATE_CANCELLED => "Cancelled".to_string(),
                Err(e) => format!("Failed: {e}"),
            });
            self.edit(|v| {
                v.log.push(format!("[late] {text}"));
                if let Some(row) = v.rows.get_mut(idx) {
                    row.status = status;
                    row.ok = Some(ok);
                }
            });
        }
    }

    fn edit(&self, f: impl FnOnce(&mut RunView)) {
        let mut v = self.view.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut v);
        v.seq += 1;
    }

    fn log(&self, started: Instant, line: &str) {
        let t = started.elapsed().as_secs_f32();
        let l = format!("[+{t:5.1}s] {line}");
        crate::osutil::log(&format!("startup: {line}"));
        self.edit(|v| {
            v.log.push(l);
            let n = v.log.len();
            if n > MAX_LOG {
                v.log.drain(..n - MAX_LOG);
            }
        });
    }
}

struct Active<'a>(&'a RunShared);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        *self.0.tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.0.edit(|v| v.active = false);
        self.0.active.store(false, Ordering::SeqCst);
    }
}

/// Run `items` to the end on the calling thread. `cmds[i]` = (command, admin)
/// of item `i`. Nothing here blocks on a program: each launch runs on its own
/// short-lived thread and reports back.
pub fn run_plan(shared: &Arc<RunShared>, label: &str, items: Vec<Item>, cmds: Vec<(String, bool)>, mut env: Box<dyn Env + Send>, starter: Starter, log_file: Option<PathBuf>) -> Result<(), String> {
    if shared.active.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Err("A startup run is already in progress.".into());
    }
    let _active = Active(shared);
    {
        let mut g = shared.flight.lock().unwrap_or_else(|e| e.into_inner());
        if !g.1.is_empty() {
            return Err(format!("{} was handed to Windows by an earlier run and has not reported back yet. Wait for it before starting again.", g.1.iter().map(|f| f.name.clone()).collect::<Vec<_>>().join(", ")));
        }
        g.0 = false;
    }
    let run_no = shared.run_no.fetch_add(1, Ordering::SeqCst) + 1;
    let (tx, rx) = mpsc::channel::<Msg>();
    *shared.tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
    let started = Instant::now();
    let now = move || started.elapsed().as_millis() as u64;
    shared.edit(|v| {
        *v = RunView { active: true, label: label.to_string(), rows: items.iter().map(|i| RunRow { id: i.id.clone(), name: i.name.clone(), status: "Waiting".into(), finished: false, ok: None }).collect(), log: Vec::new(), seq: v.seq };
    });
    shared.log(started, &format!("{label}: {} entr{}", items.len(), if items.len() == 1 { "y" } else { "ies" }));
    let names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
    let mut sched = Sched::new(items, now());
    let mut hows: Vec<String> = vec![String::new(); names.len()];

    let handle = |sched: &mut Sched, evs: Vec<Event>, hows: &mut Vec<String>| {
        for e in evs {
            match e {
                Event::Launch(i) => {
                    let (cmd, admin) = cmds[i].clone();
                    let (st, tx2, sh) = (starter.clone(), tx.clone(), shared.clone());
                    let name = names[i].clone();
                    shared.log(started, &format!("{}: starting{}", names[i], if admin { " (as administrator)" } else { "" }));
                    std::thread::spawn(move || {
                        let r = if sh.begin_launch(run_no, i, &name) {
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| st(&cmd, admin))).unwrap_or_else(|_| Err("the launcher crashed".into()));
                            sh.end_launch(run_no, i);
                            r
                        } else {
                            Err(GATE_CANCELLED.to_string())
                        };
                        if let Err(mpsc::SendError(Msg::Launched(i, r))) = tx2.send(Msg::Launched(i, r)) {
                            // the run stopped listening (it ended after reporting this launch as unconfirmed)
                            sh.late_answer(run_no, i, &name, &r);
                        }
                    });
                }
                Event::Note(i, n) => shared.log(started, &format!("{}: {n}", names[i])),
                Event::Finished(i, o) => {
                    let how = std::mem::take(&mut hows[i]);
                    let text = match &o {
                        Outcome::Started if !how.is_empty() => format!("Started {how}"),
                        _ => sched::outcome_text(&o),
                    };
                    shared.log(started, &format!("{}: {text}", names[i]));
                    shared.edit(|v| {
                        if let Some(r) = v.rows.get_mut(i) {
                            r.status = text.clone();
                            r.finished = true;
                            r.ok = Some(matches!(o, Outcome::Started | Outcome::AlreadyRunning(_)));
                        }
                    });
                }
            }
        }
        for i in 0..names.len() {
            if sched.outcome(i).is_none() {
                let s = sched.status(i);
                shared.edit(|v| {
                    if let Some(r) = v.rows.get_mut(i) {
                        if r.status != s {
                            r.status = s;
                        }
                    }
                });
            }
        }
    };

    loop {
        let evs = sched.tick(now(), env.as_mut());
        handle(&mut sched, evs, &mut hows);
        if sched.finished() {
            break;
        }
        let wait = sched.next_wake().map(|w| w.saturating_sub(now())).unwrap_or(1000).clamp(5, 1000);
        match rx.recv_timeout(Duration::from_millis(wait)) {
            Ok(Msg::Launched(i, Err(e))) if e == GATE_CANCELLED => {
                let evs = sched.launch_cancelled(i, now());
                handle(&mut sched, evs, &mut hows);
            }
            Ok(Msg::Launched(i, r)) => {
                let res = match r {
                    Ok(how) => {
                        hows[i] = how;
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                let evs = sched.launched(i, res, now());
                handle(&mut sched, evs, &mut hows);
            }
            Ok(Msg::Cancel) => {
                shared.log(started, "cancelled");
                let evs = sched.cancel(now());
                handle(&mut sched, evs, &mut hows);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    shared.log(started, "done");
    if let Some(f) = log_file {
        let text = shared.view().log.join("\r\n") + "\r\n";
        crate::bgio::write_file(f, text.into_bytes());
    }
    Ok(())
}

/// Items and their (command, admin) for a run. `only` = just that entry,
/// started right away (an explicit Run now skips the delay, the dependency and
/// the idle gate, but still will not start a second copy of a program that is
/// already running).
pub fn build(cfg: &Config, only: Option<&str>, automatic: bool) -> Result<(Vec<Item>, Vec<(String, bool)>), String> {
    let process_of = |a: &super::model::App| launch::process_name(&a.command, a.process.as_deref());
    match only {
        Some(id) => {
            let problems: Vec<String> = validate(cfg);
            let a = cfg.app(id).ok_or("no such entry")?;
            if a.own.as_ref().is_some_and(|o| o.state == super::model::OwnState::Pending) {
                return Err("This entry's take-over of the Windows startup entry was never confirmed, so it is not run (the original may still start). Restart Hayate to settle it, or unmanage it.".into());
            }
            if problems.iter().any(|p| p.starts_with(&format!("{}:", a.name))) {
                return Err(format!("Fix this entry first: {}", problems.iter().find(|p| p.starts_with(&format!("{}:", a.name))).cloned().unwrap_or_default()));
            }
            let item = Item { id: a.id.clone(), name: a.name.clone(), delay_ms: 0, after: None, cpu_below: None, process: process_of(a), manual: false };
            Ok((vec![item], vec![(a.command.clone(), a.admin)]))
        }
        None => {
            let items = sched::plan(cfg, automatic, &process_of).map_err(|p| format!("The startup list has problems: {}", p.join("; ")))?;
            if items.is_empty() {
                return Err("No entry is enabled.".into());
            }
            let cmds = items.iter().map(|i| cfg.app(&i.id).map(|a| (a.command.clone(), a.admin)).unwrap_or_default()).collect();
            Ok((items, cmds))
        }
    }
}

// ---- the manager ----------------------------------------------------------------------

pub struct Manager {
    pub store: Arc<Store>,
    pub loc: Locations,
    pub run: Arc<RunShared>,
    pub dir: PathBuf,
    /// what the logon rules did at this start, in words ("" until decided)
    note: Mutex<String>,
}

static MANAGER: OnceLock<Manager> = OnceLock::new();

pub fn manager() -> Option<&'static Manager> {
    MANAGER.get()
}

/// Give every Windows startup entry Hayate switched off back, for an uninstall
/// (see `adopt::release_all`). Uses the running manager's list when there is
/// one, otherwise reads `startup.ini` itself (headless `--startup-release`).
/// `Err` = something could not be given back: the caller removes NOTHING.
pub fn release_all_owned() -> Result<super::adopt::ReleasedAll, String> {
    if let Some(m) = manager() {
        return super::adopt::release_all(&m.loc, &m.store);
    }
    let store = Store::new(crate::osutil::data_dir().join("startup.ini"));
    store.load();
    super::adopt::release_all(&Locations::system(), &store)
}

/// Create the manager and, on the I/O worker, load the list and apply the
/// logon rules. Called once from startup; reads only, and starts nothing unless
/// every rule in [`decide`] allows it.
pub fn init(args: Vec<String>) {
    let dir = crate::osutil::data_dir();
    let m = Manager { store: Arc::new(Store::new(dir.join("startup.ini"))), loc: Locations::system(), run: Arc::new(RunShared::default()), dir, note: Mutex::new(String::new()) };
    if MANAGER.set(m).is_err() {
        return;
    }
    crate::bgio::submit("startup: load list, logon rules", move || {
        let Some(m) = manager() else { return };
        m.store.load();
        for n in super::adopt::reconcile(&m.loc, &m.store) {
            crate::osutil::log(&format!("startup: {n}"));
        }
        m.at_logon(&args);
    });
}

impl Manager {
    pub fn note(&self) -> String {
        self.note.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_note(&self, s: String) {
        crate::osutil::log(&format!("startup: {s}"));
        *self.note.lock().unwrap_or_else(|e| e.into_inner()) = s;
    }

    fn at_logon(&self, args: &[String]) {
        let snap = self.store.snapshot();
        let ident = logon_ident();
        let elevated = crate::osutil::elevation();
        // the shortcut is only read by a process that is provably not elevated
        let own = if elevated == Some(false) && is_autostart_args(args) { super::own::read(&self.loc) } else { None };
        let ctx = LogonCtx {
            manage: snap.cfg.manage,
            autostart_args: is_autostart_args(args),
            autorun_ok: own.as_ref().is_some_and(|o| o.ok()),
            elevated,
            logon_age_secs: ident.as_ref().and_then(|(_, l)| logon_age_secs(l.filetime, approved::filetime_now() as i64)),
            list_valid: snap.locked.is_none() && validate(&snap.cfg).is_empty(),
            enabled_items: snap.cfg.apps.iter().filter(|a| a.enabled && !a.command.trim().is_empty() && !a.own.as_ref().is_some_and(|o| o.state == super::model::OwnState::Pending)).count(),
        };
        match decide(&ctx) {
            Decision::Skip(why) => {
                if snap.cfg.manage {
                    self.set_note(format!("Did not run the list at this start: {why}."));
                }
            }
            Decision::Run => {
                let Some((id, _)) = ident else {
                    self.set_note("Did not run the list: Windows did not report this logon's identity.".into());
                    return;
                };
                match claim(&self.dir, &id) {
                    Claim::Won => {
                        let manual = snap.cfg.apps.iter().filter(|a| a.enabled && a.admin).count();
                        self.set_note(if manual > 0 { format!("Ran the list for this logon. {manual} Admin entr{} did not start by themselves: press Run now.", if manual == 1 { "y" } else { "ies" }) } else { "Ran the list for this logon.".into() });
                        if let Err(e) = self.start_with(None, "Logon run", true) {
                            self.set_note(format!("Could not run the list: {e}"));
                        }
                    }
                    Claim::AlreadyRan => self.set_note("The list already ran for this logon; not run again.".into()),
                    Claim::Failed(e) => self.set_note(format!("Did not run the list: could not record this logon ({e}).")),
                }
            }
        }
    }

    /// Start a run in the background (returns at once). `only` = one entry.
    pub fn start(&self, only: Option<&str>, label: &str) -> Result<(), String> {
        self.start_with(only, label, false)
    }

    /// `automatic` = the logon run: Admin entries are reported, not started.
    fn start_with(&self, only: Option<&str>, label: &str, automatic: bool) -> Result<(), String> {
        let cfg = self.store.snapshot().cfg;
        let (items, cmds) = build(&cfg, only, automatic)?;
        if self.run.is_active() {
            return Err("A startup run is already in progress.".into());
        }
        let busy = self.run.in_flight();
        if !busy.is_empty() {
            return Err(format!("{} was handed to Windows and has not reported back yet. It may still start; wait for it before running again.", busy.join(", ")));
        }
        let (run, label, log) = (self.run.clone(), label.to_string(), self.dir.join("startup-last-run.log"));
        std::thread::Builder::new()
            .name("startup-run".into())
            .spawn(move || {
                let starter: Starter = Arc::new(|c, a| launch::start(c, a));
                if let Err(e) = run_plan(&run, &label, items, cmds, Box::new(LiveEnv::system()), starter, Some(log)) {
                    crate::osutil::log(&format!("startup: {e}"));
                }
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::model::App;
    use std::collections::HashSet;
    use std::sync::Mutex as M;

    fn ctx() -> LogonCtx {
        LogonCtx { manage: true, autostart_args: true, autorun_ok: true, elevated: Some(false), logon_age_secs: Some(30), list_valid: true, enabled_items: 2 }
    }

    #[test]
    fn runs_only_when_every_condition_holds() {
        assert_eq!(decide(&ctx()), Decision::Run);
        let cases: Vec<(&str, LogonCtx)> = vec![
            ("off", LogonCtx { manage: false, ..ctx() }),
            ("args", LogonCtx { autostart_args: false, ..ctx() }),
            ("own autorun missing, disabled or without the flag", LogonCtx { autorun_ok: false, ..ctx() }),
            ("elevated", LogonCtx { elevated: Some(true), ..ctx() }),
            ("elevation unknown", LogonCtx { elevated: None, ..ctx() }),
            ("no logon time", LogonCtx { logon_age_secs: None, ..ctx() }),
            ("old logon", LogonCtx { logon_age_secs: Some(FRESH_LOGON_SECS + 1), ..ctx() }),
            ("invalid", LogonCtx { list_valid: false, ..ctx() }),
            ("empty", LogonCtx { enabled_items: 0, ..ctx() }),
        ];
        for (n, c) in cases {
            assert!(matches!(decide(&c), Decision::Skip(_)), "{n} must not run");
        }
        assert_eq!(decide(&LogonCtx { logon_age_secs: Some(FRESH_LOGON_SECS), ..ctx() }), Decision::Run);
    }

    /// RED against the old rule (`--resident` + a logon in the last 10 minutes):
    /// a manual `--resident` right after sign-in, an update or an admin restart
    /// must never run the list, because it does not carry the flag.
    #[test]
    fn a_manual_resident_start_inside_the_logon_window_does_not_run() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let manual = LogonCtx { autostart_args: is_autostart_args(&s(&["hayate.exe", "--resident"])), ..ctx() };
        assert!(!manual.autostart_args);
        assert_eq!(decide(&manual), Decision::Skip("Hayate was not started by its Start with Windows shortcut for the startup manager (update, restart or manual start)"));
        // the flag typed by hand on a machine whose own autorun was never saved for the manager (or is disabled): still no
        let typed = LogonCtx { autostart_args: is_autostart_args(&s(&["hayate.exe", "--resident", LOGON_FLAG])), autorun_ok: false, ..ctx() };
        assert!(matches!(decide(&typed), Decision::Skip(w) if w.contains("shortcut")), "{:?}", decide(&typed));
    }

    #[test]
    fn off_is_the_default_and_wins_over_everything() {
        assert!(!Config::default().manage);
        assert_eq!(decide(&LogonCtx { manage: Config::default().manage, ..ctx() }), Decision::Skip("the startup manager is off"));
    }

    #[test]
    fn only_the_flagged_autostart_command_line_qualifies() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(is_autostart_args(&s(&[r"C:\b\hayate.exe", "--resident", "--startup-logon"])));
        assert!(is_autostart_args(&s(&["hayate.exe", "--startup-logon", "--resident"])), "either order");
        // plain autostart without the flag, update relaunch, elevation restart, taskbar task, manual starts
        assert!(!is_autostart_args(&s(&["hayate.exe", "--resident"])));
        assert!(!is_autostart_args(&s(&["hayate.exe", "--startup-logon"])), "the flag alone is not an autostart");
        assert!(!is_autostart_args(&s(&["hayate.exe", "--resident", "--update-show", "front"])));
        assert!(!is_autostart_args(&s(&["hayate.exe", "--wait-pid", "12", "--replace-for-admin"])));
        assert!(!is_autostart_args(&s(&["hayate.exe", "--open"])));
        assert!(!is_autostart_args(&s(&["hayate.exe"])));
        assert!(!is_autostart_args(&s(&[])));
    }

    fn ident(session: u32, logon: i64) -> Ident {
        Ident { sid: "010500000000000515000000AABBCCDD".into(), auth: 0x1234, session, logon }
    }

    fn claim_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("HayateStartupFixture-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn claims(d: &Path) -> usize {
        std::fs::read_dir(d).map(|r| r.flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".claim")).count()).unwrap_or(0)
    }

    #[test]
    fn a_logon_runs_at_most_once_and_a_finished_logon_s_claim_goes_with_positive_proof() {
        let d = claim_dir("claim");
        // session 1 is alive with logon time 1000, until it is replaced by logon 2000
        let live = std::cell::Cell::new(1000i64);
        let probe = |_s: u32| SessionState::Logon(live.get());
        let t1 = ident(1, 1000);
        assert_eq!(claim_with(&d, &t1, &probe), Claim::Won);
        assert_eq!(claim_with(&d, &t1, &probe), Claim::AlreadyRan, "resident restart / update / second instance in the same logon");
        // the user signs out and in again: session 1 now reports logon 2000
        live.set(2000);
        let t2 = ident(1, 2000);
        assert_eq!(claim_with(&d, &t2, &probe), Claim::Won, "the next logon is a new one");
        assert_eq!(claim_with(&d, &t2, &probe), Claim::AlreadyRan);
        assert_eq!(claims(&d), 1, "the earlier logon's claim was removed because Windows says that logon is over");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// RED against the old code, which deleted every other claim in the folder.
    #[test]
    fn a_second_concurrent_logon_of_the_same_user_does_not_delete_the_first_ones_claim() {
        let d = claim_dir("claim2");
        let probe = |s: u32| SessionState::Logon(if s == 1 { 1000 } else { 2000 });
        let (a, b) = (ident(1, 1000), ident(2, 2000));
        assert_eq!(claim_with(&d, &a, &probe), Claim::Won);
        assert_eq!(claim_with(&d, &b, &probe), Claim::Won, "another logon session of the same user");
        assert_eq!(claim_with(&d, &a, &probe), Claim::AlreadyRan, "the first logon's claim must survive the second logon's");
        assert_eq!(claims(&d), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_claim_is_only_deleted_on_positive_proof() {
        let d = claim_dir("claim3");
        let alive = |_s: u32| SessionState::Logon(1000);
        assert_eq!(claim_with(&d, &ident(1, 1000), &alive), Claim::Won);
        // unknown: keep
        assert_eq!(claim_with(&d, &ident(2, 2000), &|_| SessionState::Unknown), Claim::Won);
        assert_eq!(claims(&d), 2, "unknown is not proof");
        // an unparseable file is not ours to delete
        std::fs::write(d.join("startup-run-deadbeefdeadbeef.claim"), b"garbage").unwrap();
        assert_eq!(claim_with(&d, &ident(3, 3000), &|_| SessionState::Unknown), Claim::Won);
        assert_eq!(claims(&d), 4);
        // the session of the first one is gone
        assert_eq!(claim_with(&d, &ident(4, 4000), &|s| if s == 1 { SessionState::Gone } else { SessionState::Unknown }), Claim::Won);
        assert!(!d.join(format!("startup-run-{:016x}.claim", fnv(&ident(1, 1000).key()))).exists(), "the gone session's claim is removed");
        assert_eq!(claims(&d), 4, "only that one went");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_claim_that_cannot_be_written_or_flushed_is_a_failure_not_a_win() {
        let d = claim_dir("claim4");
        CLAIM_FAIL.with(|c| c.set(true));
        let r = claim_with(&d, &ident(1, 1000), &|_| SessionState::Unknown);
        CLAIM_FAIL.with(|c| c.set(false));
        assert!(matches!(r, Claim::Failed(ref e) if e.contains("could not record")), "{r:?}");
        assert_eq!(claims(&d), 0, "no phantom claim is left behind");
        assert_eq!(claim_with(&d, &ident(1, 1000), &|_| SessionState::Unknown), Claim::Won, "the next try can still win");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn racing_instances_get_exactly_one_win() {
        let d = claim_dir("race");
        let wins = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let (d, w) = (d.clone(), wins.clone());
                std::thread::spawn(move || {
                    if claim_with(&d, &ident(1, 1000), &|_| SessionState::Logon(1000)) == Claim::Won {
                        w.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(wins.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_claim_that_cannot_be_recorded_does_not_run() {
        let f = std::env::temp_dir().join(format!("HayateStartupFixture-claimfile-{}", std::process::id()));
        std::fs::write(&f, b"not a folder").unwrap();
        assert!(matches!(claim(&f, &ident(1, 1)), Claim::Failed(_)));
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn logon_age_handles_missing_and_future_times() {
        assert_eq!(logon_age_secs(0, 100), None);
        assert_eq!(logon_age_secs(-5, 100), None);
        assert_eq!(logon_age_secs(500, 100), None);
        assert_eq!(logon_age_secs(0x1_0000_0000, 0x1_0000_0000 + 30 * 10_000_000), Some(30));
    }

    #[test]
    fn windows_reports_this_logon() {
        // soft: a service-like harness may not have a session to report
        if let Some((id, l)) = logon_ident() {
            assert!(!id.sid.is_empty() && id.auth != 0, "{id:?}");
            let age = logon_age_secs(l.filetime, approved::filetime_now() as i64);
            assert!(age.is_some(), "logon time {} should be in the past", l.filetime);
            assert_eq!(session_state(l.session), SessionState::Logon(l.filetime), "this very session reports its own logon time");
        }
    }

    #[test]
    fn the_idle_gate_uses_each_sampler_reading_once() {
        let feed: Arc<M<(u64, f32)>> = Arc::new(M::new((0, 0.0)));
        let f2 = feed.clone();
        let mut env = LiveEnv::new(Box::new(move || {
            let g = *f2.lock().unwrap();
            (g.0 != u64::MAX).then_some(g)
        }));
        *feed.lock().unwrap() = (5, 12.0);
        assert_eq!(env.cpu_percent(), Some(12.0));
        assert_eq!(env.cpu_percent(), None, "the same sampler tick is not a second sample");
        *feed.lock().unwrap() = (6, 7.0);
        assert_eq!(env.cpu_percent(), Some(7.0));
        *feed.lock().unwrap() = (u64::MAX, 0.0);
        assert_eq!(env.cpu_percent(), None, "no reading yet (sampler not ready)");
    }

    // ---- runner with a fake machine ----

    struct Fake {
        running: HashSet<String>,
    }
    impl Env for Fake {
        fn cpu_percent(&mut self) -> Option<f32> {
            Some(0.0)
        }
        fn is_running(&mut self, p: &str) -> bool {
            self.running.contains(p)
        }
    }

    fn item(id: &str, name: &str) -> Item {
        Item { id: id.into(), name: name.into(), delay_ms: 0, after: None, cpu_below: None, process: None, manual: false }
    }

    type Calls = Arc<M<Vec<(String, bool)>>>;

    fn recorder(fail: &'static str) -> (Starter, Calls) {
        let calls: Calls = Arc::new(M::new(Vec::new()));
        let c2 = calls.clone();
        let st: Starter = Arc::new(move |cmd, admin| {
            c2.lock().unwrap().push((cmd.to_string(), admin));
            if !fail.is_empty() && cmd.contains(fail) {
                Err("no such file".into())
            } else {
                Ok("(pid 1)".into())
            }
        });
        (st, calls)
    }

    #[test]
    fn a_run_follows_the_dependency_order_and_records_every_outcome() {
        let shared = Arc::new(RunShared::default());
        let mut b = item("b", "B");
        b.after = Some("a".into());
        b.delay_ms = 60;
        let mut c = item("c", "C");
        c.process = Some("c.exe".into());
        let (st, calls) = recorder("");
        let cmds = vec![("cmd-a".into(), false), ("cmd-b".into(), true), ("cmd-c".into(), false)];
        let env = Fake { running: ["c.exe".to_string()].into() };
        run_plan(&shared, "test", vec![item("a", "A"), b, c], cmds, Box::new(env), st, None).unwrap();
        let v = shared.view();
        assert!(!v.active && !shared.is_active());
        assert_eq!(v.rows.iter().map(|r| r.ok).collect::<Vec<_>>(), vec![Some(true), Some(true), Some(true)]);
        assert!(v.rows[2].status.contains("already running"));
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["cmd-a", "cmd-b"], "the running one is skipped; B follows A");
        assert!(calls[1].1, "the admin flag reaches the launcher");
        assert!(v.log.iter().any(|l| l.contains("A: Started (pid 1)")), "{:?}", v.log);
    }

    #[test]
    fn a_failed_prerequisite_blocks_its_dependent_and_says_why() {
        let shared = Arc::new(RunShared::default());
        let mut b = item("b", "B");
        b.after = Some("a".into());
        let (st, calls) = recorder("bad-a");
        run_plan(&shared, "t", vec![item("a", "A"), b], vec![("bad-a".into(), false), ("cmd-b".into(), false)], Box::new(Fake { running: HashSet::new() }), st, None).unwrap();
        let v = shared.view();
        assert_eq!(v.rows[0].ok, Some(false));
        assert!(v.rows[0].status.contains("Failed: no such file"));
        assert!(v.rows[1].status.contains("Not started"), "{:?}", v.rows[1]);
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn cancel_stops_waiting_entries_without_launching_them() {
        let shared = Arc::new(RunShared::default());
        let mut slow = item("a", "A");
        slow.delay_ms = 30_000;
        let (st, calls) = recorder("");
        let s2 = shared.clone();
        let h = std::thread::spawn(move || run_plan(&s2, "t", vec![slow], vec![("cmd".into(), false)], Box::new(Fake { running: HashSet::new() }), st, None));
        let t0 = Instant::now();
        while !shared.is_active() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        shared.cancel();
        h.join().unwrap().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5), "cancel does not wait out the delay");
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(shared.view().rows[0].status, "Cancelled");
    }

    #[test]
    fn only_one_run_at_a_time() {
        let shared = Arc::new(RunShared::default());
        shared.active.store(true, Ordering::SeqCst);
        let (st, _c) = recorder("");
        let e = run_plan(&shared, "t", vec![item("a", "A")], vec![("c".into(), false)], Box::new(Fake { running: HashSet::new() }), st, None).unwrap_err();
        assert!(e.contains("already in progress"));
        assert!(shared.is_active(), "the other run's flag is left alone");
    }

    #[test]
    fn a_panicking_launcher_is_a_failure_not_a_crash() {
        let shared = Arc::new(RunShared::default());
        let st: Starter = Arc::new(|_, _| panic!("boom"));
        run_plan(&shared, "t", vec![item("a", "A")], vec![("c".into(), false)], Box::new(Fake { running: HashSet::new() }), st, None).unwrap();
        assert!(shared.view().rows[0].status.contains("launcher crashed"));
        assert!(!shared.is_active());
    }

    #[test]
    fn build_runs_enabled_entries_and_one_entry_runs_without_gates() {
        let mut cfg = Config::default();
        let mut a = App::new("a1".into(), "A".into(), r"C:\x\a.exe".into());
        a.delay_ms = 5000;
        a.cpu_below = Some(10);
        let mut b = App::new("a2".into(), "B".into(), r"C:\x\b.exe".into());
        b.enabled = false;
        cfg.apps = vec![a, b];
        let (items, cmds) = build(&cfg, None, false).unwrap();
        assert_eq!((items.len(), cmds.len()), (1, 1));
        assert_eq!(items[0].delay_ms, 5000);
        let (one, _) = build(&cfg, Some("a2"), false).unwrap();
        assert_eq!((one[0].delay_ms, one[0].after.clone(), one[0].cpu_below), (0, None, None), "Run now is immediate, even for a disabled entry");
        assert_eq!(one[0].process.as_deref(), Some("b.exe"));
        assert!(build(&Config::default(), None, false).unwrap_err().contains("No entry"));
        assert!(build(&cfg, Some("zz"), false).is_err());
    }

    // ---- the async launch seam (a launcher that blocks, a run that gives up) ----

    fn short_timeout(ms: u64) {
        sched::LAUNCH_TIMEOUT_TEST.with(|c| c.set(ms));
    }

    /// A launcher that does not return until released.
    fn blocking_starter() -> (Starter, std::sync::mpsc::Sender<()>, Calls) {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let rx = Arc::new(M::new(rx));
        let calls: Calls = Arc::new(M::new(Vec::new()));
        let c2 = calls.clone();
        let st: Starter = Arc::new(move |cmd, admin| {
            c2.lock().unwrap().push((cmd.to_string(), admin));
            let _ = rx.lock().unwrap().recv_timeout(Duration::from_secs(20));
            Ok("(pid 9)".into())
        });
        (st, tx, calls)
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// RED against the old code: the 120 s timeout marked the entry Failed and
    /// freed it, while the detached launcher could still start it later, so a
    /// Retry started a second copy. Now: Indeterminate, the reservation outlives
    /// the run, a new run / Retry is refused, and the late answer is recorded.
    #[test]
    fn a_launch_that_outlives_its_run_stays_reserved_until_it_really_returns() {
        short_timeout(150);
        let shared = Arc::new(RunShared::default());
        let mut a = item("a", "A");
        a.process = Some("slow.exe".into());
        let (st, release, calls) = blocking_starter();
        let t0 = Instant::now();
        run_plan(&shared, "t", vec![a.clone()], vec![("slow".into(), false)], Box::new(Fake { running: HashSet::new() }), st, None).unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5), "the run ends when the launch times out");
        short_timeout(0);
        let v = shared.view();
        assert!(v.rows[0].status.starts_with("Unconfirmed"), "not Failed, not Started: {:?}", v.rows[0]);
        assert_eq!(v.rows[0].ok, Some(false));
        assert_eq!(shared.in_flight(), vec!["A".to_string()], "the launcher thread is still inside the OS call");

        // Retry / Run now / another run: refused while it is in flight, nothing is started
        let (st2, calls2) = recorder("");
        let e = run_plan(&shared, "retry", vec![a], vec![("slow".into(), false)], Box::new(Fake { running: HashSet::new() }), st2, None).unwrap_err();
        assert!(e.contains("has not reported back"), "{e}");
        assert!(calls2.lock().unwrap().is_empty(), "no second copy");
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(!shared.is_active(), "the refused run did not leave the run flag set");

        // the launcher finally returns: the reservation is released and the row tells the truth
        release.send(()).unwrap();
        wait_until("the late launcher to return", || shared.in_flight().is_empty());
        wait_until("the late result on the row", || shared.view().rows[0].status.starts_with("Started late"));
        assert!(shared.view().log.iter().any(|l| l.contains("[late]") && l.contains("did start")), "{:?}", shared.view().log);
    }

    /// The gate in the worker: a launch that has not been handed to Windows yet is
    /// refused after Cancel; one already in flight is counted, not claimed cancelled.
    #[test]
    fn cancel_refuses_launches_not_yet_handed_over_and_counts_the_ones_in_flight() {
        let shared = Arc::new(RunShared::default());
        assert!(shared.begin_launch(1, 0, "A"), "nothing cancelled yet");
        assert_eq!(shared.cancel(), 1, "one launch is already with Windows and cannot be recalled");
        assert!(!shared.begin_launch(1, 1, "B"), "the gate refuses every launch after Cancel");
        assert_eq!(shared.in_flight(), vec!["A".to_string()], "B never became a reservation");
        shared.end_launch(1, 0);
        assert!(shared.in_flight().is_empty());
        // a new run starts clean
        let (st, calls) = recorder("");
        run_plan(&shared, "t", vec![item("a", "A")], vec![("c".into(), false)], Box::new(Fake { running: HashSet::new() }), st, None).unwrap();
        assert_eq!(calls.lock().unwrap().len(), 1, "the cancel flag does not leak into the next run");
    }

    #[test]
    fn a_launch_cancelled_at_the_gate_is_reported_cancelled_and_never_started() {
        let shared = Arc::new(RunShared::default());
        let (st, calls) = recorder("");
        // cancel arrives after the scheduler handed the launch to the worker but before the OS call:
        // the starter itself cancels the flag of a SECOND item whose worker has not reached the gate
        let sh = shared.clone();
        let st2: Starter = Arc::new(move |c, a| {
            let r = st(c, a);
            sh.cancel();
            r
        });
        let mut b = item("b", "B");
        b.after = Some("a".into());
        b.delay_ms = 200;
        run_plan(&shared, "t", vec![item("a", "A"), b], vec![("cmd-a".into(), false), ("cmd-b".into(), false)], Box::new(Fake { running: HashSet::new() }), st2, None).unwrap();
        let v = shared.view();
        assert_eq!(v.rows[0].ok, Some(true));
        assert_eq!(v.rows[1].status, "Cancelled", "{:?}", v.rows[1]);
        assert_eq!(calls.lock().unwrap().len(), 1, "B was never handed to Windows");
    }

    #[test]
    fn the_logon_run_never_starts_an_admin_entry_and_blocks_what_waits_for_it() {
        let mut cfg = Config::default();
        let mut a = App::new("a1".into(), "Needs admin".into(), r"C:\x\a.exe".into());
        a.admin = true;
        let mut b = App::new("a2".into(), "After it".into(), r"C:\x\b.exe".into());
        b.after = Some("a1".into());
        let c = App::new("a3".into(), "Plain".into(), r"C:\x\c.exe".into());
        cfg.apps = vec![a, b, c];
        let (items, cmds) = build(&cfg, None, true).unwrap();
        assert!(items[0].manual && !items[1].manual && !items[2].manual);
        let shared = Arc::new(RunShared::default());
        let (st, calls) = recorder("");
        run_plan(&shared, "Logon run", items, cmds, Box::new(Fake { running: HashSet::new() }), st, None).unwrap();
        let v = shared.view();
        assert!(v.rows[0].status.contains("Run now"), "{:?}", v.rows[0]);
        assert!(v.rows[1].status.starts_with("Not started"), "{:?}", v.rows[1]);
        assert_eq!(calls.lock().unwrap().iter().map(|c| c.0.clone()).collect::<Vec<_>>(), vec![r"C:\x\c.exe".to_string()], "only the plain entry ran");
        // an explicit Run now / Run all now is a person asking: admin entries are allowed there
        let (items, _) = build(&cfg, None, false).unwrap();
        assert!(items.iter().all(|i| !i.manual));
        let (one, _) = build(&cfg, Some("a1"), true).unwrap();
        assert!(!one[0].manual, "one entry run by its own button");
    }

    #[test]
    fn an_entry_whose_takeover_was_never_confirmed_is_not_run() {
        use crate::startup::id::{EntryId, Kind, Scope, View};
        use crate::startup::model::{Own, OwnState};
        let mut cfg = Config::default();
        let mut a = App::new("a1".into(), "Half taken".into(), r"C:\x\a.exe".into());
        a.own = Some(Own { source: EntryId::new(Kind::Run, Scope::User, View::Native, "x"), state: OwnState::Pending, original: None, written: vec![3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], raw_at_adopt: String::new() });
        let b = App::new("a2".into(), "Fine".into(), r"C:\x\b.exe".into());
        cfg.apps = vec![a, b];
        let (items, _) = build(&cfg, None, true).unwrap();
        assert_eq!(items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["a2"]);
        assert!(build(&cfg, Some("a1"), false).unwrap_err().contains("never confirmed"));
    }
}
