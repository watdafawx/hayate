//! Services tab: every Win32 service with state, PID and start type, plus
//! Start / Stop / Restart for the selected one, a right-click menu (the same
//! actions, startup type, Properties) and a Properties panel (ui/svcprops.rs).
//! Uses only the documented Service Control Manager API (EnumServicesStatusEx,
//! StartService, ControlService, ChangeServiceConfig; see svcctl.rs). Listing
//! works unelevated; controlling a service needs the rights the SCM grants
//! (usually administrator) and says so when denied.
//! The SCM (an RPC to services.exe, which can stall during a start storm) and
//! the registry are read on the background I/O worker; paint only shows the
//! latest list.

use super::*;
use crate::osutil::{from_wide, wide};
use crate::watch::scm;
use std::collections::{HashMap, HashSet};
use crate::svcctl::{self, Op, StartKind, Target};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_MORE_DATA;
use windows::Win32::System::Services::*;

#[derive(Clone, Default, PartialEq)]
pub struct Row {
    pub name: String,
    pub display: String,
    pub state: u32,
    pub pid: u32,
    pub start: u32,
    pub can_stop: bool,
    /// image path (registry), for the row icon; svchost-hosted ones share the gears
    pub path: String,
    /// CompanyName of the image (the ServiceDll for svchost-hosted services)
    pub publisher: String,
    /// Automatic (delayed start)
    pub delayed: bool,
    /// started on a trigger (device arrival, network, ...)
    pub trigger: bool,
}

/// Registry and file facts per service that do not change while it is installed.
#[derive(Clone, Default)]
struct Extra {
    publisher: String,
    delayed: bool,
    trigger: bool,
}

/// Only the I/O worker touches it.
#[derive(Default)]
struct Cache {
    /// start type and image path per lower-case service name
    cfg: Option<HashMap<String, scm::SvcInfo>>,
    extra: HashMap<String, Extra>,
    /// CompanyName per image path
    publishers: HashMap<String, String>,
}

pub const CHIPS: [&str; 4] = ["All", "Running", "Stopped", "Non-Microsoft"];
const GROUPS: [&str; 3] = ["Automatic, not running", "Running", "Stopped"];

#[derive(Default)]
pub struct ServicesView {
    pub rows: Arc<Vec<Row>>,
    pub scroll: f32,
    pub selected: Option<String>,
    pub chip: usize,
    pub collapsed: [bool; 3],
    /// keyboard: the rows of the last paint in display order (indices into `rows`)
    pub nav: super::nav::ListNav<usize>,
    widths: Widths,
    /// the list, produced on the I/O worker
    data: crate::bgio::Cell<Arc<Vec<Row>>>,
    cache: Arc<Mutex<Cache>>,
    /// re-read the registry on the next list refresh
    cfg_stale: Arc<AtomicBool>,
    /// last `EPOCH` seen: an action finished since, so re-read config and states
    seen_epoch: u32,
}

/// Bumped when a Start/Stop/Restart/startup change finishes (on any thread).
static EPOCH: AtomicU32 = AtomicU32::new(0);

/// An action finished: the next paint re-reads the list and the start types.
pub fn bump() {
    EPOCH.fetch_add(1, Ordering::Relaxed);
    super::icons::poke();
}

/// Result line of the last Start/Stop/Restart, shown in the header.
static MSG: Mutex<String> = Mutex::new(String::new());

/// Tests that read the header line back take this, so they do not see each other's.
#[cfg(test)]
pub(crate) static MSG_TEST_LOCK: Mutex<()> = Mutex::new(());

pub fn last_message() -> String {
    MSG.lock().map(|m| m.clone()).unwrap_or_default()
}

/// Status pill: (label, fill, border, dot, text). A fill of 0 means none
/// (stopped: outline dot only).
fn pill_style(r: &Row) -> (&'static str, u32, u32, u32, u32) {
    match SERVICE_STATUS_CURRENT_STATE(r.state) {
        SERVICE_RUNNING => ("Running", OK_BG, 0, OK, OK_TX),
        SERVICE_STOPPED if attention(r) => ("Not running", WARN_BG, WARN_LN, WARN, WARN_TX),
        SERVICE_STOPPED => ("Stopped", 0, 0, TX4, TX3),
        SERVICE_START_PENDING => ("Starting", ACC_BG, 0, ACC, ACC_TX),
        SERVICE_STOP_PENDING => ("Stopping", ACC_BG, 0, ACC, ACC_TX),
        SERVICE_PAUSED => ("Paused", ACC_BG, 0, ACC, ACC_TX),
        _ => ("…", ACC_BG, 0, ACC, ACC_TX),
    }
}

/// An Automatic service (not delayed, not trigger-started) that is not
/// running: the one state worth noticing.
fn attention(r: &Row) -> bool {
    r.state == SERVICE_STOPPED.0 && r.start == 2 && !r.delayed && !r.trigger
}

fn group_of(r: &Row) -> usize {
    if attention(r) {
        0
    } else if r.state == SERVICE_STOPPED.0 {
        2
    } else {
        1
    }
}

fn is_microsoft(r: &Row) -> bool {
    r.publisher.starts_with("Microsoft")
}

fn chip_keeps(chip: usize, r: &Row) -> bool {
    match chip {
        1 => r.state != SERVICE_STOPPED.0,
        2 => r.state == SERVICE_STOPPED.0,
        3 => !is_microsoft(r),
        _ => true,
    }
}

fn reg_sz(sub: &str, value: &str) -> String {
    use windows::Win32::System::Registry::*;
    let (s, v) = (wide(sub), wide(value));
    let mut buf = vec![0u16; 1024];
    let mut n = (buf.len() * 2) as u32;
    // RRF_RT_REG_SZ also takes REG_EXPAND_SZ and expands it
    let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, PCWSTR(s.as_ptr()), PCWSTR(v.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut n)) };
    if r.is_ok() {
        from_wide(&buf)
    } else {
        String::new()
    }
}

fn reg_dword(sub: &str, value: &str) -> Option<u32> {
    use windows::Win32::System::Registry::*;
    let (s, v) = (wide(sub), wide(value));
    let mut d = 0u32;
    let mut n = 4u32;
    let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, PCWSTR(s.as_ptr()), PCWSTR(v.as_ptr()), RRF_RT_REG_DWORD, None, Some(&mut d as *mut u32 as *mut _), Some(&mut n)) };
    r.is_ok().then_some(d)
}

fn reg_key_exists(sub: &str) -> bool {
    use windows::Win32::System::Registry::*;
    let s = wide(sub);
    let mut k = HKEY::default();
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(s.as_ptr()), None, KEY_READ, &mut k) }.is_ok() {
        unsafe {
            let _ = RegCloseKey(k);
        }
        true
    } else {
        false
    }
}

/// Delayed / trigger start and publisher (a few registry reads and, once per
/// image, a version-resource read). Worker thread only.
fn read_extra(name: &str, path: &str, publishers: &mut HashMap<String, String>) -> Extra {
    let key = format!("SYSTEM\\CurrentControlSet\\Services\\{name}");
    let delayed = reg_dword(&key, "DelayedAutostart") == Some(1);
    let trigger = reg_key_exists(&format!("{key}\\TriggerInfo"));
    // svchost-hosted: the publisher is the DLL's, not svchost's
    let mut image = path.to_string();
    if path.to_ascii_lowercase().ends_with("\\svchost.exe") {
        let mut dll = reg_sz(&format!("{key}\\Parameters"), "ServiceDll");
        if dll.is_empty() {
            dll = reg_sz(&key, "ServiceDll");
        }
        if !dll.is_empty() {
            image = crate::osutil::clean_image_path(&dll);
        }
    }
    let publisher = if image.is_empty() {
        String::new()
    } else {
        publishers
            .entry(image.to_ascii_lowercase())
            .or_insert_with(|| {
                let p = crate::sample::meta::company_name(&image).map(|s| s.to_string()).unwrap_or_default();
                if p == "Microsoft Corporation" {
                    "Microsoft".to_string()
                } else {
                    p
                }
            })
            .clone()
    };
    Extra { publisher, delayed, trigger }
}

impl ServicesView {
    /// Re-read the registry cache (start types) with the next refresh; run on
    /// every switch to the tab.
    pub fn reload_config(&mut self) {
        self.cfg_stale.store(true, Ordering::Relaxed);
        self.data.invalidate();
    }

    /// Ask the worker for live service states (at most every 2 s while shown)
    /// and take whatever list it last produced. Never waits.
    ///
    /// `hold`: a context menu is open; keep the rows (and their order) as they are
    /// until it closes. The menu's target is captured by service name anyway.
    fn refresh(&mut self, hold: bool) -> bool {
        let e = EPOCH.load(Ordering::Relaxed);
        if e != self.seen_epoch {
            self.seen_epoch = e;
            self.cfg_stale.store(true, Ordering::Relaxed);
            self.data.invalidate();
        }
        let (cache, stale) = (self.cache.clone(), self.cfg_stale.clone());
        self.data.refresh("services list", Duration::from_millis(1900), move || {
            crate::bgio::fake_stall("scm");
            let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
            if c.cfg.is_none() || stale.swap(false, Ordering::Relaxed) {
                c.cfg = Some(scm::read_all());
                c.extra.clear();
            }
            let mut rows = enumerate();
            let Cache { cfg, extra, publishers } = &mut *c;
            for r in &mut rows {
                let lname = r.name.to_lowercase();
                if let Some(s) = cfg.as_ref().and_then(|m| m.get(&lname)) {
                    r.start = s.start;
                    r.path = s.path.clone();
                }
                let x = extra.entry(lname).or_insert_with(|| read_extra(&r.name, &r.path, publishers));
                r.publisher = x.publisher.clone();
                r.delayed = x.delayed;
                r.trigger = x.trigger;
            }
            drop(c);
            rows.sort_by_cached_key(|r| r.display.to_lowercase());
            super::icons::poke();
            Some(Arc::new(rows))
        });
        match adopt(&self.rows, self.data.get(), hold) {
            Some(rows) => {
                let changed = *rows != *self.rows;
                self.rows = rows;
                changed
            }
            None => false,
        }
    }
}

/// The list to show after a refresh: the worker's newest one, unless a menu is open (the rows and
/// their order stay put until it closes).
fn adopt(cur: &Arc<Vec<Row>>, next: Option<Arc<Vec<Row>>>, hold: bool) -> Option<Arc<Vec<Row>>> {
    if hold {
        return None;
    }
    next.filter(|n| !Arc::ptr_eq(n, cur))
}

fn enumerate() -> Vec<Row> {
    let mut out = Vec::new();
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_ENUMERATE_SERVICE) else { return out };
        let mut buf = vec![0u8; 256 * 1024];
        let mut resume = 0u32;
        loop {
            let mut need = 0u32;
            let mut count = 0u32;
            let r = EnumServicesStatusExW(scm, SC_ENUM_PROCESS_INFO, SERVICE_WIN32, SERVICE_STATE_ALL, Some(&mut buf), &mut need, &mut count, Some(&mut resume), PCWSTR::null());
            let p = buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW;
            for i in 0..count as usize {
                let e = &*p.add(i);
                let st = &e.ServiceStatusProcess;
                out.push(Row {
                    name: from_wide(std::slice::from_raw_parts(e.lpServiceName.0, 256.min(wlen(e.lpServiceName.0)))),
                    display: from_wide(std::slice::from_raw_parts(e.lpDisplayName.0, 256.min(wlen(e.lpDisplayName.0)))),
                    state: st.dwCurrentState.0,
                    pid: st.dwProcessId,
                    start: u32::MAX,
                    can_stop: st.dwControlsAccepted & SERVICE_ACCEPT_STOP != 0,
                    ..Default::default()
                });
            }
            match r {
                Ok(()) => break,
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => continue,
                Err(_) => break,
            }
        }
        let _ = CloseServiceHandle(scm);
    }
    out
}

unsafe fn wlen(p: *const u16) -> usize {
    let mut n = 0;
    while !p.is_null() && *p.add(n) != 0 && n < 256 {
        n += 1;
    }
    n
}

/// Which header line is current: every new line takes the next number, so an Undo offered for an
/// older line is gone the moment anything else is said.
static MSG_SEQ: AtomicU32 = AtomicU32::new(0);

/// Replace the header line; returns its number.
fn set_msg(m: String) -> u32 {
    let mut g = MSG.lock().unwrap_or_else(|e| e.into_inner());
    *g = m;
    MSG_SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

/// A result line for the header from outside this module (the Properties panel's startup change,
/// which may finish after the panel was closed).
pub fn report(m: String) {
    set_msg(m);
    bump();
}

/// What the user was looking at: the row, by service KEY name, as of now.
fn target_of(r: &Row) -> Target {
    Target { name: r.name.clone(), display: r.display.clone(), path: r.path.clone(), state: r.state, pid: r.pid, can_stop: r.can_stop, start: r.start, delayed: r.delayed, trigger: r.trigger }
}

/// Start / Stop / Restart on a worker thread; the result lands in the header.
pub fn run(t: Target, op: Op) {
    set_msg(format!("{} {}…", op.progress(), if t.display.is_empty() { &t.name } else { &t.display }));
    svcctl::spawn_op(t, op, |r| {
        set_msg(match r {
            Ok(m) | Err(m) => m,
        });
        bump();
    });
}

/// Change the startup type (an explicit click on one type: menu, toolbar or Alt+1..4). The
/// service must still have the type the list showed. A change that really happened offers Undo.
pub fn set_startup(t: Target, want: StartKind) {
    change_startup(t, want, true);
}

fn label_of(t: &Target) -> String {
    if t.display.is_empty() { t.name.clone() } else { t.display.clone() }
}

/// The one write path of every startup-type control: `svcctl::spawn_startup`, with its per-service
/// guard, 4-change limit, fresh re-read and "changed since you looked" refusal. `undoable`: offer
/// Undo for the result (false for an Undo itself, which is never chained).
fn change_startup(t: Target, want: StartKind, undoable: bool) {
    let Some(expect) = t.kind() else {
        set_msg(format!("{}: this start type cannot be changed here", t.name));
        return;
    };
    let name = label_of(&t);
    if is_protected(&t) {
        // also the Undo of a change: it never goes to Windows for a service it refuses
        set_msg(format!("{name}: {}", Off::Protected.reason()));
        return;
    }
    set_msg(format!("Changing {name} to {}…", want.label()));
    let seen = t.clone();
    svcctl::spawn_startup(t, expect, want, move |r| {
        finish_startup(&seen, expect, want, &name, r, undoable);
        bump();
    });
}

/// Result line of a startup change, and the Undo that goes with it.
fn finish_startup(t: &Target, expect: StartKind, want: StartKind, name: &str, r: Result<svcctl::StartupDone, String>, undoable: bool) {
    match r {
        Ok(d) => {
            let seq = set_msg(format!("{name}: {}", d.note));
            if let Some((after, risky)) = undo_after(t, expect, want, &d).filter(|_| undoable) {
                *UNDO.lock().unwrap_or_else(|e| e.into_inner()) = Some(Undo { t: after, back: expect, risky, seq, at: Instant::now() });
            }
        }
        Err(e) => {
            if note_refusal(&t.key(), &e, crate::osutil::is_elevated()) {
                set_msg(format!("{name}: {e} Its startup controls are now off for this session."));
            } else {
                set_msg(format!("{name}: {e}"));
            }
        }
    }
}

// ---------- quick startup type: menu section, toolbar segments, Alt+1..4 ----------

/// The four types in the order every quick control shows them (low risk to boot-time); Alt+1..4
/// follow it. The menu ids stay `ID_KIND + index in StartKind::ALL`.
pub const QUICK_ORDER: [StartKind; 4] = [StartKind::Disabled, StartKind::Manual, StartKind::Automatic, StartKind::AutomaticDelayed];

fn kind_id(k: StartKind) -> usize {
    StartKind::ALL.iter().position(|x| *x == k).unwrap_or(0)
}

/// Toolbar segment label; `short` for a narrow window.
pub fn seg_label(k: StartKind, short: bool) -> &'static str {
    match (k, short) {
        (StartKind::Disabled, false) => "Disabled",
        (StartKind::Disabled, true) => "Off",
        (StartKind::Manual, _) => "Manual",
        (StartKind::Automatic, _) => "Auto",
        (StartKind::AutomaticDelayed, false) => "Auto (delayed)",
        (StartKind::AutomaticDelayed, true) => "Delayed",
    }
}

fn kind_tip(k: StartKind) -> &'static str {
    match k {
        StartKind::Disabled => "Windows will not start it, at boot or on request. A running service keeps running until you stop it.",
        StartKind::Manual => "Starts only when you or another program asks for it.",
        StartKind::Automatic => "Starts with Windows, in the main boot phase.",
        StartKind::AutomaticDelayed => "Starts automatically a minute or two after boot, so the desktop comes up faster.",
    }
}

// ---------- protected services: greyed up front ----------

/// Services that Windows itself protects: on a stock Windows 11 the service's own security
/// descriptor gives Administrators no "change configuration" right (read-only `sc sdshow` scan of
/// a machine, 2026-11-18), so a startup change is refused even when Blink is elevated. These are
/// names Blink already shows in every row; nothing is opened or read to decide (no extra SCM
/// handle, registry read or ACL code). Lower case; per-user services match by prefix below.
const PROTECTED_NAMES: &[&str] = &[
    "appxsvc", "bfe", "brokerinfrastructure", "clipsvc", "coremessagingregistrar", "dcomlaunch", "dnscache", "embeddedmode", "entappsvc", "gpsvc", "mdcoresvc",
    "mpssvc", "msiserver", "ngcctnrsvc", "ngcsvc", "rpceptmapper", "rpcss", "schedule", "securityhealthservice", "sense", "sppsvc", "staterepository",
    "systemeventsbroker", "textinputmanagementservice", "timebrokersvc", "wdnissvc", "windefend", "winhttpautoproxysvc", "wscsvc",
];

/// Per-user service templates (`Name_<luid>`): same protection as the template.
const PROTECTED_USER_PREFIXES: &[&str] = &["credentialenrollmentmanagerusersvc_", "deviceassociationbrokersvc_", "printworkflowusersvc_"];

/// Pure: protected by name or by program folder (the Defender / Windows Security folders, from the
/// same `rules/protect.rs` list that keeps Blink from ending those programs). The Windows folder
/// is NOT used: nearly every service lives there and most can be changed.
fn protected_by(ctx: &crate::rules::protect::Ctx, name: &str, path: &str) -> bool {
    let n = name.to_ascii_lowercase();
    PROTECTED_NAMES.contains(&n.as_str())
        || PROTECTED_USER_PREFIXES.iter().any(|p| n.starts_with(p))
        || (!path.is_empty() && matches!(crate::rules::protect::path_check(ctx, Some(path)), Err(crate::rules::protect::Refusal::SecurityTool)))
}

/// Services that refused a startup change while Blink was elevated: Windows protects them. Kept
/// for the rest of the session (lower-case key names); filled from the result of the one guarded
/// write path, never by probing.
static REFUSED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn refused_set<R>(f: impl FnOnce(&mut HashSet<String>) -> R) -> R {
    f(REFUSED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashSet::new))
}

/// Is this service greyed as protected (the static list, or refused earlier this session)?
pub fn is_protected(t: &Target) -> bool {
    protected_by(crate::rules::protect::Ctx::system(), &t.name, &t.path) || refused_set(|s| s.contains(&t.key()))
}

/// Is `e` the text of a refusal by Windows (access denied)?
fn is_denied_text(e: &str) -> bool {
    e.contains(&svcctl::ScmError::Denied.message())
}

/// A startup change came back with `e` for service key `key`. Access denied while elevated means
/// Windows protects it: grey it for the rest of the session. Unelevated, denied is the normal
/// answer for any service (restart as administrator), so nothing is remembered. Returns whether
/// the service was newly marked.
fn note_refusal(key: &str, e: &str, elevated: bool) -> bool {
    elevated && is_denied_text(e) && refused_set(|s| s.insert(key.to_string()))
}

/// For the Properties panel's own startup change: remember a refusal like the quick controls do.
pub fn note_startup_result(t: &Target, r: &Result<svcctl::StartupDone, String>) {
    if let Err(e) = r {
        note_refusal(&t.key(), e, crate::osutil::is_elevated());
    }
}

/// Why the quick controls are off for the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Off {
    NoSelection,
    /// Windows refuses startup changes for it, even for an administrator
    Protected,
    /// an op or another startup change is in flight on this service
    Busy(svcctl::Work),
    /// the list is still reading start types
    Loading,
    /// no image path yet: nothing to check the live service against
    NoPath,
    /// Boot / System: a driver type, never changed here
    Driver,
    NoKind,
}

impl Off {
    pub fn reason(self) -> String {
        match self {
            Off::NoSelection => "Select a service to change its startup type.".into(),
            Off::Protected => "Windows protects this service. It refuses startup changes, even from an administrator, and Hayate does not bypass that.".into(),
            Off::Busy(w) => format!("This service is {} now. Wait for it to finish.", w.progress()),
            Off::Loading => "Still reading the startup types. Try again in a moment.".into(),
            Off::NoPath => "The list has no program path for it yet, so Hayate cannot tell it from a replacement. Try again when the list has loaded.".into(),
            Off::Driver => "A driver start type (Boot / System). Hayate only changes Automatic, Manual and Disabled.".into(),
            Off::NoKind => "Its startup type is not known from the list, so Hayate will not change it.".into(),
        }
    }
}

/// What the quick controls show and may do for one selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quick {
    /// the current type (marked); None: no selection or not a changeable type
    pub cur: Option<StartKind>,
    /// None: a click may change it
    pub off: Option<Off>,
}

/// Pure: the state of the quick controls for `t` (None: nothing selected) given what it is busy with.
pub fn quick_with(t: Option<&Target>, busy: Option<svcctl::Work>) -> Quick {
    let Some(t) = t else { return Quick { cur: None, off: Some(Off::NoSelection) } };
    let off = if is_protected(t) {
        // known before anything is read: no refusal has to be provoked first
        Some(Off::Protected)
    } else if let Some(w) = busy {
        Some(Off::Busy(w))
    } else if t.start == u32::MAX {
        Some(Off::Loading)
    } else if t.path.is_empty() {
        Some(Off::NoPath)
    } else if matches!(t.start, 0 | 1) {
        Some(Off::Driver)
    } else if t.kind().is_none() {
        Some(Off::NoKind)
    } else {
        None
    };
    Quick { cur: t.kind(), off }
}

pub fn quick(t: Option<&Target>) -> Quick {
    quick_with(t, t.and_then(|t| svcctl::busy_work(&t.key())))
}

/// The selected row, captured by name as of now.
fn selected_target(app: &App) -> Option<Target> {
    app.services.selected.as_ref().and_then(|n| app.services.rows.iter().find(|r| &r.name == n)).map(target_of)
}

/// A quick type pick on `t` (menu item, Alt+1..4): refused with the reason when the controls are
/// off, "already" when it has that type, else the guarded change.
pub fn quick_set(t: Target, want: StartKind) {
    let q = quick(Some(&t));
    if let Some(off) = q.off {
        set_msg(format!("{}: {}", label_of(&t), off.reason()));
    } else if q.cur == Some(want) {
        set_msg(format!("{}: already {}", label_of(&t), want.label()));
    } else {
        set_startup(t, want);
    }
}

/// Alt+1..4 (`d` = 0..3): the selected service. None: nothing selected, the key is not ours.
pub fn quick_key(app: &mut App, d: usize) -> Option<Action> {
    let k = *QUICK_ORDER.get(d)?;
    let t = selected_target(app)?;
    quick_set(t, k);
    Some(Action::Repaint)
}

/// The Undo of the last startup change: another guarded change back to the old type, expecting the
/// type that was just written, so anything that changed it since is refused by the usual
/// "changed since you looked" check and Undo can never overwrite a newer change.
#[derive(Clone, Debug)]
struct Undo {
    /// the service as it is after the change
    t: Target,
    back: StartKind,
    /// Disabled on a running service: amber, longer
    risky: bool,
    /// the header line it belongs to
    seq: u32,
    at: Instant,
}

static UNDO: Mutex<Option<Undo>> = Mutex::new(None);

pub const UNDO_SECS: u64 = 8;
pub const UNDO_RISKY_SECS: u64 = 12;

fn undo_life(risky: bool) -> Duration {
    Duration::from_secs(if risky { UNDO_RISKY_SECS } else { UNDO_SECS })
}

/// Pure: still offered (its line is the current one, and the window has not passed).
fn undo_live(u: &Undo, seq_now: u32, now: Instant) -> bool {
    u.seq == seq_now && now.saturating_duration_since(u.at) < undo_life(u.risky)
}

/// Pure: the target as it stands after the confirmed change, and whether it is risky. None when
/// nothing changed or the read-back did not confirm `want`: no Undo is offered for what Blink
/// cannot vouch for.
fn undo_after(t: &Target, expect: StartKind, want: StartKind, d: &svcctl::StartupDone) -> Option<(Target, bool)> {
    if expect == want || d.after.kind() != Some(want) {
        return None;
    }
    let after = Target { start: d.after.start, delayed: d.after.delayed.unwrap_or(false), state: d.after.status.state, pid: d.after.status.pid, ..t.clone() };
    let running = matches!(d.after.status.state, svcctl::st::RUNNING | svcctl::st::START_PENDING | svcctl::st::PAUSED);
    Some((after, want == StartKind::Disabled && running))
}

/// What the header offers now: (risky, time left). Retires an Undo whose window has passed, and
/// its line with it (the header goes back to the summary).
fn undo_offer(now: Instant) -> Option<(bool, Duration)> {
    let mut g = UNDO.lock().unwrap_or_else(|e| e.into_inner());
    let u = g.as_ref()?;
    let seq = MSG_SEQ.load(Ordering::Relaxed);
    if undo_live(u, seq, now) {
        return Some((u.risky, undo_life(u.risky).saturating_sub(now.saturating_duration_since(u.at))));
    }
    if u.seq == seq {
        // the window passed on a line nothing replaced: let it fade
        *MSG.lock().unwrap_or_else(|e| e.into_inner()) = String::new();
    }
    *g = None;
    None
}

/// Take the Undo (once: a second click finds nothing).
fn take_undo(now: Instant) -> Option<Undo> {
    let mut g = UNDO.lock().unwrap_or_else(|e| e.into_inner());
    let u = g.take()?;
    undo_live(&u, MSG_SEQ.load(Ordering::Relaxed), now).then_some(u)
}

/// Undo the last startup change.
pub fn undo() {
    if let Some(u) = take_undo(Instant::now()) {
        change_startup(u.t, u.back, false);
    }
}

/// How the toolbar fits its controls into `room` (px right of the title). Priority: the Start /
/// Stop / Restart buttons, the startup segments, a minimum of header text, the filter chips last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TbPlan {
    chips: bool,
    short: bool,
    seg: bool,
}

const MSG_MIN: f32 = 150.0;

fn toolbar_plan(room: f32, btns: f32, seg_full: f32, seg_short: f32, chips: f32) -> TbPlan {
    // (chips, short, header text wanted): short words before losing the filter chips
    let tries = [(true, false, MSG_MIN), (true, true, MSG_MIN), (false, false, MSG_MIN), (false, true, MSG_MIN), (false, true, 0.0)];
    for (c, s, m) in tries {
        let need = btns + if s { seg_short } else { seg_full } + if c { chips } else { 0.0 } + m;
        if need <= room {
            return TbPlan { chips: c, short: s, seg: true };
        }
    }
    // even the short control does not fit beside the buttons: the buttons win
    TbPlan { chips: false, short: true, seg: false }
}

/// Debug test hook (main.rs `--test-svc`): the startup change, waited for, result as text.
#[cfg(debug_assertions)]
pub fn set_startup_wait(t: Target, expect: StartKind, want: StartKind) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    svcctl::spawn_startup(t, expect, want, move |r| {
        let _ = tx.send(match r {
            Ok(d) => format!("ok: {}", d.note),
            Err(e) => format!("error: {e}"),
        });
    });
    rx.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| "no answer in 30 s".into())
}

// ---------- right-click menu ----------

pub const ID_START: usize = 1;
pub const ID_STOP: usize = 2;
pub const ID_RESTART: usize = 3;
/// startup type i = ID_KIND + i (StartKind::ALL order)
pub const ID_KIND: usize = 10;
pub const ID_PROPS: usize = 20;
pub const ID_CANCEL: usize = 21;
pub const ID_COPY: usize = 22;
pub const ID_ADMIN: usize = 23;
pub const ID_BUSY: usize = 24;
pub const ID_NOBASE: usize = 25;
/// the "Startup type" caption and the note under the four types (both always off)
pub const ID_HEAD: usize = 26;
pub const ID_NOTE: usize = 27;

/// The right-click menu of one captured service. Pure: what is enabled is
/// decided from the target's state as captured, and each action re-checks the
/// live service before it runs.
pub fn menu(t: &Target, elevated: bool) -> Vec<procmenu::Entry> {
    use procmenu::Entry;
    let busy = svcctl::busy_work(&t.key());
    let q = quick_with(Some(t), busy);
    let item = |id: usize, text: &str, enabled: bool| Entry::Item { id, text: text.to_string(), checked: false, enabled };
    // The items reflect the state the list showed when the menu opened. Every click re-reads the
    // live service on its own handle and refuses if it is no longer that service. A capture with no
    // image path / known startup type has nothing to be checked against: its actions stay off.
    let baseline = t.baseline_problem().is_none();
    let idle = busy.is_none() && baseline;
    let running = matches!(t.state, svcctl::st::RUNNING | svcctl::st::PAUSED);
    let mut m = vec![
        item(ID_START, "Start", t.state == svcctl::st::STOPPED && t.start != 4 && idle),
        item(ID_STOP, "Stop", running && t.can_stop && idle),
        item(ID_RESTART, "Restart", t.state == svcctl::st::RUNNING && t.can_stop && idle),
    ];
    match busy {
        // an op's wait can be abandoned; a startup change is short and is simply waited out
        Some(svcctl::Work::Op(op)) => {
            m.push(Entry::Sep);
            m.push(item(ID_CANCEL, &format!("Stop waiting for {}", op.verb().to_lowercase()), true));
        }
        Some(svcctl::Work::Startup) => {
            m.push(Entry::Sep);
            m.push(item(ID_BUSY, "Changing its startup type…", false));
        }
        None => {}
    }
    // Startup type: one flat section (the same guarded change as the toolbar control and Alt+1..4)
    m.push(Entry::Sep);
    m.push(item(ID_HEAD, "Startup type", false));
    for (n, k) in QUICK_ORDER.iter().enumerate() {
        m.push(Entry::Item { id: ID_KIND + kind_id(*k), text: format!("{}\tAlt+{}", k.label(), n + 1), checked: q.cur == Some(*k), enabled: q.off.is_none() });
    }
    match q.off {
        // the "Changing its startup type…" / "Stop waiting" lines above already say it
        Some(Off::Busy(_)) => {}
        Some(Off::Protected) => m.push(item(ID_NOTE, "Windows protects this service: not changed here", false)),
        Some(Off::NoPath) => m.push(item(ID_NOBASE, "Actions off: the list has no program path for it yet", false)),
        Some(Off::Loading) => m.push(item(ID_NOTE, "Still reading the startup types…", false)),
        Some(Off::Driver) => m.push(item(ID_NOTE, "A driver start type: not changed here", false)),
        Some(_) => m.push(item(ID_NOTE, "Its startup type is not known: not changed here", false)),
        None if running && q.cur != Some(StartKind::Disabled) => m.push(item(ID_NOTE, "Disabled does not stop a running service", false)),
        None => {}
    }
    m.push(Entry::Sep);
    m.push(item(ID_PROPS, "Properties…", true));
    m.push(Entry::Sep);
    m.push(item(ID_COPY, "Copy service name", true));
    if !elevated {
        m.push(Entry::Sep);
        m.push(item(ID_ADMIN, "Restart Hayate as administrator…", true));
    }
    m
}

/// Right-click on row `ri`: select it, capture it, ask for the menu.
pub fn right_click(app: &mut App, ri: usize, x: f32, y: f32) -> Action {
    let Some(r) = app.services.rows.get(ri) else { return Action::None };
    let t = target_of(r);
    app.services.selected = Some(t.name.clone());
    Action::SvcMenu(x, y, Box::new(t))
}

/// Up / Down / Page / Home / End: select by service name from the rows of the last paint.
pub fn nav(app: &mut App, m: super::nav::Move) {
    let rows = app.services.rows.clone();
    let sel = app.services.selected.clone();
    let pick = app.services.nav.step(sel.is_some(), |&i| rows.get(i).is_some_and(|r| Some(&r.name) == sel.as_ref()), m);
    if let Some(r) = pick.and_then(|i| rows.get(i)) {
        app.services.selected = Some(r.name.clone());
    }
}

/// Enter: the selected service's Properties panel (read-only until Apply).
pub fn open_selected(app: &mut App) {
    let t = app.services.selected.as_ref().and_then(|n| app.services.rows.iter().find(|r| &r.name == n)).map(target_of);
    if let Some(t) = t {
        super::svcprops::open(app, t);
    }
}

fn start_label(r: &Row) -> &'static str {
    match (r.start, r.delayed, r.trigger) {
        (2, true, _) => "Automatic (delayed)",
        (2, _, true) => "Automatic (trigger)",
        (2, ..) => "Automatic",
        (3, _, true) => "Manual (trigger)",
        (3, ..) => "Manual",
        (4, ..) => "Disabled",
        (0, ..) => "Boot",
        (1, ..) => "System",
        _ => "",
    }
}

pub fn click(app: &mut App, h: Hit) -> Action {
    match h {
        Hit::Svc(ri) => {
            if let Some(r) = app.services.rows.get(ri) {
                app.services.selected = Some(r.name.clone());
            }
        }
        Hit::SvcBtn(b) => {
            // the selected row, captured by name as of this click
            if let Some(t) = selected_target(app) {
                run(t, match b { 0 => Op::Start, 1 => Op::Stop, _ => Op::Restart });
                app.services.data.invalidate();
            }
        }
        Hit::SvcKind(i) => {
            // a segment that is off (nothing selected, busy, protected row) only shows its reason
            if let (Some(k), Some(t)) = (QUICK_ORDER.get(i), selected_target(app)) {
                if quick(Some(&t)).off.is_none() {
                    quick_set(t, *k);
                }
            }
        }
        Hit::SvcUndo => undo(),
        Hit::SvcChip(i) => {
            app.services.chip = i;
            app.services.scroll = 0.0;
        }
        Hit::SvcGroup(i) => {
            if let Some(c) = app.services.collapsed.get_mut(i) {
                *c = !*c;
            }
        }
        _ => return Action::None,
    }
    Action::Repaint
}

fn matches(r: &Row, q: &str) -> bool {
    q.is_empty() || r.name.to_lowercase().contains(q) || r.display.to_lowercase().contains(q) || r.publisher.to_lowercase().contains(q)
}

const ROW_H: f32 = 27.0;
const BAND_H: f32 = 30.0;

enum Item {
    /// group index, member count
    Band(usize, usize),
    /// index into `rows`, zebra parity
    Row(usize, bool),
}

/// Column x positions and widths inside the table.
struct Cols {
    icon: f32,
    name: (f32, f32),
    status: f32,
    pid: Option<f32>,
    start: (f32, f32),
    publisher: Option<(f32, f32)>,
}

fn columns(t: Rect) -> Cols {
    const GAP: f32 = 10.0;
    let x0 = t.x + 12.0;
    let w = t.w - 24.0;
    // narrow windows drop PID and Publisher
    let narrow = w < 720.0;
    let (status_w, pid_w, start_w) = (108.0, 64.0, 132.0);
    let fixed = 20.0 + status_w + start_w + if narrow { 0.0 } else { pid_w };
    let gaps = GAP * if narrow { 3.0 } else { 5.0 };
    let flex = (w - fixed - gaps).max(80.0);
    let (name_w, pub_w) = if narrow { (flex, 0.0) } else { (flex * 1.25 / 2.25, flex / 2.25) };
    let name_x = x0 + 20.0 + GAP;
    let status = name_x + name_w + GAP;
    let pid = (!narrow).then_some(status + status_w + GAP);
    let start_x = pid.map(|p| p + pid_w + GAP).unwrap_or(status + status_w + GAP);
    let publisher = (!narrow).then_some((start_x + start_w + GAP, pub_w));
    Cols { icon: x0, name: (name_x, name_w), status, pid, start: (start_x, start_w), publisher }
}

/// Parts of the header summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seg {
    /// a count (bold, mono)
    Num,
    Dim,
    /// the Automatic-not-running count
    Warn,
}

/// The header summary from longest to shortest. Every form is made of whole words: a form that
/// does not fit is replaced by the next, never cut in the middle of a word.
fn summary_forms(n_all: usize, n_run: usize, n_att: usize) -> Vec<Vec<(String, Seg)>> {
    let seg = |s: String, k: Seg| (s, k);
    let (a, r) = (n_all.to_string(), n_run.to_string());
    let mut out = Vec::new();
    if n_att > 0 {
        out.push(vec![seg(a.clone(), Seg::Num), seg(" services · ".into(), Seg::Dim), seg(r.clone(), Seg::Num), seg(" running".into(), Seg::Dim), seg(" · ".into(), Seg::Dim), seg(format!("{n_att} automatic not running"), Seg::Warn)]);
    }
    out.push(vec![seg(a.clone(), Seg::Num), seg(" services · ".into(), Seg::Dim), seg(r.clone(), Seg::Num), seg(" running".into(), Seg::Dim)]);
    out.push(vec![seg(a.clone(), Seg::Num), seg(" · ".into(), Seg::Dim), seg(r.clone(), Seg::Num), seg(" running".into(), Seg::Dim)]);
    out.push(vec![seg(a.clone(), Seg::Num), seg(" · ".into(), Seg::Dim), seg(r, Seg::Num)]);
    out.push(vec![seg(a, Seg::Num)]);
    out
}

/// The first (longest) form whose measured width fits `room`; None when not even the last does.
fn pick_summary<'a>(forms: &'a [Vec<(String, Seg)>], room: f32, width: &mut dyn FnMut(&str, Seg) -> f32) -> Option<&'a [(String, Seg)]> {
    forms.iter().find(|f| f.iter().map(|(s, k)| width(s, *k)).sum::<f32>() <= room).map(|f| f.as_slice())
}

pub fn paint(app: &mut App, g: &mut Gfx, body: Rect) {
    // a scroll bar drag holds the order like a menu: rows must not shift under the thumb
    let hold = app.menu_open || matches!(app.down, Some(Hit::Bar(_)));
    app.services.refresh(hold);
    let area = body.inset(12.0, 10.0);
    let q = app.search.to_lowercase();
    let rows = app.services.rows.clone();
    let cur = app.services.chip;

    // counts for the header and chips; groups as indices (no per-frame clone of the rows)
    let (mut n_run, mut n_stop, mut n_nonms, mut n_att) = (0, 0, 0, 0);
    let mut groups: [Vec<usize>; 3] = Default::default();
    for (i, r) in rows.iter().enumerate() {
        if r.state == SERVICE_STOPPED.0 {
            n_stop += 1;
        } else {
            n_run += 1;
        }
        if !is_microsoft(r) {
            n_nonms += 1;
        }
        if attention(r) {
            n_att += 1;
        }
        if chip_keeps(cur, r) && matches(r, &q) {
            groups[group_of(r)].push(i);
        }
    }

    // ---- toolbar: title + summary, actions for the selection, filter chips
    let tb = Rect::new(area.x, area.y, area.w, 30.0);
    let wd = &mut app.services.widths;
    let counts = [rows.len(), n_run, n_stop, n_nonms];
    let sel = app.services.selected.as_ref().and_then(|n| rows.iter().find(|r| &r.name == n));
    let sel_t = sel.map(target_of);
    let q = quick(sel_t.as_ref());

    // what fits right of the title: buttons first, then the startup segments, header text, chips last
    let mx = tb.x + 84.0;
    // The button slots are reserved even with nothing selected, so the startup control and the
    // chips do not shift when a row is picked (the buttons themselves only paint with a selection).
    let defs: Vec<(&str, usize, bool)> = match sel {
        Some(s) => {
            let run = s.state == SERVICE_RUNNING.0;
            // a Disabled service cannot be started: say so by being off, like the menu
            let stopped = s.state == SERVICE_STOPPED.0 && s.start != 4;
            vec![("Restart", 2, run && s.can_stop), ("Stop", 1, run && s.can_stop), ("Start", 0, stopped)]
        }
        None => vec![("Restart", 2, false), ("Stop", 1, false), ("Start", 0, false)],
    };
    let btn_ws: Vec<f32> = defs.iter().map(|d| wd.get(g, d.0, f(12.0)) + 24.0).collect();
    let btns = btn_ws.iter().sum::<f32>() + 6.0 * btn_ws.len() as f32 + 8.0;
    let seg_w = |wd: &mut Widths, g: &mut Gfx, short: bool| QUICK_ORDER.iter().map(|k| wd.get(g, seg_label(*k, short), f(11.5)) + 18.0).sum::<f32>() + 4.0 + 10.0;
    let (seg_full, seg_short) = (seg_w(wd, g, false), seg_w(wd, g, true));
    let chips_w: f32 = (0..CHIPS.len()).map(|i| chip_w(wd, g, CHIPS[i], Some(counts[i])) + 4.0).sum::<f32>() + 10.0;
    let plan = toolbar_plan(tb.r() - mx, btns, seg_full, seg_short, chips_w);

    let mut right = tb.r();
    if plan.chips {
        for i in (0..CHIPS.len()).rev() {
            let w = chip_w(wd, g, CHIPS[i], Some(counts[i]));
            right -= w;
            let r = chip(wd, g, right, tb.y + 3.0, CHIPS[i], Some(counts[i]), cur == i, app.hover == Some(Hit::SvcChip(i)));
            app.hits.push((r, Hit::SvcChip(i)));
            right -= 4.0;
        }
        right -= 10.0;
    }
    for (d, w) in defs.iter().zip(&btn_ws) {
        let (label, id, on) = *d;
        let r = Rect::new(right - w, tb.y + 3.0, *w, 24.0);
        right = r.x - 6.0;
        if sel.is_some() {
            button(g, r, label, app.hover == Some(Hit::SvcBtn(id)), on);
            if on {
                app.hits.push((r, Hit::SvcBtn(id)));
            }
        }
    }
    right -= 8.0;
    if plan.seg {
        let total = seg_w(wd, g, plan.short) - 10.0;
        let sr = Rect::new(right - total, tb.y + 3.0, total, 24.0);
        right = sr.x - 10.0;
        g.round(sr, 12.0, BG);
        g.round_stroke(sr, 12.0, LN, 1.0);
        let dim = q.off.is_some();
        let mut x = sr.x + 2.0;
        for (i, k) in QUICK_ORDER.iter().enumerate() {
            let label = seg_label(*k, plan.short);
            let w = wd.get(g, label, f(11.5)) + 18.0;
            let b = Rect::new(x, sr.y + 1.0, w, 22.0);
            x += w;
            let on = q.cur == Some(*k);
            let hov = app.hover == Some(Hit::SvcKind(i));
            if on {
                g.round(b, 11.0, if dim { 0x1c222b } else { PAN3 });
                if !dim {
                    g.round_stroke(b, 11.0, ACC_LN, 1.0);
                }
                g.circle(b.x + 9.0, b.y + b.h / 2.0, 2.5, if dim { TX4 } else { ACC });
            } else if hov && !dim {
                g.round(b, 11.0, PAN2);
            }
            let tc = if dim { if on { TX3 } else { TX4 } } else if on { TX1 } else if hov { TX1 } else { TX2 };
            g.text(label, Rect::new(b.x + if on { 8.0 } else { 0.0 }, b.y, b.w - if on { 8.0 } else { 0.0 }, b.h), f(11.5).c(), tc);
            // off segments keep a hit so the hover help can say why
            app.hits.push((b, Hit::SvcKind(i)));
            if hov && app.down.is_none() {
                let mut paras = vec![k.label().to_string(), kind_tip(*k).to_string()];
                match q.off {
                    Some(off) => paras.push(off.reason()),
                    None => {
                        paras.push(format!("Applies on click to the selected service. Alt+{}", i + 1));
                        if *k == StartKind::Disabled && sel.is_some_and(|s| s.state != SERVICE_STOPPED.0) {
                            paras.push("It is running now: it stays running. Undo is offered.".to_string());
                        }
                    }
                }
                app.tip = Some(Tip { anchor: b, paras });
            }
        }
    }

    g.text("SERVICES", Rect::new(tb.x + 2.0, tb.y, 76.0, tb.h), f(11.0).b(), TX2);
    let room = right - mx;
    let msg = last_message();
    let offer = undo_offer(Instant::now());
    if let Some((_, left)) = offer {
        app.timer_ms = Some(left.as_millis() as u32 + 60);
    }
    if !msg.is_empty() {
        let risky = offer.is_some_and(|(r, _)| r);
        let tc = if risky { WARN_TX } else { TX2 };
        if offer.is_some() && room > 140.0 {
            // the line, then Undo right after it (the line is cut to leave Undo room)
            let uw = wd.get(g, "Undo", f(12.0).b()) + 12.0;
            let mw = wd.get(g, &msg, f(12.0)).min(room - uw - 8.0);
            g.text(&msg, Rect::new(mx, tb.y, mw.max(0.0) + 1.0, tb.h), f(12.0), tc);
            let ur = Rect::new(mx + mw + 8.0, tb.y + 3.0, uw, 24.0);
            if app.hover == Some(Hit::SvcUndo) {
                g.round(ur, 5.0, ACC_BG);
            }
            g.text("Undo", ur, f(12.0).b().c(), ACC);
            app.hits.push((ur, Hit::SvcUndo));
        } else {
            g.text(&msg, Rect::new(mx, tb.y, room.max(0.0), tb.h), f(12.0), tc);
        }
    } else if room > 60.0 {
        // "354 services · 167 running · 1 automatic not running", shorter forms when it does not fit
        let forms = summary_forms(rows.len(), n_run, n_att);
        let font = |k: Seg| match k {
            Seg::Num => f(12.0).b().m(),
            Seg::Dim | Seg::Warn => f(12.0),
        };
        let mut width = |s: &str, k: Seg| wd.get(g, s, font(k));
        if let Some(form) = pick_summary(&forms, room, &mut width) {
            let mut x = mx;
            for (s, k) in form {
                let w = wd.get(g, s, font(*k));
                g.text(s, Rect::new(x, tb.y, w + 1.0, tb.h), font(*k), match k {
                    Seg::Num => TX1,
                    Seg::Dim => TX3,
                    Seg::Warn => WARN,
                });
                x += w;
            }
        }
    }

    // ---- table panel
    let t = Rect::new(area.x, tb.b() + 8.0, area.w, area.b() - tb.b() - 8.0);
    if t.h < 60.0 {
        return;
    }
    panel(g, t);
    // the scroll bar takes its gutter from the columns only while the list overflows (the view is
    // `t.h - 33` tall: 1 border, the 28 header, the 4 px rounded-corner margin)
    let list_h: f32 = groups.iter().enumerate().filter(|(_, l)| !l.is_empty()).map(|(gi, l)| BAND_H + if app.services.collapsed[gi] { 0.0 } else { l.len() as f32 * ROW_H }).sum();
    let gut = super::nav::gutter(list_h, t.h - 33.0);
    let cols = columns(Rect::new(t.x, t.y, t.w - gut, t.h));
    let hr = Rect::new(t.x + 1.0, t.y + 1.0, t.w - 2.0, 28.0);
    g.fill(Rect::new(hr.x, hr.b() - 1.0, hr.w, 1.0), SEP);
    let hf = f(10.5).b();
    g.text("NAME", Rect::new(cols.name.0, hr.y, cols.name.1, hr.h), hf, TX2);
    g.text("STATUS", Rect::new(cols.status, hr.y, 108.0, hr.h), hf, TX4);
    if let Some(px) = cols.pid {
        g.text("PID", Rect::new(px, hr.y, 64.0, hr.h), hf.r(), TX4);
    }
    g.text("START TYPE", Rect::new(cols.start.0, hr.y, cols.start.1, hr.h), hf, TX4);
    if let Some((px, pw)) = cols.publisher {
        g.text("PUBLISHER", Rect::new(px, hr.y, pw, hr.h), hf, TX4);
    }

    // the rounded bottom corners stay clear of row fills
    let view = Rect::new(t.x + 1.0, hr.b(), t.w - 2.0, t.b() - hr.b() - 4.0);
    let mut items = Vec::new();
    let mut n = 0usize;
    for (gi, list) in groups.iter().enumerate() {
        if list.is_empty() {
            continue;
        }
        items.push(Item::Band(gi, list.len()));
        if app.services.collapsed[gi] {
            continue;
        }
        for &ri in list {
            items.push(Item::Row(ri, n % 2 == 1));
            n += 1;
        }
    }
    // keyboard: record the row order; a key's new selection scrolls into view
    let sel_name = app.services.selected.clone();
    app.services.nav.begin(super::nav::page_rows(view.h, ROW_H));
    let (mut top, mut sel_top) = (0.0f32, None);
    for it in &items {
        match it {
            Item::Band(..) => top += BAND_H,
            Item::Row(ri, _) => {
                let s = sel_name.as_deref() == Some(rows[*ri].name.as_str());
                if s {
                    sel_top = Some(top);
                }
                app.services.nav.push(*ri, s);
                top += ROW_H;
            }
        }
    }
    if let Some(t) = sel_top.filter(|_| app.services.nav.take_reveal()) {
        app.services.scroll = super::nav::reveal(app.services.scroll, t, ROW_H, view.h);
    }
    if items.is_empty() {
        g.text(if rows.is_empty() { "Reading services…" } else { "No services match." }, Rect::new(view.x, view.y + 20.0, view.w, 20.0), f(12.0).c(), TX3);
        return;
    }
    let total: f32 = items.iter().map(|it| if matches!(it, Item::Band(..)) { BAND_H } else { ROW_H }).sum();
    app.services.scroll = app.services.scroll.clamp(0.0, (total - view.h).max(0.0));
    g.clip(view);
    let mut y = view.y - app.services.scroll;
    let mut first = true;
    for it in items {
        let h = if matches!(it, Item::Band(..)) { BAND_H } else { ROW_H };
        if y + h >= view.y && y <= view.b() {
            let rr = Rect::new(view.x, y, view.w - gut, h);
            let (y0, y1) = (rr.y.max(view.y), rr.b().min(view.b()));
            let vis = Rect::new(rr.x, y0, rr.w, (y1 - y0).max(0.0));
            match it {
                Item::Band(gi, count) => {
                    band(app, g, rr, &cols, gi, count, first);
                    app.hits.push((vis, Hit::SvcGroup(gi)));
                }
                Item::Row(ri, odd) => {
                    row(app, g, rr, &cols, &rows[ri], ri, odd);
                    app.hits.push((vis, Hit::Svc(ri)));
                }
            }
        }
        first = false;
        y += h;
    }
    g.unclip();
    debug_assert_eq!(gut, super::nav::gutter(total, view.h), "the gutter was sized from the same content");
    let sc = app.services.scroll;
    super::nav::paint_bar(app, g, super::nav::BAR_SERVICES, view, total, sc);
}

fn band(app: &mut App, g: &mut Gfx, r: Rect, cols: &Cols, gi: usize, count: usize, first: bool) {
    let hov = app.hover == Some(Hit::SvcGroup(gi));
    g.fill(r, if hov { PAN3 } else { PAN2 });
    if !first {
        g.fill(Rect::new(r.x, r.y, r.w, 1.0), LN);
    }
    g.fill(Rect::new(r.x, r.b() - 1.0, r.w, 1.0), SEP);
    let open = !app.services.collapsed[gi];
    let mut x = cols.icon;
    g.text(if open { "\u{E70D}" } else { "\u{E76C}" }, Rect::new(x, r.y, 12.0, r.h), f(8.5).icon().c(), TX3);
    x += 18.0;
    let warn = gi == 0;
    if warn {
        g.text("\u{E7BA}", Rect::new(x, r.y, 14.0, r.h), f(11.0).icon().c(), WARN);
        x += 20.0;
    }
    let label = GROUPS[gi];
    let lw = app.services.widths.get(g, label, f(12.0).b());
    g.text(label, Rect::new(x, r.y, lw + 2.0, r.h), f(12.0).b(), if warn { WARN_TX } else { TX1 });
    x += lw + 8.0;
    g.text(&count.to_string(), Rect::new(x, r.y, 60.0, r.h), f(11.0).m(), TX3);
    let hint = if warn { "usually worth a look" } else if !open { "collapsed" } else { "" };
    if !hint.is_empty() {
        g.text(hint, Rect::new(r.r() - 212.0, r.y, 200.0, r.h), f(11.5).r(), TX3);
    }
}

fn row(app: &mut App, g: &mut Gfx, rr: Rect, cols: &Cols, r: &Row, ri: usize, odd: bool) {
    let is_sel = app.services.selected.as_deref() == Some(r.name.as_str());
    let bg = if is_sel {
        ACC_BG
    } else if app.hover == Some(Hit::Svc(ri)) {
        PAN2
    } else if odd {
        ZEBRA
    } else {
        PAN
    };
    if bg != PAN {
        g.fill(rr, bg);
    }
    if is_sel {
        g.fill(Rect::new(rr.x, rr.y, 2.0, rr.h), ACC);
    }
    // meta text on the selection fill needs TX2 for contrast
    let meta = if is_sel { TX2 } else { TX3 };
    let stopped = r.state == SERVICE_STOPPED.0;

    let ir = Rect::new(cols.icon + 2.0, rr.y + 5.5, 16.0, 16.0);
    if !super::icons::draw(&mut app.icons, g, &r.path, r.pid, ir) {
        g.round(Rect::new(ir.x + 2.0, ir.y + 2.0, 12.0, 12.0), 3.0, PAN3);
    }

    // display name, then the service key in mono when it differs
    let (nx, nw) = cols.name;
    let wd = &mut app.services.widths;
    let dw = wd.get(g, &r.display, f(12.5)).min(nw);
    g.text(&r.display, Rect::new(nx, rr.y, dw + 1.0, rr.h), f(12.5), if stopped && !is_sel { TX2 } else { TX1 });
    if r.name != r.display && nw - dw > 40.0 {
        g.text(&r.name, Rect::new(nx + dw + 8.0, rr.y, nw - dw - 8.0, rr.h), f(11.0).m(), meta);
    }

    // status pill
    let (label, fill, border, dot, tc) = pill_style(r);
    let tw = wd.get(g, label, f(11.5));
    let pr = Rect::new(cols.status, rr.y + 4.0, 7.0 + 6.0 + 5.0 + tw + 8.0, 19.0);
    if fill != 0 {
        g.round(pr, 9.5, fill);
    }
    if border != 0 {
        g.round_stroke(pr, 9.5, border, 1.0);
    }
    let (cx, cy) = (pr.x + 10.0, pr.y + pr.h / 2.0);
    if fill == 0 {
        g.circle(cx, cy, 3.5, dot);
        g.circle(cx, cy, 2.5, bg);
    } else {
        g.circle(cx, cy, 3.0, dot);
    }
    g.text(label, Rect::new(pr.x + 18.0, pr.y, tw + 2.0, pr.h), f(11.5), tc);

    if let Some(px) = cols.pid {
        let pr = Rect::new(px, rr.y, 64.0, rr.h);
        if r.pid != 0 {
            g.text(&r.pid.to_string(), pr, f(11.5).m().r(), meta);
        } else {
            g.text("—", pr, f(11.5).m().r(), TX4);
        }
    }
    let disabled = r.start == 4;
    g.text(start_label(r), Rect::new(cols.start.0, rr.y, cols.start.1, rr.h), f(12.0), if disabled { if is_sel { TX3 } else { TX4 } } else { TX2 });
    if let Some((px, pw)) = cols.publisher {
        g.text(&r.publisher, Rect::new(px, rr.y, pw - 4.0, rr.h), f(12.0), meta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, state: u32, start: u32) -> Row {
        Row {
            name: name.into(),
            display: format!("{name} display"),
            state,
            pid: if state == SERVICE_RUNNING.0 { 100 } else { 0 },
            start,
            can_stop: true,
            path: format!("C:\\x\\{name}.exe"),
            ..Default::default()
        }
    }

    fn texts(m: &[procmenu::Entry]) -> Vec<(String, bool, bool)> {
        let mut out = Vec::new();
        for e in m {
            match e {
                // the right-aligned accelerator hint ("\tAlt+1") is not part of the label
                procmenu::Entry::Item { text, checked, enabled, .. } => out.push((text.split('\t').next().unwrap_or("").to_string(), *checked, *enabled)),
                procmenu::Entry::Sub { items, .. } => out.extend(texts(items)),
                procmenu::Entry::Sep => {}
            }
        }
        out
    }
    fn find(m: &[procmenu::Entry], label: &str) -> (bool, bool) {
        let t = texts(m);
        let (_, c, e) = t.iter().find(|(t, ..)| t == label).unwrap_or_else(|| panic!("no {label} in {t:?}"));
        (*c, *e)
    }

    #[test]
    fn menu_enabled_states_match_the_service() {
        let run = target_of(&row("A", SERVICE_RUNNING.0, 2));
        let m = menu(&run, true);
        assert_eq!(find(&m, "Start"), (false, false), "running: Start is off");
        assert!(find(&m, "Stop").1 && find(&m, "Restart").1);
        let stopped = target_of(&row("B", SERVICE_STOPPED.0, 3));
        let m = menu(&stopped, true);
        assert!(find(&m, "Start").1);
        assert!(!find(&m, "Stop").1 && !find(&m, "Restart").1, "stopped: nothing to stop or restart");
        // a disabled service cannot be started: the menu says so by being off
        let dis = target_of(&row("C", SERVICE_STOPPED.0, 4));
        assert!(!find(&menu(&dis, true), "Start").1);
        // a service that does not accept Stop is never offered Stop / Restart
        let mut nostop = target_of(&row("D", SERVICE_RUNNING.0, 3));
        nostop.can_stop = false;
        let m = menu(&nostop, true);
        assert!(!find(&m, "Stop").1 && !find(&m, "Restart").1);
        // pending states offer no action
        let pend = target_of(&row("E", SERVICE_STOP_PENDING.0, 3));
        let m = menu(&pend, true);
        assert!(!find(&m, "Start").1 && !find(&m, "Stop").1 && !find(&m, "Restart").1);
        assert!(find(&m, "Properties…").1 && find(&m, "Copy service name").1, "always honest and available");
    }

    #[test]
    fn menu_startup_type_marks_the_current_one() {
        for (start, delayed, label) in [(2, false, "Automatic"), (2, true, "Automatic (delayed start)"), (3, false, "Manual"), (4, false, "Disabled")] {
            let mut t = target_of(&row("A", SERVICE_STOPPED.0, start));
            t.delayed = delayed;
            let m = menu(&t, true);
            let marked: Vec<_> = texts(&m).into_iter().filter(|(l, c, _)| *c && StartKind::ALL.iter().any(|k| k.label() == l)).collect();
            assert_eq!(marked.len(), 1, "{label}: exactly one type is checked");
            assert_eq!(marked[0].0, label);
            assert!(marked[0].2, "changeable");
        }
        // start type not known yet (list still loading) or a driver type: all four off
        for start in [u32::MAX, 0, 1] {
            let m = menu(&target_of(&row("A", SERVICE_STOPPED.0, start)), true);
            for k in StartKind::ALL {
                assert_eq!(find(&m, k.label()), (false, false), "start {start}");
            }
        }
    }

    #[test]
    fn menu_admin_item_only_when_not_elevated() {
        let t = target_of(&row("A", SERVICE_RUNNING.0, 2));
        assert!(texts(&menu(&t, false)).iter().any(|(l, ..)| l.starts_with("Restart Hayate as administrator")));
        assert!(!texts(&menu(&t, true)).iter().any(|(l, ..)| l.starts_with("Restart Hayate as administrator")));
    }

    #[test]
    fn menu_for_a_busy_service_blocks_actions_and_offers_to_stop_waiting() {
        let t = target_of(&row("BusyMenuSvc", SERVICE_RUNNING.0, 3));
        // simulate an op in flight on this key through the real registry
        let g = svcctl::hold_for_test(&t.key(), svcctl::Work::Op(Op::Restart));
        let m = menu(&t, true);
        assert!(!find(&m, "Stop").1 && !find(&m, "Restart").1 && !find(&m, "Start").1);
        assert!(find(&m, "Stop waiting for restart").1);
        for k in StartKind::ALL {
            assert!(!find(&m, k.label()).1, "no config change while an op is in flight");
        }
        drop(g);
        assert!(!texts(&menu(&t, true)).iter().any(|(l, ..)| l.starts_with("Stop waiting")));
    }

    #[test]
    fn a_row_without_a_program_path_offers_no_action_and_says_why() {
        let mut r = row("NoPathSvc", SERVICE_STOPPED.0, 3);
        r.path.clear();
        let t = target_of(&r);
        let m = menu(&t, true);
        assert!(!find(&m, "Start").1 && !find(&m, "Stop").1 && !find(&m, "Restart").1);
        for k in StartKind::ALL {
            assert!(!find(&m, k.label()).1, "{}: no startup change without a baseline", k.label());
        }
        assert_eq!(find(&m, "Actions off: the list has no program path for it yet"), (false, false));
        assert!(find(&m, "Properties…").1 && find(&m, "Copy service name").1, "still honest and available (read-only)");
        // and run() refuses at once with the reason, instead of acting on whatever answers to the name
        let _l = MSG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        run(t, Op::Start);
        assert!(last_message().contains("no program path"), "{}", last_message());
    }

    #[test]
    fn menu_for_a_service_whose_startup_is_changing_blocks_everything_and_says_why() {
        let t = target_of(&row("StartupBusyMenuSvc", SERVICE_STOPPED.0, 3));
        let g = svcctl::hold_for_test(&t.key(), svcctl::Work::Startup);
        let m = menu(&t, true);
        assert!(!find(&m, "Start").1 && !find(&m, "Stop").1 && !find(&m, "Restart").1);
        for k in StartKind::ALL {
            assert!(!find(&m, k.label()).1, "no second startup change while one is in flight");
        }
        assert_eq!(find(&m, "Changing its startup type…"), (false, false), "shown, and off");
        assert!(!texts(&m).iter().any(|(l, ..)| l.starts_with("Stop waiting")), "a startup change has no wait to abandon");
        drop(g);
        assert!(find(&menu(&t, true), "Start").1, "idle again after the guard drops");
    }

    #[test]
    fn right_click_captures_the_service_name_not_the_row() {
        let mut app = App::new();
        app.tab = Tab::Services;
        let rows = vec![row("Alpha", SERVICE_RUNNING.0, 2), row("Beta", SERVICE_STOPPED.0, 3), row("Gamma", SERVICE_RUNNING.0, 4)];
        app.services.rows = Arc::new(rows);
        app.hits.clear();
        app.hits.push((Rect::new(0.0, 100.0, 500.0, 27.0), Hit::Svc(1)));
        app.hits.push((Rect::new(0.0, 130.0, 500.0, 27.0), Hit::Svc(77))); // a stale index
        let Action::SvcMenu(_, _, t) = app.right_click(10.0, 110.0) else { panic!("menu expected") };
        assert_eq!(t.name, "Beta");
        assert_eq!((t.state, t.start, t.pid), (SERVICE_STOPPED.0, 3, 0));
        assert_eq!(app.services.selected.as_deref(), Some("Beta"), "the row under the pointer is selected");
        // the list is re-sorted while the menu is up: the captured target does not move
        app.services.rows = Arc::new(vec![row("Gamma", SERVICE_RUNNING.0, 4), row("Beta", SERVICE_RUNNING.0, 3), row("Alpha", SERVICE_RUNNING.0, 2)]);
        assert_eq!(t.name, "Beta");
        assert_eq!(t.key(), "beta");
        // a stale index opens nothing
        assert!(matches!(app.right_click(10.0, 140.0), Action::None));
    }

    fn changed_to(start: u32, delayed: bool, state: u32) -> svcctl::StartupDone {
        svcctl::StartupDone {
            after: svcctl::Config { start, delayed: Some(delayed), status: svcctl::Status { state, pid: if state == svcctl::st::RUNNING { 100 } else { 0 }, controls: 0 }, ..Default::default() },
            note: "startup type is now x".into(),
        }
    }

    #[test]
    fn quick_order_is_the_four_types_and_ids_stay_in_all_order() {
        let mut all: Vec<_> = QUICK_ORDER.iter().map(|k| kind_id(*k)).collect();
        assert_eq!(QUICK_ORDER.iter().map(|k| k.label()).collect::<Vec<_>>(), ["Disabled", "Manual", "Automatic", "Automatic (delayed start)"]);
        all.sort();
        assert_eq!(all, [0, 1, 2, 3], "every type once, ids are positions in StartKind::ALL");
        for k in StartKind::ALL {
            assert_eq!(StartKind::ALL[kind_id(k)], k);
            assert!(!seg_label(k, false).is_empty() && !seg_label(k, true).is_empty());
            assert!(!kind_tip(k).is_empty());
        }
        assert_eq!((seg_label(StartKind::Disabled, false), seg_label(StartKind::AutomaticDelayed, false)), ("Disabled", "Auto (delayed)"));
    }

    #[test]
    fn quick_state_current_type_and_reasons() {
        // nothing selected: no current type, off with the reason
        assert_eq!(quick_with(None, None), Quick { cur: None, off: Some(Off::NoSelection) });
        // current type incl. delayed
        for (start, delayed, want) in [(2, false, StartKind::Automatic), (2, true, StartKind::AutomaticDelayed), (3, false, StartKind::Manual), (4, false, StartKind::Disabled)] {
            let mut t = target_of(&row("Q", SERVICE_STOPPED.0, start));
            t.delayed = delayed;
            assert_eq!(quick_with(Some(&t), None), Quick { cur: Some(want), off: None }, "start {start} delayed {delayed}");
        }
        // a running Disabled service is still just Disabled, and changeable
        assert_eq!(quick_with(Some(&target_of(&row("Q", SERVICE_RUNNING.0, 4))), None).off, None);
        // busy: the current type stays shown, controls off
        let t = target_of(&row("Q", SERVICE_RUNNING.0, 3));
        assert_eq!(quick_with(Some(&t), Some(svcctl::Work::Startup)), Quick { cur: Some(StartKind::Manual), off: Some(Off::Busy(svcctl::Work::Startup)) });
        assert_eq!(quick_with(Some(&t), Some(svcctl::Work::Op(Op::Stop))).off, Some(Off::Busy(svcctl::Work::Op(Op::Stop))));
        // no path / still loading / driver start types
        let mut np = row("Q", SERVICE_STOPPED.0, 3);
        np.path.clear();
        assert_eq!(quick_with(Some(&target_of(&np)), None).off, Some(Off::NoPath));
        assert_eq!(quick_with(Some(&target_of(&row("Q", SERVICE_STOPPED.0, u32::MAX))), None), Quick { cur: None, off: Some(Off::Loading) });
        for s in [0, 1] {
            assert_eq!(quick_with(Some(&target_of(&row("Q", SERVICE_STOPPED.0, s))), None), Quick { cur: None, off: Some(Off::Driver) });
        }
        assert_eq!(quick_with(Some(&target_of(&row("Q", SERVICE_STOPPED.0, 7))), None).off, Some(Off::NoKind));
        // every reason has words (the tooltip shows it)
        for o in [Off::NoSelection, Off::Busy(svcctl::Work::Startup), Off::Loading, Off::NoPath, Off::Driver, Off::NoKind] {
            assert!(o.reason().len() > 20, "{o:?}");
        }
        // the real registry feeds `quick`
        let t = target_of(&row("QuickBusySvc", SERVICE_STOPPED.0, 3));
        assert_eq!(quick(Some(&t)).off, None);
        let g = svcctl::hold_for_test(&t.key(), svcctl::Work::Startup);
        assert_eq!(quick(Some(&t)).off, Some(Off::Busy(svcctl::Work::Startup)));
        drop(g);
    }

    #[test]
    fn menu_has_one_flat_startup_section_and_no_submenu() {
        let t = target_of(&row("A", SERVICE_RUNNING.0, 2));
        let m = menu(&t, true);
        assert!(!m.iter().any(|e| matches!(e, procmenu::Entry::Sub { .. })), "one way to do it: no Startup type submenu");
        // the four types, in quick order, right after the caption, with their Alt+N hints
        let at = m.iter().position(|e| matches!(e, procmenu::Entry::Item { id, .. } if *id == ID_HEAD)).expect("caption");
        for (n, k) in QUICK_ORDER.iter().enumerate() {
            let procmenu::Entry::Item { id, text, checked, enabled } = &m[at + 1 + n] else { panic!("item expected") };
            assert_eq!(text, &format!("{}\tAlt+{}", k.label(), n + 1));
            assert_eq!(*id, ID_KIND + kind_id(*k));
            assert_eq!((*checked, *enabled), (*k == StartKind::Automatic, true));
        }
        // a running service that is not Disabled: the note that Disabled does not stop it
        assert!(texts(&m).iter().any(|(l, ..)| l == "Disabled does not stop a running service"));
        // already Disabled: no such note; stopped: none either
        assert!(!texts(&menu(&target_of(&row("A", SERVICE_RUNNING.0, 4)), true)).iter().any(|(l, ..)| l.starts_with("Disabled does not")));
        assert!(!texts(&menu(&target_of(&row("A", SERVICE_STOPPED.0, 3)), true)).iter().any(|(l, ..)| l.starts_with("Disabled does not")));
        // a driver start type: all four off, with the reason
        let m = menu(&target_of(&row("A", SERVICE_RUNNING.0, 1)), true);
        assert_eq!(find(&m, "A driver start type: not changed here"), (false, false));
        for k in StartKind::ALL {
            assert_eq!(find(&m, k.label()), (false, false));
        }
    }

    #[test]
    fn toolbar_plan_drops_chips_then_words_then_the_control_never_the_buttons() {
        // wide: everything
        assert_eq!(toolbar_plan(1100.0, 200.0, 280.0, 200.0, 330.0), TbPlan { chips: true, short: false, seg: true });
        // a bit less: short words, chips kept
        assert_eq!(toolbar_plan(900.0, 200.0, 280.0, 200.0, 330.0), TbPlan { chips: true, short: true, seg: true });
        // medium: chips go next, the words come back
        assert_eq!(toolbar_plan(700.0, 200.0, 280.0, 200.0, 330.0), TbPlan { chips: false, short: false, seg: true });
        // 640 px window (room ~486): short labels, header text may be squeezed but nothing overlaps
        let p = toolbar_plan(486.0, 199.0, 290.0, 214.0, 340.0);
        assert_eq!(p, TbPlan { chips: false, short: true, seg: true });
        // no buttons (nothing selected): more room
        assert!(toolbar_plan(486.0, 0.0, 290.0, 214.0, 340.0).seg);
        // too narrow for the short control beside the buttons: the control yields
        assert_eq!(toolbar_plan(300.0, 199.0, 290.0, 214.0, 340.0), TbPlan { chips: false, short: true, seg: false });
    }

    #[test]
    fn undo_is_offered_only_for_a_confirmed_change_and_is_risky_only_for_running_disabled() {
        let t = target_of(&row("U", SERVICE_RUNNING.0, 2));
        // Automatic -> Disabled, read back as Disabled, still running: risky
        let (after, risky) = undo_after(&t, StartKind::Automatic, StartKind::Disabled, &changed_to(4, false, svcctl::st::RUNNING)).expect("undo");
        assert!(risky);
        assert_eq!((after.start, after.kind()), (4, Some(StartKind::Disabled)), "the undo's baseline is the service as it is now");
        assert_eq!(after.path, t.path, "same image: the identity check still applies");
        // stopped: not risky
        assert!(!undo_after(&t, StartKind::Automatic, StartKind::Disabled, &changed_to(4, false, svcctl::st::STOPPED)).unwrap().1);
        // other changes are never risky
        assert!(!undo_after(&t, StartKind::Automatic, StartKind::Manual, &changed_to(3, false, svcctl::st::RUNNING)).unwrap().1);
        // Automatic -> delayed: the delayed flag is part of the baseline
        let (a, _) = undo_after(&t, StartKind::Automatic, StartKind::AutomaticDelayed, &changed_to(2, true, svcctl::st::RUNNING)).unwrap();
        assert_eq!(a.kind(), Some(StartKind::AutomaticDelayed));
        // no change ("already"), or a read-back that is not what was asked: no Undo
        assert!(undo_after(&t, StartKind::Manual, StartKind::Manual, &changed_to(3, false, 1)).is_none());
        assert!(undo_after(&t, StartKind::Automatic, StartKind::Disabled, &changed_to(3, false, 1)).is_none());
    }

    #[test]
    fn undo_window_line_and_single_use() {
        let _l = MSG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t = target_of(&row("UndoSvc", SERVICE_STOPPED.0, 3));
        let done = changed_to(4, false, svcctl::st::STOPPED);
        let now = Instant::now();
        // time rules (pure)
        let u = Undo { t: t.clone(), back: StartKind::Manual, risky: false, seq: 5, at: now };
        assert!(undo_live(&u, 5, now));
        assert!(undo_live(&u, 5, now + Duration::from_secs(UNDO_SECS - 1)));
        assert!(!undo_live(&u, 5, now + Duration::from_secs(UNDO_SECS)), "plain: 8 s");
        assert!(!undo_live(&u, 6, now), "another line replaced it");
        let risky = Undo { risky: true, ..u.clone() };
        assert!(undo_live(&risky, 5, now + Duration::from_secs(UNDO_SECS + 1)), "risky: longer");
        assert!(!undo_live(&risky, 5, now + Duration::from_secs(UNDO_RISKY_SECS)));

        // the real flow: result line + Undo; a second take finds nothing
        finish_startup(&t, StartKind::Manual, StartKind::Disabled, "Undo Svc", Ok(done), true);
        assert!(last_message().starts_with("Undo Svc: "));
        assert!(undo_offer(Instant::now()).is_some());
        let u = take_undo(Instant::now()).expect("offered");
        assert_eq!((u.back, u.t.kind()), (StartKind::Manual, Some(StartKind::Disabled)));
        assert!(take_undo(Instant::now()).is_none(), "one use");
        assert!(undo_offer(Instant::now()).is_none());

        // any newer header line retires the offer
        finish_startup(&t, StartKind::Manual, StartKind::Disabled, "Undo Svc", Ok(changed_to(4, false, 1)), true);
        assert!(undo_offer(Instant::now()).is_some());
        set_msg("something else".into());
        assert!(undo_offer(Instant::now()).is_none());
        assert_eq!(last_message(), "something else", "a replaced line is not faded away");

        // a failure, "already", and an Undo itself offer no Undo
        finish_startup(&t, StartKind::Manual, StartKind::Disabled, "Undo Svc", Err("not changed: nope".into()), true);
        assert!(undo_offer(Instant::now()).is_none() && last_message().contains("not changed"));
        finish_startup(&t, StartKind::Manual, StartKind::Disabled, "Undo Svc", Ok(changed_to(4, false, 1)), false);
        assert!(undo_offer(Instant::now()).is_none(), "an Undo is never chained");

        // an unreplaced line whose window passed fades back to the summary
        finish_startup(&t, StartKind::Manual, StartKind::Disabled, "Undo Svc", Ok(changed_to(4, false, 1)), true);
        assert!(undo_offer(Instant::now() + Duration::from_secs(UNDO_SECS + 1)).is_none());
        assert_eq!(last_message(), "");
    }

    #[test]
    fn quick_set_refuses_with_the_reason_or_says_already_and_never_calls_the_scm() {
        let _l = MSG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // already that type: no change is started
        quick_set(target_of(&row("QuickSetSvc", SERVICE_STOPPED.0, 3)), StartKind::Manual);
        assert_eq!(last_message(), "QuickSetSvc display: already Manual");
        // no program path: the reason, nothing started
        let mut r = row("QuickSetSvc", SERVICE_STOPPED.0, 3);
        r.path.clear();
        quick_set(target_of(&r), StartKind::Disabled);
        assert!(last_message().contains("no program path"), "{}", last_message());
        // busy: the reason
        let t = target_of(&row("QuickSetBusy", SERVICE_STOPPED.0, 3));
        let g = svcctl::hold_for_test(&t.key(), svcctl::Work::Startup);
        quick_set(t, StartKind::Disabled);
        assert!(last_message().contains("changing its startup type"), "{}", last_message());
        drop(g);
    }

    #[test]
    fn alt_digit_is_only_for_the_services_tab_with_a_selection_and_no_text_or_panel_focus() {
        let mut app = App::new();
        app.services.rows = Arc::new(vec![row("AltSvc", SERVICE_STOPPED.0, 3)]);
        // wrong tab
        app.tab = Tab::Vitals;
        app.services.selected = Some("AltSvc".into());
        assert!(app.alt_digit(1, false).is_none());
        app.tab = Tab::Services;
        // nothing selected: the key is not ours
        app.services.selected = None;
        assert!(app.alt_digit(1, false).is_none());
        // search box focused: it keeps the keyboard
        app.services.selected = Some("AltSvc".into());
        app.search_focus = true;
        assert!(app.alt_digit(1, false).is_none());
        app.search_focus = false;
        // a menu open
        app.menu_open = true;
        assert!(app.alt_digit(1, false).is_none());
        app.menu_open = false;
        // out of range
        assert!(app.alt_digit(4, false).is_none());
        // held key: swallowed without a change
        assert!(matches!(app.alt_digit(1, true), Some(Action::None)));
        // a normal press on the already-current type: handled, says so, changes nothing
        let _l = MSG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(matches!(app.alt_digit(1, false), Some(Action::Repaint)));
        assert_eq!(last_message(), "AltSvc display: already Manual");
    }

    #[test]
    fn rows_hold_while_a_menu_is_open_and_thaw_after() {
        let cur = Arc::new(vec![row("A", SERVICE_RUNNING.0, 2), row("B", SERVICE_RUNNING.0, 2)]);
        let reordered = Arc::new(vec![row("B", SERVICE_RUNNING.0, 2), row("A", SERVICE_RUNNING.0, 2)]);
        assert!(adopt(&cur, Some(reordered.clone()), true).is_none(), "menu open: order frozen");
        let got = adopt(&cur, Some(reordered.clone()), false).unwrap();
        assert_eq!(got[0].name, "B", "menu closed: the new order lands");
        assert!(adopt(&cur, Some(cur.clone()), false).is_none(), "same list: no change");
        assert!(adopt(&cur, None, false).is_none());
    }

    fn test_ctx() -> crate::rules::protect::Ctx {
        use crate::rules::protect::{norm, Ctx, Refusal};
        Ctx {
            own_pid: 1,
            own_exe: String::new(),
            dirs: vec![(norm("C:\\Windows"), Refusal::WindowsFolder), (norm("C:\\ProgramData\\Microsoft\\Windows Defender"), Refusal::SecurityTool)],
        }
    }

    #[test]
    fn protected_by_name_user_prefix_and_defender_folder_only() {
        let ctx = test_ctx();
        let svchost = "C:\\Windows\\System32\\svchost.exe";
        // known protected names, any case
        for n in ["RpcSs", "WinDefend", "wscsvc", "SecurityHealthService", "Sense", "BFE"] {
            assert!(protected_by(&ctx, n, svchost), "{n}");
        }
        // per-user service instances carry a LUID suffix
        assert!(protected_by(&ctx, "PrintWorkflowUserSvc_fa729", svchost));
        // a Defender-folder program is protected even under an unknown name
        assert!(protected_by(&ctx, "SomeDefenderPart", "C:\\ProgramData\\Microsoft\\Windows Defender\\Platform\\4.18\\MsMpEng.exe"));
        // ordinary services stay changeable, including svchost-hosted ones and Windows-folder programs
        for n in ["Spooler", "BITS", "Themes", "wuauserv", "MyVendorSvc"] {
            assert!(!protected_by(&ctx, n, svchost), "{n}");
        }
        assert!(!protected_by(&ctx, "SomethingInWindows", "C:\\Windows\\System32\\foo.exe"));
        assert!(!protected_by(&ctx, "NoPathYet", ""), "no path, no name match: not protected");
    }

    #[test]
    fn protected_row_greys_every_quick_control_with_the_reason() {
        let mut r = row("RpcSs", SERVICE_RUNNING.0, 2);
        r.path = "C:\\Windows\\System32\\svchost.exe".into();
        let t = target_of(&r);
        let q = quick_with(Some(&t), None);
        assert_eq!(q.off, Some(Off::Protected));
        assert_eq!(q.cur, Some(StartKind::Automatic), "the current type is still shown");
        assert!(Off::Protected.reason().contains("Windows protects this service"));
        // menu: all four types off, and the note says why
        let m = menu(&t, true);
        for k in StartKind::ALL {
            assert!(!find(&m, k.label()).1, "{}", k.label());
        }
        assert_eq!(find(&m, "Windows protects this service: not changed here"), (false, false));
        // a normal row next to it is untouched
        let ok = target_of(&row("PlainSvc", SERVICE_RUNNING.0, 2));
        assert_eq!(quick_with(Some(&ok), None).off, None);
        assert!(find(&menu(&ok, true), "Manual").1);
        // Alt+1..4 / menu pick: refused with the reason, nothing sent
        let _l = MSG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        quick_set(t.clone(), StartKind::Manual);
        assert!(last_message().contains("Windows protects this service"), "{}", last_message());
        assert_eq!(svcctl::busy_work(&t.key()), None, "no guarded change was started");
    }

    #[test]
    fn a_refusal_while_elevated_greys_the_row_for_the_session() {
        let t = target_of(&row("RefusedByWindowsSvc", SERVICE_RUNNING.0, 3));
        assert!(!is_protected(&t));
        let denied = format!("not changed: {}", svcctl::ScmError::Denied.message());
        // unelevated: access denied is the normal answer for any service, so nothing is remembered
        assert!(!note_refusal(&t.key(), &denied, false));
        assert!(!is_protected(&t));
        // elevated, but some other failure: not a protection
        assert!(!note_refusal(&t.key(), "not changed: the service no longer exists", true));
        assert!(!is_protected(&t));
        // elevated and denied: grey from now on, marked once
        assert!(note_refusal(&t.key(), &denied, true));
        assert!(!note_refusal(&t.key(), &denied, true), "already marked");
        assert!(is_protected(&t));
        assert_eq!(quick_with(Some(&t), None).off, Some(Off::Protected));
        assert!(!find(&menu(&t, true), "Disabled").1);
        // another service is not affected
        assert!(!is_protected(&target_of(&row("OtherSvc", SERVICE_RUNNING.0, 3))));
    }

    #[test]
    fn undo_of_a_refused_change_is_greyed_too() {
        // the Undo is a change back; it must not be sent for a service that is now known refused
        let t = target_of(&row("UndoRefusedSvc", SERVICE_RUNNING.0, 4));
        let denied = format!("not changed: {}", svcctl::ScmError::Denied.message());
        assert!(note_refusal(&t.key(), &denied, true));
        let _l = MSG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        change_startup(t.clone(), StartKind::Manual, false);
        assert!(last_message().contains("Windows protects this service"), "{}", last_message());
        assert_eq!(svcctl::busy_work(&t.key()), None, "nothing was sent to Windows");
        // and a refused result never offers an Undo
        *UNDO.lock().unwrap_or_else(|e| e.into_inner()) = None;
        finish_startup(&t, StartKind::Disabled, StartKind::Manual, "x", Err(denied), true);
        assert!(UNDO.lock().unwrap_or_else(|e| e.into_inner()).is_none());
    }

    #[test]
    fn summary_never_cuts_a_word_and_keeps_the_wide_form() {
        let forms = summary_forms(300, 1, 2);
        let text = |f: &[(String, Seg)]| f.iter().map(|(s, _)| s.as_str()).collect::<String>();
        let mut w7 = |s: &str, _k: Seg| s.chars().count() as f32 * 7.0;
        // wide: the full line, unchanged
        assert_eq!(text(pick_summary(&forms, 900.0, &mut w7).unwrap()), "300 services · 1 running · 2 automatic not running");
        // narrower: shorter forms, each made of whole words
        assert_eq!(text(pick_summary(&forms, 300.0, &mut w7).unwrap()), "300 services · 1 running");
        assert_eq!(text(pick_summary(&forms, 130.0, &mut w7).unwrap()), "300 · 1 running");
        assert_eq!(text(pick_summary(&forms, 60.0, &mut w7).unwrap()), "300 · 1");
        assert_eq!(text(pick_summary(&forms, 30.0, &mut w7).unwrap()), "300");
        assert!(pick_summary(&forms, 10.0, &mut w7).is_none(), "nothing fits: draw nothing, not a stub");
        // no automatic-not-running: no warning form
        assert_eq!(text(&summary_forms(300, 1, 0)[0]), "300 services · 1 running");
    }
}
