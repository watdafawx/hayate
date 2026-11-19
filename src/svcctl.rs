//! Service actions for the Services tab: Start / Stop / Restart and the startup
//! type, with nothing else about a service ever written.
//!
//! Only documented Service Control Manager calls, with the smallest access
//! rights each step needs: `QueryServiceConfig(2)` / `QueryServiceStatusEx`
//! (read), `ControlService(STOP)` / `StartService`, and `ChangeServiceConfig`
//! with every parameter `SERVICE_NO_CHANGE` / NULL except `dwStartType`, plus
//! `ChangeServiceConfig2(DELAYED_AUTO_START_INFO)`. No registry writes, no
//! service creation or deletion, no process kill, no cascading stops, no
//! privileges, no elevation: whatever the SCM's own ACL says is final.
//!
//! The decisions are pure and sit behind two small traits ([`Scm`] opens ONE
//! retained service handle, [`Retained`] / [`Svc`] are what that handle can do)
//! so the tests drive a fake SCM that models handle identity: what is read
//! before a write, what the write may touch, and what happens when the service
//! was deleted and recreated, changed, vanished or refuses.
//!
//! A [`Target`] is captured when a menu or the Properties panel opens (service
//! KEY name, never a row index or display name). The service is opened by name
//! ONCE per action or startup change; every read, validation, write, rollback
//! and read-back goes through that same handle, never a second open by name
//! (a name can be deleted and recreated under another image in between). The
//! action fails closed when the service is gone, now has another image or
//! startup type than the user saw, or was restarted.
//!
//! One piece of work per service at a time ([`BusyGuard`], shared by Start /
//! Stop / Restart and startup changes) and at most [`MAX_WORKERS`] workers in
//! all; a refused request answers at once and never waits on the UI thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// SERVICE_STATUS current states (winsvc.h), kept OS-free for the pure code.
pub mod st {
    pub const STOPPED: u32 = 1;
    pub const START_PENDING: u32 = 2;
    pub const STOP_PENDING: u32 = 3;
    pub const RUNNING: u32 = 4;
    pub const CONTINUE_PENDING: u32 = 5;
    pub const PAUSE_PENDING: u32 = 6;
    pub const PAUSED: u32 = 7;
    pub const ACCEPT_STOP: u32 = 1;
}

pub fn state_label(s: u32) -> &'static str {
    match s {
        st::STOPPED => "Stopped",
        st::START_PENDING => "Starting",
        st::STOP_PENDING => "Stopping",
        st::RUNNING => "Running",
        st::CONTINUE_PENDING => "Resuming",
        st::PAUSE_PENDING => "Pausing",
        st::PAUSED => "Paused",
        _ => "Unknown",
    }
}

// ---------- startup type ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartKind {
    Automatic,
    AutomaticDelayed,
    Manual,
    Disabled,
}

impl StartKind {
    pub const ALL: [StartKind; 4] = [StartKind::Automatic, StartKind::AutomaticDelayed, StartKind::Manual, StartKind::Disabled];

    pub fn label(self) -> &'static str {
        match self {
            StartKind::Automatic => "Automatic",
            StartKind::AutomaticDelayed => "Automatic (delayed start)",
            StartKind::Manual => "Manual",
            StartKind::Disabled => "Disabled",
        }
    }

    /// The SCM `Start` value that makes this kind.
    pub fn raw_start(self) -> u32 {
        match self {
            StartKind::Automatic | StartKind::AutomaticDelayed => 2,
            StartKind::Manual => 3,
            StartKind::Disabled => 4,
        }
    }

    /// Kind of a raw start type + the delayed flag. The flag only means
    /// something for an auto-start service (winsvc docs). None: Boot / System
    /// (drivers only) or an unknown value; these are never changed here.
    pub fn from_raw(start: u32, delayed: bool) -> Option<StartKind> {
        match (start, delayed) {
            (2, true) => Some(StartKind::AutomaticDelayed),
            (2, false) => Some(StartKind::Automatic),
            (3, _) => Some(StartKind::Manual),
            (4, _) => Some(StartKind::Disabled),
            _ => None,
        }
    }
}

/// The two config writes that make `want` from what the service has now.
/// `start` is `ChangeServiceConfig(dwStartType)`; `delayed` is
/// `ChangeServiceConfig2(DELAYED_AUTO_START_INFO)`. Everything else about the
/// service is never part of a plan. `None` = leave as it is.
///
/// The delayed flag is touched only when the target is an auto-start kind (it
/// is ignored otherwise), and only when it differs, so Manual and Disabled
/// never write it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Plan {
    pub start: Option<u32>,
    pub delayed: Option<bool>,
}

pub fn plan_change(cur_start: u32, cur_delayed: bool, want: StartKind) -> Plan {
    let start = (cur_start != want.raw_start()).then_some(want.raw_start());
    let delayed = match want {
        StartKind::Automatic => cur_delayed.then_some(false),
        StartKind::AutomaticDelayed => (!cur_delayed).then_some(true),
        StartKind::Manual | StartKind::Disabled => None,
    };
    Plan { start, delayed }
}

// ---------- captured target and live config ----------

/// What the user was looking at when a menu or panel opened.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Target {
    /// the service KEY name (identity)
    pub name: String,
    pub display: String,
    /// cleaned image path as the list had it ("" = not known)
    pub path: String,
    pub state: u32,
    pub pid: u32,
    pub can_stop: bool,
    /// raw start type and delayed flag as the list showed them
    pub start: u32,
    pub delayed: bool,
    pub trigger: bool,
}

impl Target {
    pub fn kind(&self) -> Option<StartKind> {
        StartKind::from_raw(self.start, self.delayed)
    }
    /// Why this capture cannot be the baseline of a control or startup change (None = it can).
    /// An action is judged against what the user SAW; a capture without the image path or a
    /// changeable startup type has nothing to compare the live service with, and a baseline
    /// read only at operation time would just be "whatever answers to the name now".
    pub fn baseline_problem(&self) -> Option<String> {
        if self.name.is_empty() {
            return Some("no service name was captured".into());
        }
        if self.path.is_empty() {
            return Some(format!("{}: the list has no program path for this service yet, so Hayate cannot tell it from a replacement. Nothing was done; try again when the list has loaded", self.name));
        }
        if self.kind().is_none() {
            return Some(format!("{}: its startup type is not known from the list (or is a driver type), so Hayate will not act on it", self.name));
        }
        None
    }
    pub fn key(&self) -> String {
        self.name.to_lowercase()
    }
}

/// One full read of a service from the SCM.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub name: String,
    pub display: String,
    pub description: String,
    /// lpBinaryPathName as stored (with arguments)
    pub binary: String,
    /// the image path cleaned like the list does
    pub path: String,
    pub service_type: u32,
    pub start: u32,
    pub error_control: u32,
    pub load_order_group: String,
    pub account: String,
    pub dependencies: Vec<String>,
    /// delayed auto-start flag; None = Windows did not report it (never guessed as "off")
    pub delayed: Option<bool>,
    /// trigger-start entries (None: not readable)
    pub triggers: Option<u32>,
    pub status: Status,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub state: u32,
    pub pid: u32,
    pub controls: u32,
}

impl Config {
    /// The startup type, or None for a driver start type or an Automatic service
    /// whose delayed flag could not be read (then it is not known to be Automatic
    /// rather than Automatic (delayed start)). Manual / Disabled never need the flag.
    pub fn kind(&self) -> Option<StartKind> {
        match self.start {
            2 => StartKind::from_raw(2, self.delayed?),
            s => StartKind::from_raw(s, false),
        }
    }
    /// Blink may change this one: a changeable type and a delayed flag that was really read.
    pub fn changeable(&self) -> bool {
        self.delayed.is_some() && self.kind().is_some()
    }
    /// "Manual (trigger start)" style note, shown next to the type, never written.
    pub fn trigger_note(&self) -> Option<&'static str> {
        match self.triggers {
            Some(n) if n > 0 => Some("Also started by a trigger (device, network, ...). Windows keeps that setting; Hayate never edits triggers."),
            _ => None,
        }
    }
    /// Same service with only the startup type (and the delayed flag) set aside:
    /// the "nothing else changed" comparison.
    fn without_startup(&self) -> Config {
        Config { start: 0, delayed: None, status: Status::default(), ..self.clone() }
    }
}

/// The captured target still names this service: same key and the same image.
/// A capture with no image path is never "the same": there is nothing to compare
/// (see [`Target::baseline_problem`]). An SCM path that cannot be read is not trusted either.
pub fn same_service(t: &Target, c: &Config) -> bool {
    t.name.eq_ignore_ascii_case(&c.name) && !t.path.is_empty() && t.path.eq_ignore_ascii_case(&c.path)
}

/// Same key only (the read-only Properties view of a service the list had no path for).
pub fn same_name(t: &Target, c: &Config) -> bool {
    t.name.eq_ignore_ascii_case(&c.name)
}

/// The live service still is what the user saw when the menu / panel opened:
/// same key and image, and the same startup type. Run on
/// the config read through the action's own retained handle, immediately before
/// each Start / Stop request.
pub fn captured_matches(t: &Target, c: &Config) -> Result<(), String> {
    if !same_service(t, c) {
        return Err(format!("it is no longer the service you picked (its image is now {}). Reopen the list.", if c.path.is_empty() { "unknown" } else { &c.path }));
    }
    if t.kind().is_some() {
        if c.start != t.start {
            return Err(format!("its startup type changed since you looked ({} is now {}). Check it and try again.", t.kind().map(|k| k.label()).unwrap_or("it"), c.kind().map(|k| k.label()).unwrap_or("another type")));
        }
        if t.start == 2 {
            match c.delayed {
                Some(d) if d != t.delayed => return Err("its delayed-start setting changed since you looked. Check it and try again.".into()),
                None => return Err("Windows did not report its delayed-start setting, so Hayate cannot confirm it is still the service you picked.".into()),
                _ => {}
            }
        }
    }
    Ok(())
}

// ---------- errors ----------

#[derive(Clone, Debug, PartialEq)]
pub enum ScmError {
    /// no such service (deleted, or never there)
    Gone,
    /// deletion pending
    Marked,
    Denied,
    Other(String),
}

impl ScmError {
    pub fn message(&self) -> String {
        match self {
            ScmError::Gone => "the service no longer exists".into(),
            ScmError::Marked => "the service is marked for deletion".into(),
            ScmError::Denied => "access denied: Windows allows only administrators (or protects this service). Hayate does not bypass that. Use Restart as administrator".into(),
            ScmError::Other(m) => m.clone(),
        }
    }
}

// ---------- startup type change ----------

/// The SCM, as far as a startup change is concerned: it opens the service by
/// name ONCE and hands back the retained handle. Nothing else in a transaction
/// may name the service again.
pub trait Scm {
    /// Open the service for reading (`write` false) or for reading and changing
    /// its configuration (`write` true), with the least rights that need.
    fn open(&mut self, name: &str, write: bool) -> Result<Box<dyn Retained>, ScmError>;
}

/// One open handle to one service object. Whatever the name points at later, every
/// call here acts on the service this handle was opened on.
pub trait Retained {
    fn read(&mut self) -> Result<Config, ScmError>;
    /// `ChangeServiceConfig` with only `dwStartType` set
    fn set_start(&mut self, start: u32) -> Result<(), ScmError>;
    /// `ChangeServiceConfig2(DELAYED_AUTO_START_INFO)`
    fn set_delayed(&mut self, on: bool) -> Result<(), ScmError>;
}

/// One read of a service by name (the Properties panel): open, read, close.
pub fn read_once(scm: &mut dyn Scm, name: &str) -> Result<Config, ScmError> {
    scm.open(name, false)?.read()
}

#[derive(Debug, PartialEq)]
pub struct StartupDone {
    pub after: Config,
    pub note: String,
}

/// What the service has now, in words, read through the same handle.
fn actual(h: &mut dyn Retained) -> String {
    match h.read() {
        Ok(c) => match c.kind() {
            Some(k) => format!("now {}", k.label()),
            None => "now in a state Hayate cannot name (check it in Properties)".to_string(),
        },
        Err(e) => format!("not readable now ({})", e.message()),
    }
}

/// The delayed-flag write failed after the start type was already changed. Put the
/// start type back ONLY when the service still holds exactly what this change wrote
/// (a newer change by anything else is never undone), then say what it really is.
fn undo_start(h: &mut dyn Retained, before: &Config, plan: &Plan, flag_err: &ScmError) -> String {
    let head = format!("could not set the delayed-start flag: {}", flag_err.message());
    // Two writes cannot be one atomic change (the SCM has no such call): from here the service
    // may hold only the first of them. Every branch below says what it really holds.
    let (Some(wrote), Some(was)) = (plan.start, before.kind()) else {
        return match h.read() {
            Ok(now) if now.start == before.start && now.delayed.is_some() && now.delayed == before.delayed => format!("{head}. Nothing was changed."),
            Ok(now) => format!("{head}. The result is not confirmed unchanged; {}. Check it in Properties.", now.kind().map(|k| format!("it is now {}", k.label())).unwrap_or_else(|| "its startup type is unknown".into())),
            Err(e) => format!("{head}. The result is unknown because reading it back failed ({}). Check it in Properties.", e.message()),
        };
    };
    match h.read() {
        Ok(now) if now.start == wrote && now.delayed == before.delayed => match h.set_start(before.start) {
            Ok(()) => format!("{head}. The startup type was put back ({}).", actual(h)),
            Err(e) => format!("{head}. The startup type could not be put back ({}); it is {}. Check it in Properties.", e.message(), actual(h)),
        },
        Ok(_) => format!("{head}. The startup type was changed again by something else meanwhile, so Hayate left it alone; it is {}.", actual(h)),
        Err(e) => {
            let new = match wrote {
                2 => "Automatic",
                3 => "Manual",
                4 => "Disabled",
                _ => "another type",
            };
            format!("{head}. The start type was changed to {new} (it was {}) and Hayate could not read it again to put it back ({}). Check it in Properties.", was.label(), e.message())
        }
    }
}

/// Change the startup type of `t` to `want`. `expect` is what the user saw
/// (list row / open Properties panel); the service must still have it.
///
/// One handle for the whole transaction: open → read → identity → "still what the
/// user saw" → write(s) → (rollback of Blink's own write only) → read back → verify.
pub fn apply_startup(scm: &mut dyn Scm, t: &Target, expect: StartKind, want: StartKind) -> Result<StartupDone, String> {
    if let Some(why) = t.baseline_problem() {
        return Err(format!("not changed: {why}"));
    }
    let mut h = scm.open(&t.name, true).map_err(|e| format!("not changed: {}", e.message()))?;
    let before = h.read().map_err(|e| format!("not changed: {}", e.message()))?;
    if !same_service(t, &before) {
        return Err(format!("not changed: {} is no longer the service you picked (its image changed: {}). Reopen the list.", t.name, before.path));
    }
    let Some(cur_delayed) = before.delayed else {
        return Err("not changed: Windows did not report the delayed-start setting, so Hayate cannot be sure of the current startup type. Close and reopen to read it again.".into());
    };
    let Some(cur) = before.kind() else {
        return Err(format!("not changed: start type {} is a driver start type; Hayate only changes Automatic, Manual and Disabled", before.start));
    };
    if cur != expect {
        return Err(format!("not changed: it is now {} (it was {} when you looked). Check and choose again.", cur.label(), expect.label()));
    }
    if cur == want {
        return Ok(StartupDone { after: before, note: format!("already {}", want.label()) });
    }
    let plan = plan_change(before.start, cur_delayed, want);
    if let Some(s) = plan.start {
        if let Err(e) = h.set_start(s) {
            // a transport failure can still have committed the write: look before claiming "unchanged"
            match h.read() {
                Ok(now) if now.start == s => {} // it landed: carry on as if the call had succeeded
                Ok(now) if now.start == before.start => return Err(format!("not changed: {}", e.message())),
                Ok(now) => {
                    return Err(format!(
                        "the start-type write reported an error ({}) and the service now reads {} (neither what it was nor what Hayate asked for). Hayate did not retry; check it in Properties.",
                        e.message(),
                        now.kind().map(|k| k.label()).unwrap_or("another type")
                    ))
                }
                Err(r) => {
                    return Err(format!(
                        "the start-type write reported an error ({}) and Hayate could not read the service to see whether it was applied ({}). The startup type is unknown; check it in Properties.",
                        e.message(),
                        r.message()
                    ))
                }
            }
        }
    }
    if let Some(d) = plan.delayed {
        if let Err(e) = h.set_delayed(d) {
            match h.read() {
                Ok(now) if now.delayed == Some(d) => {} // the flag landed after all
                _ => return Err(undo_start(&mut *h, &before, &plan, &e)),
            }
        }
    }
    let after = h.read().map_err(|e| format!("written, but reading it back failed: {}", e.message()))?;
    if after.delayed.is_none() {
        return Err("written, but Windows did not report the delayed-start setting afterwards, so the result cannot be confirmed; check it in Properties".into());
    }
    if after.kind() != Some(want) || after.without_startup() != before.without_startup() {
        return Err(format!("written, but it reads back as {}; check it in Properties", after.kind().map(|k| k.label()).unwrap_or("an unexpected value")));
    }
    let mut note = format!("startup type is now {}", want.label());
    if want == StartKind::Disabled && matches!(after.status.state, st::RUNNING | st::START_PENDING | st::PAUSED) {
        note.push_str(". It is still running: disabling does not stop it. Use Stop");
    }
    if after.triggers.is_some_and(|n| n > 0) && matches!(want, StartKind::Manual | StartKind::Disabled) {
        note.push_str(if want == StartKind::Disabled { ". A disabled service cannot be trigger-started" } else { ". Its trigger start stays as it was" });
    }
    Ok(StartupDone { after, note })
}

// ---------- start / stop / restart ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Start,
    Stop,
    Restart,
}

impl Op {
    pub fn verb(self) -> &'static str {
        match self {
            Op::Start => "Start",
            Op::Stop => "Stop",
            Op::Restart => "Restart",
        }
    }
    pub fn progress(self) -> &'static str {
        match self {
            Op::Start => "Starting",
            Op::Stop => "Stopping",
            Op::Restart => "Restarting",
        }
    }
    pub fn done(self) -> &'static str {
        match self {
            Op::Start => "started",
            Op::Stop => "stopped",
            Op::Restart => "restarted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Stop,
    WaitStopped,
    Start,
    WaitRunning,
}

#[derive(Debug, PartialEq)]
pub enum Steps {
    Do(Vec<Step>),
    /// nothing to do; the text says why
    Nothing(String),
}

/// What an op does from the service's state now. Pure.
pub fn plan_op(t: &Target, cur: &Status, op: Op) -> Result<Steps, String> {
    let n = if t.display.is_empty() { &t.name } else { &t.display };
    // the process the user saw is not the one running now
    if op != Op::Start && t.pid != 0 && cur.pid != 0 && cur.pid != t.pid {
        return Err(format!("{n} was restarted since you looked (PID {} is now {}). Nothing done; check it and try again.", t.pid, cur.pid));
    }
    let accepts_stop = cur.controls & st::ACCEPT_STOP != 0;
    match (op, cur.state) {
        (Op::Start, st::STOPPED) => Ok(Steps::Do(vec![Step::Start, Step::WaitRunning])),
        (Op::Start, st::RUNNING) => Ok(Steps::Nothing(format!("{n} is already running"))),
        (Op::Start, st::START_PENDING) => Ok(Steps::Do(vec![Step::WaitRunning])),
        (Op::Start, s) => Err(format!("{n} is {}: Start only works on a stopped service", state_label(s).to_lowercase())),
        (Op::Stop, st::STOPPED) => Ok(Steps::Nothing(format!("{n} is already stopped"))),
        (Op::Stop, st::STOP_PENDING) => Ok(Steps::Do(vec![Step::WaitStopped])),
        (Op::Stop, st::RUNNING | st::PAUSED | st::START_PENDING) if accepts_stop => Ok(Steps::Do(vec![Step::Stop, Step::WaitStopped])),
        (Op::Stop, st::RUNNING | st::PAUSED | st::START_PENDING) => Err(format!("{n} does not accept Stop. Hayate never kills a service process")),
        (Op::Stop, s) => Err(format!("{n} is {}: try again when it settles", state_label(s).to_lowercase())),
        (Op::Restart, st::RUNNING) if accepts_stop => Ok(Steps::Do(vec![Step::Stop, Step::WaitStopped, Step::Start, Step::WaitRunning])),
        (Op::Restart, st::RUNNING) => Err(format!("{n} does not accept Stop, so it cannot be restarted. Hayate never kills a service process")),
        (Op::Restart, s) => Err(format!("{n} is {}: Restart needs a running service", state_label(s).to_lowercase())),
    }
}

/// Why `ControlService(STOP)` failed, in words. `deps` are the running services
/// that depend on it (ERROR_DEPENDENT_SERVICES_RUNNING 1051); none are touched.
pub fn stop_failure(name: &str, code: u32, msg: &str, deps: &[String]) -> String {
    match code {
        1051 => {
            let list = if deps.is_empty() { "other services".to_string() } else { deps.iter().take(6).cloned().collect::<Vec<_>>().join(", ") + if deps.len() > 6 { ", ..." } else { "" } };
            format!("cannot stop {name}: {} running service{} depend{} on it ({list}). Hayate does not stop them for you; stop those first", deps.len().max(1), if deps.len() == 1 { "" } else { "s" }, if deps.len() == 1 { "s" } else { "" })
        }
        1052 => format!("cannot stop {name}: it does not accept Stop"),
        1061 => format!("cannot stop {name} right now: it is busy with another request"),
        5 => ScmError::Denied.message(),
        _ => format!("cannot stop {name}: {msg}"),
    }
}

pub fn start_failure(code: u32, msg: &str) -> String {
    match code {
        1058 => "it is Disabled. Change its startup type first".into(),
        1056 => "it is already running".into(),
        1053 => "it did not respond to the start request in time".into(),
        1068 => "a service it depends on could not start".into(),
        1069 => "its logon account was refused (check the account in Windows Services)".into(),
        5 => ScmError::Denied.message(),
        _ => msg.to_string(),
    }
}

/// One live service handle (rights chosen per op), opened once for the whole op.
pub trait Svc {
    fn status(&mut self) -> Result<Status, ScmError>;
    /// The configuration read through THIS handle (not a second open by name)
    fn config(&mut self) -> Result<Config, ScmError>;
    /// Err((win32 code, message, running dependents when 1051))
    fn stop(&mut self) -> Result<(), (u32, String, Vec<String>)>;
    fn start(&mut self) -> Result<(), (u32, String)>;
    fn sleep(&mut self);
}

#[derive(Debug, PartialEq)]
pub enum Wait {
    Reached,
    /// still not there; the last state seen
    TimedOut(u32),
    Cancelled,
    Failed(String),
}

/// Poll until `want`. A start that falls back to Stopped is a failure, not a wait.
pub fn wait_for(svc: &mut dyn Svc, want: u32, polls: u32, cancel: &dyn Fn() -> bool) -> Wait {
    let mut last = 0;
    for _ in 0..polls {
        if cancel() {
            return Wait::Cancelled;
        }
        match svc.status() {
            Ok(s) if s.state == want => return Wait::Reached,
            Ok(s) => {
                if want == st::RUNNING && s.state == st::STOPPED {
                    return Wait::Failed("it stopped again right after starting (the service exited; see the Windows event log)".into());
                }
                last = s.state;
            }
            Err(e) => return Wait::Failed(e.message()),
        }
        svc.sleep();
    }
    Wait::TimedOut(last)
}

/// Read the config through the op's own handle and check it is still what the user
/// picked. Called immediately before every Start / Stop request, so a service that was
/// deleted and recreated (other image, other startup type) never receives one.
fn still_the_target(svc: &mut dyn Svc, t: &Target, step: Step, expected: Status) -> Result<(), String> {
    let c = svc.config().map_err(|e| format!("could not re-read it first: {}", e.message()))?;
    captured_matches(t, &c)?;
    if step == Step::Stop && c.status.pid != expected.pid {
        return Err("its process changed immediately before the request; refresh and try again".into());
    }
    let op = if step == Step::Stop { Op::Stop } else { Op::Start };
    match plan_op(t, &c.status, op)? {
        Steps::Do(steps) if steps.first() == Some(&step) => Ok(()),
        _ => Err("its state or accepted controls changed immediately before the request; refresh and try again".into()),
    }
}

/// Run `op` on the service behind `svc`. Returns the result line.
pub fn run_op(svc: &mut dyn Svc, t: &Target, op: Op, polls: u32, cancel: &dyn Fn() -> bool) -> Result<String, String> {
    let n = if t.display.is_empty() { t.name.clone() } else { t.display.clone() };
    if let Some(why) = t.baseline_problem() {
        return Err(why);
    }
    let cur = svc.status().map_err(|e| format!("{n}: {}", e.message()))?;
    let steps = match plan_op(t, &cur, op).map_err(|e| e.to_string())? {
        Steps::Nothing(why) => return Ok(why),
        Steps::Do(s) => s,
    };
    let mut stopped_by_us = false;
    for s in steps {
        match s {
            Step::Stop => {
                still_the_target(svc, t, Step::Stop, cur).map_err(|why| format!("{n}: nothing done, {why}"))?;
                if let Err((code, msg, deps)) = svc.stop() {
                    // already gone by the time we asked (1062 not started) is fine
                    if code != 1062 {
                        return Err(stop_failure(&n, code, &msg, &deps));
                    }
                }
            }
            Step::Start => {
                if let Err(why) = still_the_target(svc, t, Step::Start, cur) {
                    return Err(if stopped_by_us { format!("{n} was stopped, but not started again: {why}") } else { format!("{n}: nothing done, {why}") });
                }
                if let Err((code, msg)) = svc.start() {
                    let why = start_failure(code, &msg);
                    return Err(if stopped_by_us { format!("{n} was stopped, but starting it again failed: {why}. It is now stopped") } else { format!("cannot start {n}: {why}") });
                }
            }
            Step::WaitStopped | Step::WaitRunning => {
                let want = if s == Step::WaitStopped { st::STOPPED } else { st::RUNNING };
                match wait_for(svc, want, polls, cancel) {
                    Wait::Reached => {
                        if want == st::STOPPED {
                            stopped_by_us = true;
                        }
                    }
                    Wait::Cancelled => return Err(format!("stopped waiting for {n} (Windows is still working on it; the list shows the real state)")),
                    Wait::TimedOut(state) => {
                        return Err(format!(
                            "{n} is still {} after {} s. Windows has not finished; Hayate does not kill it. The list shows the real state{}",
                            state_label(state).to_lowercase(),
                            polls / 4,
                            if op == Op::Restart && want == st::STOPPED { " (not started again)" } else { "" }
                        ))
                    }
                    Wait::Failed(m) => return Err(format!("{n}: {m}")),
                }
            }
        }
    }
    Ok(format!("{n}: {}", op.done()))
}

// ---------- in-flight registry (one piece of work per service, bounded workers) ----------

/// What a service is busy with: an op, or a startup-type change. Both take the
/// same per-service guard, so a startup transaction (write, rollback, read-back)
/// never overlaps another startup change or an op on the same service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    Op(Op),
    Startup,
}

impl Work {
    /// "starting", "restarting", "changing its startup type"
    pub fn progress(self) -> &'static str {
        match self {
            Work::Op(Op::Start) => "starting",
            Work::Op(Op::Stop) => "stopping",
            Work::Op(Op::Restart) => "restarting",
            Work::Startup => "changing its startup type",
        }
    }
}

/// Most service workers (ops + startup changes) running at once; one more is refused.
pub const MAX_WORKERS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// this service already has work in flight
    Busy(Work),
    /// the worker limit is reached
    Full,
}

impl Refused {
    pub fn message(self, name: &str) -> String {
        match self {
            Refused::Busy(w) => format!("{name}: already busy ({} in progress). Nothing was done.", w.progress()),
            Refused::Full => format!("{name}: {MAX_WORKERS} service changes are already in progress. Wait for one to finish; nothing was done."),
        }
    }
}

type BusyMap = HashMap<String, (Work, Arc<AtomicBool>)>;

/// Who is busy with what. One process-wide instance ([`BUSY`]); the tests build their own.
struct Registry(Mutex<Option<BusyMap>>);

static BUSY: Registry = Registry(Mutex::new(None));

impl Registry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<BusyMap>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, key: &str) -> Option<Work> {
        self.lock().as_ref().and_then(|m| m.get(&key.to_lowercase())).map(|(w, _)| *w)
    }

    /// Cheap and non-blocking: the answer is immediate, never a wait. `max` workers at most.
    fn take(&'static self, key: &str, work: Work, max: usize) -> Result<(BusyGuard, Arc<AtomicBool>), Refused> {
        let key = key.to_lowercase();
        let mut g = self.lock();
        let m = g.get_or_insert_with(HashMap::new);
        if let Some((w, _)) = m.get(&key) {
            return Err(Refused::Busy(*w));
        }
        if m.len() >= max {
            return Err(Refused::Full);
        }
        let c = Arc::new(AtomicBool::new(false));
        m.insert(key.clone(), (work, c.clone()));
        Ok((BusyGuard { reg: self, key }, c))
    }
}

/// What `key` is busy with now. Process-wide: it outlives the menu and the Properties panel.
pub fn busy_work(key: &str) -> Option<Work> {
    BUSY.get(key)
}

/// Stop waiting for the op in flight on `key` (the SCM request itself continues).
pub fn cancel_wait(key: &str) {
    if let Some((_, c)) = BUSY.lock().as_ref().and_then(|m| m.get(&key.to_lowercase())) {
        c.store(true, Ordering::Relaxed);
    }
}

struct BusyGuard {
    reg: &'static Registry,
    key: String,
}
impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Some(m) = self.reg.lock().as_mut() {
            m.remove(&self.key);
        }
    }
}

/// Test helper: mark `key` busy as if `work` were running.
#[cfg(test)]
pub fn hold_for_test(key: &str, work: Work) -> impl Drop {
    BUSY.take(key, work, MAX_WORKERS).ok().expect("free").0
}

pub const POLL_MS: u64 = 250;
pub const POLLS: u32 = 80;

/// The one way work reaches a worker thread: take the per-service guard (cheap,
/// answered at once when busy or full, never a wait on the UI thread), run `job` on a
/// worker, release the service, then report. The guard is held for the whole job, so a
/// startup transaction's write, rollback and read-back finish before anything else can
/// touch the same service. `refuse` shapes the "busy" answer into the job's result type.
fn spawn_guarded<R: Send + 'static>(
    name: &str,
    key: String,
    work: Work,
    job: impl FnOnce(&dyn Fn() -> bool) -> R + Send + 'static,
    refuse: impl Fn(String) -> R + Send + Sync + 'static,
    done: impl FnOnce(R) + Send + 'static,
) {
    spawn_guarded_in(&BUSY, MAX_WORKERS, name, key, work, job, refuse, done)
}

#[allow(clippy::too_many_arguments)]
fn spawn_guarded_in<R: Send + 'static>(
    reg: &'static Registry,
    max: usize,
    name: &str,
    key: String,
    work: Work,
    job: impl FnOnce(&dyn Fn() -> bool) -> R + Send + 'static,
    refuse: impl Fn(String) -> R + Send + Sync + 'static,
    done: impl FnOnce(R) + Send + 'static,
) {
    let (guard, cancel) = match reg.take(&key, work, max) {
        Ok(g) => g,
        Err(r) => return done(refuse(r.message(name))),
    };
    let done: Arc<Mutex<Option<Box<dyn FnOnce(R) + Send>>>> = Arc::new(Mutex::new(Some(Box::new(done))));
    let done2 = done.clone();
    let refuse = Arc::new(refuse);
    let refuse2 = refuse.clone();
    let name2 = name.to_string();
    let spawned = std::thread::Builder::new().name("svc-worker".into()).spawn(move || {
        // A panic must still end in a result: the panel waits on it. Nothing is claimed about the
        // service itself: a write that was already sent is not undone or denied.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&|| cancel.load(Ordering::Relaxed))))
            .unwrap_or_else(|_| refuse2(format!("{name2}: the worker stopped unexpectedly, so Hayate does not know whether the change was made. Check the service in Properties.")));
        // free the service before reporting: a result handler (or a reopened panel) sees it idle
        drop(guard);
        if let Some(d) = done2.lock().unwrap_or_else(|e| e.into_inner()).take() {
            d(r);
        }
    });
    if spawned.is_err() {
        // the closure (and its guard) went away with the failed spawn
        if let Some(d) = done.lock().unwrap_or_else(|e| e.into_inner()).take() {
            d(refuse(format!("{name}: a worker thread could not be started. Nothing was done.")));
        }
    }
}

/// Run Start / Stop / Restart on a worker thread; `done` gets the result line.
pub fn spawn_op(t: Target, op: Op, done: impl FnOnce(Result<String, String>) + Send + 'static) {
    if let Some(why) = t.baseline_problem() {
        return done(Err(why));
    }
    let (name, key) = (t.name.clone(), t.key());
    spawn_guarded(&name, key, Work::Op(op), move |cancel| real::run(&t, op, cancel), Err, done);
}

/// Change the startup type on a worker thread; `done` gets the result. The same
/// per-service guard as the ops is held for the whole transaction (write, rollback,
/// read-back), so repeated clicks or a close/reopen of the panel cannot overlap two
/// transactions; the second is refused at once and the first is never undone by it.
pub fn spawn_startup(t: Target, expect: StartKind, want: StartKind, done: impl FnOnce(Result<StartupDone, String>) + Send + 'static) {
    if let Some(why) = t.baseline_problem() {
        return done(Err(format!("not changed: {why}")));
    }
    let (name, key) = (t.name.clone(), t.key());
    spawn_guarded(&name, key, Work::Startup, move |_| apply_startup(&mut real::Real, &t, expect, want), |m| Err(format!("not changed: {m}")), done);
}

static READERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct ReadSlot;
impl ReadSlot {
    fn take() -> Option<ReadSlot> {
        let n = READERS.fetch_add(1, Ordering::AcqRel);
        if n >= MAX_WORKERS {
            READERS.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(ReadSlot)
    }
}
impl Drop for ReadSlot {
    fn drop(&mut self) {
        READERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Read a service for the Properties panel on a worker thread (at most
/// [`MAX_WORKERS`] at once; one more answers "busy" immediately).
pub fn spawn_read(name: String, done: impl FnOnce(Result<Config, ScmError>) + Send + 'static) {
    let Some(slot) = ReadSlot::take() else {
        return done(Err(ScmError::Other("too many reads in progress; close and reopen in a moment".into())));
    };
    let done: Arc<Mutex<Option<Box<dyn FnOnce(Result<Config, ScmError>) + Send>>>> = Arc::new(Mutex::new(Some(Box::new(done))));
    let done2 = done.clone();
    let spawned = std::thread::Builder::new().name("svc-read".into()).spawn(move || {
        let r = std::panic::catch_unwind(|| read_once(&mut real::Real, &name)).unwrap_or_else(|_| Err(ScmError::Other("the read stopped unexpectedly".into())));
        drop(slot);
        if let Some(d) = done2.lock().unwrap_or_else(|e| e.into_inner()).take() {
            d(r);
        }
    });
    if spawned.is_err() {
        if let Some(d) = done.lock().unwrap_or_else(|e| e.into_inner()).take() {
            d(Err(ScmError::Other("a worker thread could not be started".into())));
        }
    }
}

/// Debug test hook only (main.rs `--test-svc`, fixture services only): build a complete
/// baseline from the service itself, standing in for the list row a user would have clicked.
#[cfg(debug_assertions)]
pub fn debug_baseline(name: &str) -> Result<Target, String> {
    let c = read_once(&mut real::Real, name).map_err(|e| e.message())?;
    Ok(Target { name: name.to_string(), display: c.display.clone(), path: c.path.clone(), state: c.status.state, pid: c.status.pid, can_stop: c.status.controls & st::ACCEPT_STOP != 0, start: c.start, delayed: c.delayed.unwrap_or(false), trigger: false })
}

pub use real::open_services_console;
#[cfg(test)]
pub(crate) use real::Real;

// ---------- the real SCM ----------

mod real {
    use super::*;
    use crate::osutil::{from_wide, wide};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::System::Services::*;

    fn code(e: &windows::core::Error) -> u32 {
        // HRESULT_FROM_WIN32 keeps the Win32 code in the low 16 bits
        (e.code().0 as u32) & 0xFFFF
    }

    fn map(e: windows::core::Error) -> ScmError {
        match code(&e) {
            1060 => ScmError::Gone,
            1072 => ScmError::Marked,
            5 => ScmError::Denied,
            _ => ScmError::Other(e.message()),
        }
    }

    /// One open service: the SCM handle and the service handle, closed together. Opened
    /// by name ONCE; every call after that acts on the service this handle was opened on,
    /// whatever the name points at later. Implements both the startup transaction
    /// ([`Retained`]) and the op ([`Svc`]).
    pub struct Held {
        scm: SC_HANDLE,
        h: SC_HANDLE,
        name: String,
    }

    impl Drop for Held {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseServiceHandle(self.h);
                let _ = CloseServiceHandle(self.scm);
            }
        }
    }

    /// Open the SCM and the service with exactly `access`.
    fn open_held(name: &str, access: u32) -> Result<Held, ScmError> {
        unsafe {
            let scm = OpenSCManagerW(None, None, SC_MANAGER_CONNECT).map_err(map)?;
            let w = wide(name);
            match OpenServiceW(scm, PCWSTR(w.as_ptr()), access) {
                Ok(h) => Ok(Held { scm, h, name: name.to_string() }),
                Err(e) => {
                    let _ = CloseServiceHandle(scm);
                    Err(map(e))
                }
            }
        }
    }

    unsafe fn pw(p: PWSTR) -> String {
        if p.is_null() {
            String::new()
        } else {
            from_wide(p.as_wide())
        }
    }

    /// double-NUL list → names (group names keep their '+' prefix out)
    unsafe fn multi(p: PWSTR) -> Vec<String> {
        let mut out = Vec::new();
        if p.is_null() {
            return out;
        }
        let mut q = p.0 as *const u16;
        loop {
            let mut n = 0;
            while *q.add(n) != 0 {
                n += 1;
            }
            if n == 0 {
                break;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(q, n));
            out.push(s.strip_prefix('+').map(|g| format!("group {g}")).unwrap_or(s));
            q = q.add(n + 1);
        }
        out
    }

    /// u64-backed buffer: aligned for the structs the SCM writes into it
    fn buf(bytes: u32) -> Vec<u64> {
        vec![0u64; (bytes as usize).div_ceil(8) + 1]
    }
    fn bytes(b: &mut [u64]) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr() as *mut u8, b.len() * 8) }
    }

    fn status_of(h: SC_HANDLE) -> Result<Status, ScmError> {
        unsafe {
            let mut s = SERVICE_STATUS_PROCESS::default();
            let mut need = 0u32;
            let b = std::slice::from_raw_parts_mut(&mut s as *mut _ as *mut u8, std::mem::size_of::<SERVICE_STATUS_PROCESS>());
            QueryServiceStatusEx(h, SC_STATUS_PROCESS_INFO, Some(b), &mut need).map_err(map)?;
            Ok(Status { state: s.dwCurrentState.0, pid: s.dwProcessId, controls: s.dwControlsAccepted })
        }
    }

    fn read_cfg(name: &str, h: SC_HANDLE) -> Result<Config, ScmError> {
        unsafe {
            let mut need = 0u32;
            let _ = QueryServiceConfigW(h, None, 0, &mut need);
            if need == 0 {
                return Err(ScmError::Other("the service configuration could not be read".into()));
            }
            let mut b = buf(need);
            let p = b.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
            QueryServiceConfigW(h, Some(p), (b.len() * 8) as u32, &mut need).map_err(map)?;
            let q = &*p;
            let binary = pw(q.lpBinaryPathName);
            let mut c = Config {
                name: name.to_string(),
                display: pw(q.lpDisplayName),
                path: if binary.is_empty() { String::new() } else { crate::osutil::clean_image_path(&binary) },
                binary,
                service_type: q.dwServiceType.0,
                start: q.dwStartType.0,
                error_control: q.dwErrorControl.0,
                load_order_group: pw(q.lpLoadOrderGroup),
                account: pw(q.lpServiceStartName),
                dependencies: multi(q.lpDependencies),
                ..Default::default()
            };
            c.status = status_of(h)?;
            // optional parts: a failed read here is "unknown", never a guess
            let mut need = 0u32;
            let _ = QueryServiceConfig2W(h, SERVICE_CONFIG_DESCRIPTION, None, &mut need);
            if need > 0 {
                let mut b = buf(need);
                if QueryServiceConfig2W(h, SERVICE_CONFIG_DESCRIPTION, Some(bytes(&mut b)), &mut need).is_ok() {
                    let d = &*(b.as_ptr() as *const SERVICE_DESCRIPTIONW);
                    c.description = pw(d.lpDescription);
                    if c.description.starts_with('@') {
                        c.description = crate::watch::scm::indirect_string(&c.description).unwrap_or_default();
                    }
                }
            }
            // a failed read stays None: unknown is never "not delayed"
            let mut need = 0u32;
            let _ = QueryServiceConfig2W(h, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, None, &mut need);
            if need > 0 {
                let mut b = buf(need);
                if QueryServiceConfig2W(h, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(bytes(&mut b)), &mut need).is_ok() {
                    c.delayed = Some((*(b.as_ptr() as *const SERVICE_DELAYED_AUTO_START_INFO)).fDelayedAutostart.as_bool());
                }
            }
            let mut need = 0u32;
            let _ = QueryServiceConfig2W(h, SERVICE_CONFIG_TRIGGER_INFO, None, &mut need);
            if need > 0 {
                let mut b = buf(need);
                if QueryServiceConfig2W(h, SERVICE_CONFIG_TRIGGER_INFO, Some(bytes(&mut b)), &mut need).is_ok() {
                    c.triggers = Some((*(b.as_ptr() as *const SERVICE_TRIGGER_INFO)).cTriggers);
                }
            } else {
                // a service with no triggers reports an empty structure or fails with a size of 0
                c.triggers = None;
            }
            Ok(c)
        }
    }

    pub struct Real;

    impl Scm for Real {
        fn open(&mut self, name: &str, write: bool) -> Result<Box<dyn Retained>, ScmError> {
            // one handle for the whole transaction: read, write, rollback, read-back
            let access = SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS | if write { SERVICE_CHANGE_CONFIG } else { 0 };
            Ok(Box::new(open_held(name, access)?))
        }
    }

    impl Retained for Held {
        fn read(&mut self) -> Result<Config, ScmError> {
            read_cfg(&self.name, self.h)
        }
        fn set_start(&mut self, start: u32) -> Result<(), ScmError> {
            unsafe {
                // every other parameter: no change
                ChangeServiceConfigW(
                    self.h,
                    ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                    SERVICE_START_TYPE(start),
                    SERVICE_ERROR(SERVICE_NO_CHANGE),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    None,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    PCWSTR::null(),
                )
                .map_err(map)
            }
        }
        fn set_delayed(&mut self, on: bool) -> Result<(), ScmError> {
            unsafe {
                let info = SERVICE_DELAYED_AUTO_START_INFO { fDelayedAutostart: on.into() };
                ChangeServiceConfig2W(self.h, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(&info as *const _ as *const core::ffi::c_void)).map_err(map)
            }
        }
    }

    impl Svc for Held {
        fn status(&mut self) -> Result<Status, ScmError> {
            status_of(self.h)
        }
        fn config(&mut self) -> Result<Config, ScmError> {
            read_cfg(&self.name, self.h)
        }
        fn stop(&mut self) -> Result<(), (u32, String, Vec<String>)> {
            unsafe {
                let mut s = SERVICE_STATUS::default();
                match ControlService(self.h, SERVICE_CONTROL_STOP, &mut s) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let c = code(&e);
                        let deps = if c == 1051 { self.running_dependents() } else { Vec::new() };
                        Err((c, e.message(), deps))
                    }
                }
            }
        }
        fn start(&mut self) -> Result<(), (u32, String)> {
            unsafe { StartServiceW(self.h, None).map_err(|e| (code(&e), e.message())) }
        }
        fn sleep(&mut self) {
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    }

    impl Held {
        fn running_dependents(&self) -> Vec<String> {
            unsafe {
                let mut need = 0u32;
                let mut count = 0u32;
                let _ = EnumDependentServicesW(self.h, SERVICE_ACTIVE, None, 0, &mut need, &mut count);
                if need == 0 {
                    return Vec::new();
                }
                let mut b = buf(need);
                let p = b.as_mut_ptr() as *mut ENUM_SERVICE_STATUSW;
                if EnumDependentServicesW(self.h, SERVICE_ACTIVE, Some(p), (b.len() * 8) as u32, &mut need, &mut count).is_err() {
                    return Vec::new();
                }
                (0..count as usize)
                    .map(|i| {
                        let e = &*p.add(i);
                        let d = pw(e.lpDisplayName);
                        if d.is_empty() { pw(e.lpServiceName) } else { d }
                    })
                    .collect()
            }
        }
    }

    pub fn run(t: &Target, op: Op, cancel: &dyn Fn() -> bool) -> Result<String, String> {
        // QUERY_CONFIG: the captured image / startup type is re-checked on this very handle before each request
        let access = SERVICE_QUERY_CONFIG
            | match op {
                Op::Start => SERVICE_START | SERVICE_QUERY_STATUS,
                Op::Stop => SERVICE_STOP | SERVICE_QUERY_STATUS | SERVICE_ENUMERATE_DEPENDENTS,
                Op::Restart => SERVICE_START | SERVICE_STOP | SERVICE_QUERY_STATUS | SERVICE_ENUMERATE_DEPENDENTS,
            };
        let n = if t.display.is_empty() { t.name.clone() } else { t.display.clone() };
        let mut live = open_held(&t.name, access).map_err(|e| format!("{n}: {}", e.message()))?;
        run_op(&mut live, t, op, POLLS, cancel)
    }

    /// The system's own Services console (services.msc). Absolute system paths
    /// (mmc.exe documents a full path to the .msc); ShellExecute "open" with the
    /// caller's own rights: no elevation prompt is requested by Blink.
    ///
    /// Windows offers no documented way to open this console on one service or
    /// its Properties page: the mmc command line takes only the console file,
    /// /a, /32 and /64. So this only opens the list.
    pub fn open_services_console() -> Result<(), String> {
        use windows::core::w;
        use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        unsafe {
            let mut b = [0u16; 260];
            let n = GetSystemDirectoryW(Some(&mut b)) as usize;
            if n == 0 || n >= b.len() {
                return Err("the Windows system folder could not be found".into());
            }
            let sys = from_wide(&b[..n]);
            let mmc = wide(&format!("{sys}\\mmc.exe"));
            let args = wide(&format!("\"{sys}\\services.msc\""));
            let r = ShellExecuteW(None, w!("open"), PCWSTR(mmc.as_ptr()), PCWSTR(args.as_ptr()), None, SW_SHOWNORMAL);
            if r.0 as isize > 32 {
                Ok(())
            } else {
                Err(format!("Windows Services could not be opened (code {})", r.0 as isize))
            }
        }
    }
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(start: u32, delayed: bool) -> Config {
        Config {
            name: "Fixture".into(),
            display: "Fixture service".into(),
            description: "desc".into(),
            binary: "\"C:\\x\\fixture.exe\" -k group".into(),
            path: "C:\\x\\fixture.exe".into(),
            service_type: 0x10,
            start,
            error_control: 1,
            load_order_group: "grp".into(),
            account: "NT AUTHORITY\\LocalService".into(),
            dependencies: vec!["RpcSs".into(), "Tcpip".into()],
            delayed: Some(delayed),
            triggers: Some(2),
            status: Status { state: st::RUNNING, pid: 100, controls: st::ACCEPT_STOP },
        }
    }

    fn target(c: &Config) -> Target {
        Target { name: c.name.clone(), display: c.display.clone(), path: c.path.clone(), state: c.status.state, pid: c.status.pid, can_stop: true, start: c.start, delayed: c.delayed.unwrap_or(false), trigger: true }
    }

    // ---- a fake SCM that models service OBJECTS, not names

    /// One service object. A handle is bound to one of these for good; the name is only
    /// how it was found. Deleting and recreating a name makes a NEW object.
    struct Inst {
        cfg: Config,
        deleted: bool,
    }

    type Hook = Box<dyn FnMut(&mut World, &str) + Send>;

    struct World {
        insts: Vec<Inst>,
        /// lower-case name → object id (the live name table)
        names: HashMap<String, usize>,
        /// every config write: (object id, what, "was the service busy then")
        writes: Vec<(usize, String, bool)>,
        opens: Vec<String>,
        reads: u32,
        fail_start: Option<ScmError>,
        /// fail the Nth (1-based) start-type write only
        fail_start_nth: Option<(u32, ScmError)>,
        start_writes: u32,
        fail_delayed: Option<ScmError>,
        /// a failing write that nevertheless took effect (the reply was lost on the way back)
        land_failed_writes: bool,
        fail_read_from: Option<u32>,
        /// ignore writes (a service that reads back unchanged)
        deaf: bool,
        /// called before each handle call with "read1", "read2", ..., "set_start", "set_delayed"
        hook: Option<Hook>,
        busy_probe: Option<Box<dyn Fn() -> bool + Send>>,
    }

    impl World {
        fn with(c: Config) -> Arc<Mutex<World>> {
            let mut names = HashMap::new();
            names.insert(c.name.to_lowercase(), 0);
            Arc::new(Mutex::new(World {
                insts: vec![Inst { cfg: c, deleted: false }],
                names,
                writes: vec![],
                opens: vec![],
                reads: 0,
                fail_start: None,
                fail_start_nth: None,
                start_writes: 0,
                fail_delayed: None,
                land_failed_writes: false,
                fail_read_from: None,
                deaf: false,
                hook: None,
                busy_probe: None,
            }))
        }

        /// Delete the name's service and create another under the same name.
        fn recreate(&mut self, c: Config) -> usize {
            if let Some(&old) = self.names.get(&c.name.to_lowercase()) {
                self.insts[old].deleted = true;
            }
            self.insts.push(Inst { cfg: c.clone(), deleted: false });
            let id = self.insts.len() - 1;
            self.names.insert(c.name.to_lowercase(), id);
            id
        }

        fn fire(&mut self, ev: &str) {
            if let Some(mut h) = self.hook.take() {
                h(self, ev);
                self.hook = Some(h);
            }
        }
    }

    #[derive(Clone)]
    struct Fake(Arc<Mutex<World>>);

    impl Fake {
        fn new(c: Config) -> Fake {
            Fake(World::with(c))
        }
        fn w(&self) -> std::sync::MutexGuard<'_, World> {
            self.0.lock().unwrap()
        }
        /// the writes, as "start=4" / "delayed=true", any object
        fn writes(&self) -> Vec<String> {
            self.w().writes.iter().map(|(_, w, _)| w.clone()).collect()
        }
        /// the object the name points at now
        fn cfg(&self) -> Config {
            let w = self.w();
            w.insts[w.names["fixture"]].cfg.clone()
        }
        fn set_hook(&self, f: impl FnMut(&mut World, &str) + Send + 'static) {
            self.w().hook = Some(Box::new(f));
        }
    }

    struct FakeHandle {
        w: Arc<Mutex<World>>,
        id: usize,
    }

    impl Scm for Fake {
        fn open(&mut self, name: &str, _write: bool) -> Result<Box<dyn Retained>, ScmError> {
            let mut w = self.0.lock().unwrap();
            w.opens.push(name.to_string());
            let id = *w.names.get(&name.to_lowercase()).ok_or(ScmError::Gone)?;
            Ok(Box::new(FakeHandle { w: self.0.clone(), id }))
        }
    }

    impl Retained for FakeHandle {
        fn read(&mut self) -> Result<Config, ScmError> {
            let mut w = self.w.lock().unwrap();
            w.reads += 1;
            let n = w.reads;
            w.fire(&format!("read{n}"));
            if w.fail_read_from.is_some_and(|f| n >= f) {
                return Err(ScmError::Other("rpc".into()));
            }
            // a deleted-but-open service can still be read, like the real SCM
            Ok(w.insts[self.id].cfg.clone())
        }
        fn set_start(&mut self, start: u32) -> Result<(), ScmError> {
            let mut w = self.w.lock().unwrap();
            w.fire("set_start");
            w.start_writes += 1;
            let failing = w.fail_start.clone().or_else(|| w.fail_start_nth.clone().filter(|(n, _)| *n == w.start_writes).map(|(_, e)| e));
            if let Some(e) = failing {
                if w.land_failed_writes {
                    w.writes.push((self.id, format!("start={start}"), false));
                    w.insts[self.id].cfg.start = start;
                }
                return Err(e);
            }
            if w.insts[self.id].deleted {
                return Err(ScmError::Marked);
            }
            let busy = w.busy_probe.as_ref().is_some_and(|p| p());
            w.writes.push((self.id, format!("start={start}"), busy));
            if !w.deaf {
                w.insts[self.id].cfg.start = start;
            }
            Ok(())
        }
        fn set_delayed(&mut self, on: bool) -> Result<(), ScmError> {
            let mut w = self.w.lock().unwrap();
            w.fire("set_delayed");
            if let Some(e) = w.fail_delayed.clone() {
                if w.land_failed_writes {
                    w.writes.push((self.id, format!("delayed={on}"), false));
                    w.insts[self.id].cfg.delayed = Some(on);
                }
                return Err(e);
            }
            if w.insts[self.id].deleted {
                return Err(ScmError::Marked);
            }
            let busy = w.busy_probe.as_ref().is_some_and(|p| p());
            w.writes.push((self.id, format!("delayed={on}"), busy));
            w.insts[self.id].cfg.delayed = Some(on);
            Ok(())
        }
    }

    // --- pure startup-type model

    #[test]
    fn start_kind_round_trip() {
        assert_eq!(StartKind::from_raw(2, false), Some(StartKind::Automatic));
        assert_eq!(StartKind::from_raw(2, true), Some(StartKind::AutomaticDelayed));
        // the delayed flag means nothing on a manual / disabled service
        assert_eq!(StartKind::from_raw(3, true), Some(StartKind::Manual));
        assert_eq!(StartKind::from_raw(4, true), Some(StartKind::Disabled));
        // driver start types are never offered
        assert_eq!(StartKind::from_raw(0, false), None);
        assert_eq!(StartKind::from_raw(1, false), None);
        assert_eq!(StartKind::from_raw(9, false), None);
        for k in StartKind::ALL {
            assert_eq!(StartKind::from_raw(k.raw_start(), k == StartKind::AutomaticDelayed), Some(k));
        }
    }

    #[test]
    fn plan_touches_only_what_differs() {
        // Manual → Automatic (delayed): both writes, in this order (flag needs an auto service)
        assert_eq!(plan_change(3, false, StartKind::AutomaticDelayed), Plan { start: Some(2), delayed: Some(true) });
        // Automatic → delayed: flag only
        assert_eq!(plan_change(2, false, StartKind::AutomaticDelayed), Plan { start: None, delayed: Some(true) });
        // delayed → Automatic: flag only
        assert_eq!(plan_change(2, true, StartKind::Automatic), Plan { start: None, delayed: Some(false) });
        // Manual → Automatic with a stale TRUE flag: start, and the flag cleared (else it reads delayed)
        assert_eq!(plan_change(3, true, StartKind::Automatic), Plan { start: Some(2), delayed: Some(false) });
        // Manual / Disabled never write the flag
        assert_eq!(plan_change(2, true, StartKind::Manual), Plan { start: Some(3), delayed: None });
        assert_eq!(plan_change(2, false, StartKind::Disabled), Plan { start: Some(4), delayed: None });
        // nothing to do
        assert_eq!(plan_change(4, false, StartKind::Disabled), Plan::default());
    }

    // --- an unreadable delayed flag is UNKNOWN, never "off"

    #[test]
    fn unknown_delayed_flag_is_not_automatic_and_not_changeable() {
        let mut c = cfg(2, false);
        c.delayed = None;
        assert_eq!(c.kind(), None, "Automatic vs Automatic (delayed start) is not known");
        assert!(!c.changeable());
        // Manual / Disabled do not depend on the flag for what they are, but Blink still wants it read
        c.start = 3;
        assert_eq!(c.kind(), Some(StartKind::Manual));
        assert!(!c.changeable(), "no change without a real read of the flag");
        assert!(cfg(2, true).changeable() && cfg(2, false).changeable());
    }

    #[test]
    fn unknown_delayed_flag_refuses_every_change_without_writing() {
        for start in [2, 3] {
            let mut c = cfg(start, false);
            c.delayed = None;
            let t = target(&c);
            let mut f = Fake::new(c);
            let e = apply_startup(&mut f, &t, StartKind::Manual, StartKind::Disabled).unwrap_err();
            assert!(e.starts_with("not changed") && e.contains("delayed-start setting"), "{e}");
            assert!(f.writes().is_empty(), "start {start}: the old code read a failed query as 'not delayed' and wrote");
        }
    }

    #[test]
    fn a_flag_that_stops_being_readable_after_the_write_is_not_a_success() {
        // read-back finds the flag unreadable: the type cannot be verified, so it is an error
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        f.set_hook(|w, ev| {
            if ev == "read2" {
                w.insts[0].cfg.delayed = None;
            }
        });
        let e = apply_startup(&mut f, &t, StartKind::Manual, StartKind::Disabled).unwrap_err();
        assert!(e.contains("did not report the delayed-start setting afterwards"), "{e}");
    }

    #[test]
    fn captured_target_check() {
        let c = cfg(2, true);
        let t = target(&c);
        assert!(captured_matches(&t, &c).is_ok());
        // other image
        let mut other = c.clone();
        other.path = "C:\\evil\\x.exe".into();
        assert!(captured_matches(&t, &other).unwrap_err().contains("no longer the service"));
        // other startup type
        let mut dis = c.clone();
        dis.start = 4;
        assert!(captured_matches(&t, &dis).unwrap_err().contains("startup type changed"));
        // delayed flag flipped, or not readable on an Automatic service
        let mut nd = c.clone();
        nd.delayed = Some(false);
        assert!(captured_matches(&t, &nd).unwrap_err().contains("delayed-start setting changed"));
        nd.delayed = None;
        assert!(captured_matches(&t, &nd).unwrap_err().contains("did not report"));
        // a capture with no image path or startup type has nothing to compare: never "matches"
        let bare = Target { name: "Fixture".into(), ..Default::default() };
        assert!(captured_matches(&bare, &c).is_err());
    }

    #[test]
    fn a_capture_without_a_baseline_cannot_drive_any_change() {
        let c = cfg(3, false);
        let full = target(&c);
        assert_eq!(full.baseline_problem(), None);
        let mut nopath = full.clone();
        nopath.path.clear();
        assert!(nopath.baseline_problem().unwrap().contains("no program path"));
        let mut driver = full.clone();
        driver.start = 1;
        assert!(driver.baseline_problem().unwrap().contains("startup type is not known"));
        assert!(Target::default().baseline_problem().is_some());
        // the transaction does not even open the service: a baseline read now would only prove "whatever has the name"
        let mut f = Fake::new(c.clone());
        let e = apply_startup(&mut f, &nopath, StartKind::Manual, StartKind::Disabled).unwrap_err();
        assert!(e.starts_with("not changed") && e.contains("no program path"), "{e}");
        assert!(f.w().opens.is_empty() && f.w().writes.is_empty());
        // an op: refused before it reaches the handle
        let mut s = FakeSvc::new(vec![stat(st::STOPPED, 0)]);
        let e = run_op(&mut s, &nopath, Op::Start, 10, &|| false).unwrap_err();
        assert!(e.contains("no program path") && s.calls.is_empty() && s.ci == 0, "{e}");
        // and the spawn entry points answer at once, without taking the service or a thread
        let key = nopath.key();
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_op(nopath.clone(), Op::Start, move |r| tx.send(r).unwrap());
        assert!(rx.try_recv().unwrap().unwrap_err().contains("no program path"));
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_startup(nopath, StartKind::Manual, StartKind::Disabled, move |r| tx.send(r.map(|_| ())).unwrap());
        assert!(rx.try_recv().unwrap().unwrap_err().starts_with("not changed"));
        assert_eq!(busy_work(&key), None);
    }

    // --- apply_startup against the fake SCM

    #[test]
    fn disable_changes_only_the_start_type() {
        let c = cfg(2, false);
        let t = target(&c);
        let mut f = Fake::new(c.clone());
        let done = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap();
        assert_eq!(f.writes(), vec!["start=4"]);
        assert_eq!(done.after.start, 4);
        // account, dependencies, binary, type, error control, group, trigger, description: byte-for-byte as before
        assert_eq!(done.after.without_startup(), c.without_startup());
        assert_eq!(done.after.account, "NT AUTHORITY\\LocalService");
        assert_eq!(done.after.dependencies, vec!["RpcSs", "Tcpip"]);
        assert_eq!(done.after.triggers, Some(2));
        // running service: said plainly that it keeps running
        assert!(done.note.contains("still running") && done.note.contains("Stop"), "{}", done.note);
        assert!(done.note.contains("cannot be trigger-started"), "{}", done.note);
    }

    #[test]
    fn manual_to_delayed_writes_start_then_flag_and_keeps_everything_else() {
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c.clone());
        let done = apply_startup(&mut f, &t, StartKind::Manual, StartKind::AutomaticDelayed).unwrap();
        assert_eq!(f.writes(), vec!["start=2", "delayed=true"]);
        assert_eq!(done.after.kind(), Some(StartKind::AutomaticDelayed));
        assert_eq!(done.after.without_startup(), c.without_startup());
    }

    #[test]
    fn delayed_to_manual_leaves_flag_and_trigger() {
        let c = cfg(2, true);
        let t = target(&c);
        let mut f = Fake::new(c);
        let done = apply_startup(&mut f, &t, StartKind::AutomaticDelayed, StartKind::Manual).unwrap();
        assert_eq!(f.writes(), vec!["start=3"]);
        assert_eq!(done.after.delayed, Some(true), "flag untouched: no write for a non-auto target");
        assert_eq!(done.after.triggers, Some(2));
        assert!(done.note.contains("trigger start stays"), "{}", done.note);
    }

    #[test]
    fn same_type_is_a_no_op_without_any_write() {
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        let done = apply_startup(&mut f, &t, StartKind::Manual, StartKind::Manual).unwrap();
        assert!(f.writes().is_empty());
        assert!(done.note.starts_with("already"));
    }

    #[test]
    fn fails_closed_when_service_is_gone_or_unreadable() {
        let c = cfg(2, false);
        let t = target(&c);
        let mut f = Fake::new(c.clone());
        f.w().names.clear();
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap_err();
        assert!(e.contains("no longer exists") && e.starts_with("not changed"), "{e}");
        assert!(f.w().writes.is_empty());
        // a denied open, and a denied read, are refusals too, never a blind write
        struct Deny(bool);
        struct DenyRead;
        impl Scm for Deny {
            fn open(&mut self, _: &str, _: bool) -> Result<Box<dyn Retained>, ScmError> {
                if self.0 { Err(ScmError::Denied) } else { Ok(Box::new(DenyRead)) }
            }
        }
        impl Retained for DenyRead {
            fn read(&mut self) -> Result<Config, ScmError> {
                Err(ScmError::Denied)
            }
            fn set_start(&mut self, _: u32) -> Result<(), ScmError> {
                panic!("wrote without reading")
            }
            fn set_delayed(&mut self, _: bool) -> Result<(), ScmError> {
                panic!("wrote without reading")
            }
        }
        for open_denied in [true, false] {
            assert!(apply_startup(&mut Deny(open_denied), &t, StartKind::Automatic, StartKind::Disabled).unwrap_err().contains("access denied"));
        }
    }

    #[test]
    fn identity_mismatch_refuses_without_writing() {
        let c = cfg(2, false);
        let mut t = target(&c);
        // deleted and recreated under another image: same key, different exe
        t.path = "C:\\other\\thing.exe".into();
        let mut f = Fake::new(c);
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap_err();
        assert!(e.contains("no longer the service"), "{e}");
        assert!(f.w().writes.is_empty());
        // case in the key and the path does not matter
        let c = cfg(2, false);
        let mut t = target(&c);
        t.name = "FIXTURE".into();
        t.path = t.path.to_uppercase();
        let mut f = Fake::new(c);
        assert!(apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Manual).is_ok());
    }

    #[test]
    fn changed_since_the_user_looked_refuses() {
        let c = cfg(2, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        // another admin disabled it between the list read and ours
        f.set_hook(|w, ev| {
            if ev == "read1" {
                w.insts[0].cfg.start = 4;
            }
        });
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Manual).unwrap_err();
        assert!(e.contains("now Disabled") && e.contains("was Automatic"), "{e}");
        assert!(f.w().writes.is_empty());
    }

    #[test]
    fn driver_start_types_are_not_changed() {
        // the list showed a Manual service; what answers to the name now has a driver start type
        let t = target(&cfg(3, false));
        let mut f = Fake::new(cfg(1, false));
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap_err();
        assert!(e.contains("driver start type"), "{e}");
        assert!(f.w().writes.is_empty());
    }

    #[test]
    fn access_denied_on_write_is_reported_not_bypassed() {
        let c = cfg(2, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        f.w().fail_start = Some(ScmError::Denied);
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap_err();
        assert!(e.contains("access denied") && e.contains("does not bypass"), "{e}");
        assert!(f.w().writes.is_empty());
    }

    #[test]
    fn read_back_mismatch_is_an_error() {
        let c = cfg(2, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        f.w().deaf = true;
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap_err();
        assert!(e.contains("reads back"), "{e}");
        let mut f = Fake::new(cfg(2, false));
        f.w().fail_read_from = Some(2);
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Manual).unwrap_err();
        assert!(e.contains("reading it back failed"), "{e}");
    }

    // --- rollback: only Blink's own write, never a newer change

    #[test]
    fn flag_failure_after_start_change_puts_back_only_its_own_write_and_says_what_it_is() {
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        f.w().fail_delayed = Some(ScmError::Other("invalid parameter".into()));
        let e = apply_startup(&mut f, &t, StartKind::Manual, StartKind::AutomaticDelayed).unwrap_err();
        assert!(e.contains("invalid parameter") && e.contains("put back") && e.contains("now Manual"), "{e}");
        assert_eq!(f.writes(), vec!["start=2", "start=3"], "start put back");
        assert_eq!(f.cfg().start, 3);
    }

    #[test]
    fn rollback_never_overwrites_a_newer_change() {
        // Blink writes start=2; before its delayed write fails, something else disables the service.
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        f.w().fail_delayed = Some(ScmError::Other("invalid parameter".into()));
        f.set_hook(|w, ev| {
            if ev == "set_delayed" {
                w.insts[0].cfg.start = 4;
            }
        });
        let e = apply_startup(&mut f, &t, StartKind::Manual, StartKind::AutomaticDelayed).unwrap_err();
        assert_eq!(f.writes(), vec!["start=2"], "no second start write: the newer change stays");
        assert_eq!(f.cfg().start, 4, "the other change is untouched");
        assert!(e.contains("changed again by something else") && e.contains("left it alone") && e.contains("now Disabled"), "{e}");
    }

    #[test]
    fn rollback_that_cannot_run_says_what_the_service_is() {
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        {
            let mut w = f.w();
            w.fail_delayed = Some(ScmError::Other("invalid parameter".into()));
            // the 2nd start-type write (the put-back) is refused
            w.fail_start_nth = Some((2, ScmError::Denied));
        }
        let e = apply_startup(&mut f, &t, StartKind::Manual, StartKind::AutomaticDelayed).unwrap_err();
        assert!(e.contains("could not be put back") && e.contains("access denied") && e.contains("now Automatic"), "{e}");
        assert_eq!(f.cfg().start, 2, "reported as it really is");
        // a read that fails before the put-back: no write attempted, truth told
        let c = cfg(3, false);
        let mut f = Fake::new(c.clone());
        {
            let mut w = f.w();
            w.fail_delayed = Some(ScmError::Other("invalid parameter".into()));
            w.fail_read_from = Some(2);
        }
        let e = apply_startup(&mut f, &target(&c), StartKind::Manual, StartKind::AutomaticDelayed).unwrap_err();
        assert!(e.contains("could not read it again to put it back") && e.contains("changed to Automatic"), "{e}");
        assert_eq!(f.writes(), vec!["start=2"]);
    }

    #[test]
    fn a_flag_only_failure_changed_nothing_and_says_so() {
        let c = cfg(2, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        f.w().fail_delayed = Some(ScmError::Denied);
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::AutomaticDelayed).unwrap_err();
        assert!(e.contains("Nothing was changed"), "{e}");
        assert!(f.w().writes.is_empty());
    }

    #[test]
    fn flag_only_failure_with_unreadable_or_unknown_result_never_claims_unchanged() {
        for unreadable in [true, false] {
            let c = cfg(2, false);
            let t = target(&c);
            let mut f = Fake::new(c);
            {
                let mut w = f.w();
                w.fail_delayed = Some(ScmError::Other("reply lost".into()));
                w.land_failed_writes = true;
                if unreadable {
                    w.fail_read_from = Some(2);
                } else {
                    w.hook = Some(Box::new(|w, ev| {
                        if ev.starts_with("read") && ev != "read1" {
                            w.insts[0].cfg.delayed = None;
                        }
                    }));
                }
            }
            let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::AutomaticDelayed).unwrap_err();
            assert!(!e.contains("Nothing was changed"), "{e}");
            assert!(e.contains("unknown"), "{e}");
        }
    }

    // --- a failed call may still have been applied: look before claiming "unchanged"

    #[test]
    fn a_start_write_that_errors_but_landed_is_not_reported_as_unchanged() {
        let c = cfg(2, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        {
            let mut w = f.w();
            w.fail_start = Some(ScmError::Other("rpc transport failure".into()));
            w.land_failed_writes = true;
        }
        // the write was applied although the call reported failure: the transaction carries on and verifies it
        let done = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap();
        assert_eq!(done.after.start, 4);
        assert_eq!(f.w().opens.len(), 1);
    }

    #[test]
    fn a_start_write_that_errors_and_did_not_land_is_unchanged_only_after_a_look() {
        let c = cfg(2, false);
        let t = target(&c);
        let mut f = Fake::new(c.clone());
        f.w().fail_start = Some(ScmError::Other("rpc transport failure".into()));
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap_err();
        assert!(e.starts_with("not changed") && e.contains("rpc transport failure"), "{e}");
        assert!(f.w().reads >= 2, "it re-read through the same handle before saying so");
        // cannot look: say the state is unknown, not "not changed"
        let mut f = Fake::new(c.clone());
        {
            let mut w = f.w();
            w.fail_start = Some(ScmError::Other("rpc transport failure".into()));
            w.fail_read_from = Some(2);
        }
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap_err();
        assert!(!e.starts_with("not changed") && e.contains("unknown") && e.contains("rpc transport failure"), "{e}");
        // somebody else changed it to a third value meanwhile: reported as such, no retry
        let mut f = Fake::new(c);
        f.w().fail_start = Some(ScmError::Other("rpc transport failure".into()));
        f.set_hook(|w, ev| {
            if ev == "set_start" {
                w.insts[0].cfg.start = 3;
            }
        });
        let e = apply_startup(&mut f, &t, StartKind::Automatic, StartKind::Disabled).unwrap_err();
        assert!(e.contains("now reads Manual") && e.contains("did not retry"), "{e}");
        assert!(f.w().writes.is_empty());
    }

    #[test]
    fn a_flag_write_that_errors_but_landed_completes_the_change() {
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        {
            let mut w = f.w();
            w.fail_delayed = Some(ScmError::Other("rpc transport failure".into()));
            w.land_failed_writes = true;
        }
        let done = apply_startup(&mut f, &t, StartKind::Manual, StartKind::AutomaticDelayed).unwrap();
        assert_eq!(done.after.kind(), Some(StartKind::AutomaticDelayed));
        assert_eq!(f.writes(), vec!["start=2", "delayed=true"], "no put-back of a change that actually took");
    }

    // --- a worker that panics still ends in a result, and frees the service

    #[test]
    fn a_panicking_worker_reports_and_releases_the_service() {
        use std::sync::mpsc;
        static PANIC_REG: Registry = Registry(Mutex::new(None));
        let (tx, rx) = mpsc::channel::<(Result<String, String>, Option<Work>)>();
        spawn_guarded_in(&PANIC_REG, MAX_WORKERS, "Svc", "svc".into(), Work::Startup, |_| -> Result<String, String> { panic!("boom") }, Err, move |r| {
            tx.send((r, PANIC_REG.get("svc"))).unwrap();
        });
        let (r, busy) = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("a result must always arrive");
        let e = r.unwrap_err();
        assert!(e.contains("stopped unexpectedly") && e.contains("does not know whether"), "{e}");
        assert_eq!(busy, None, "the guard was released before the report");
    }

    // --- one retained handle for the whole transaction

    #[test]
    fn a_startup_transaction_opens_the_service_by_name_exactly_once() {
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        f.w().fail_delayed = Some(ScmError::Other("invalid parameter".into()));
        let _ = apply_startup(&mut f, &t, StartKind::Manual, StartKind::AutomaticDelayed);
        assert_eq!(f.w().opens, vec!["Fixture"], "read, writes, rollback and read-back all use the one handle");
        let c = cfg(3, false);
        let mut f = Fake::new(c.clone());
        apply_startup(&mut f, &target(&c), StartKind::Manual, StartKind::AutomaticDelayed).unwrap();
        assert_eq!(f.w().opens.len(), 1);
    }

    fn replacement() -> Config {
        // same key, another program, another account
        Config { path: "C:\\other\\thing.exe".into(), binary: "C:\\other\\thing.exe".into(), account: "LocalSystem".into(), ..cfg(2, false) }
    }

    #[test]
    fn a_service_recreated_mid_transaction_is_never_written_by_name() {
        // Blink read the real service, then it was deleted and recreated under the same name.
        // The write must go to the object Blink opened (now marked for deletion), not to the newcomer.
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        f.set_hook(|w, ev| {
            // right after Blink's read: the name now points at a different service object
            if ev == "read1" {
                w.recreate(replacement());
            }
        });
        let e = apply_startup(&mut f, &t, StartKind::Manual, StartKind::Disabled).unwrap_err();
        assert!(e.contains("marked for deletion"), "{e}");
        let w = f.w();
        assert!(w.writes.is_empty(), "nothing was written anywhere: {:?}", w.writes);
        let new = w.names["fixture"];
        assert_eq!(new, 1);
        assert_eq!(w.insts[new].cfg, replacement(), "the new service is untouched");
    }

    #[test]
    fn rollback_and_read_back_stay_on_the_original_object_through_recreation() {
        // start=2 lands on the service Blink opened; then it is deleted and recreated and the flag write
        // fails. The put-back must not reach the newcomer either.
        let c = cfg(3, false);
        let t = target(&c);
        let mut f = Fake::new(c);
        f.set_hook(|w, ev| {
            if ev == "set_delayed" {
                w.recreate(replacement());
            }
        });
        let e = apply_startup(&mut f, &t, StartKind::Manual, StartKind::AutomaticDelayed).unwrap_err();
        assert!(e.contains("marked for deletion"), "{e}");
        let w = f.w();
        assert!(w.writes.iter().all(|(id, ..)| *id == 0), "every write went to the original object: {:?}", w.writes);
        assert_eq!(w.insts[1].cfg, replacement(), "the new service is untouched");
        assert_eq!(w.opens.len(), 1);
    }

    // --- ops

    /// what the list showed for the fixture: Manual, image fixture.exe (matches `FakeSvc`'s default config)
    fn run_t(pid: u32) -> Target {
        Target { name: "Fixture".into(), display: "Fixture service".into(), path: "C:\\x\\fixture.exe".into(), pid, can_stop: true, start: 3, ..Default::default() }
    }
    fn stat(state: u32, pid: u32) -> Status {
        Status { state, pid, controls: st::ACCEPT_STOP }
    }

    #[test]
    fn plans_follow_the_state_truthfully() {
        let t = run_t(100);
        assert_eq!(plan_op(&t, &stat(st::STOPPED, 0), Op::Start), Ok(Steps::Do(vec![Step::Start, Step::WaitRunning])));
        assert!(matches!(plan_op(&t, &stat(st::RUNNING, 100), Op::Start), Ok(Steps::Nothing(_))));
        assert_eq!(plan_op(&t, &stat(st::RUNNING, 100), Op::Restart), Ok(Steps::Do(vec![Step::Stop, Step::WaitStopped, Step::Start, Step::WaitRunning])));
        assert!(plan_op(&t, &stat(st::STOPPED, 0), Op::Restart).is_err());
        assert!(matches!(plan_op(&t, &stat(st::STOPPED, 0), Op::Stop), Ok(Steps::Nothing(_))));
        // a stop already pending: just wait for it
        assert_eq!(plan_op(&t, &stat(st::STOP_PENDING, 100), Op::Stop), Ok(Steps::Do(vec![Step::WaitStopped])));
        assert_eq!(plan_op(&t, &stat(st::START_PENDING, 0), Op::Start), Ok(Steps::Do(vec![Step::WaitRunning])));
        // a service that refuses Stop is never killed
        let no_stop = Status { state: st::RUNNING, pid: 100, controls: 0 };
        let e = plan_op(&t, &no_stop, Op::Stop).unwrap_err();
        assert!(e.contains("never kills"), "{e}");
        assert!(plan_op(&t, &no_stop, Op::Restart).is_err());
        // pending transitions are not interrupted by another request
        assert!(plan_op(&t, &stat(st::STOP_PENDING, 100), Op::Start).is_err());
        assert!(plan_op(&t, &stat(st::PAUSED, 100), Op::Start).is_err());
    }

    #[test]
    fn no_blind_pid_actions() {
        // the menu saw PID 100; it is a different process now
        let t = run_t(100);
        let e = plan_op(&t, &stat(st::RUNNING, 777), Op::Stop).unwrap_err();
        assert!(e.contains("restarted since you looked") && e.contains("100") && e.contains("777"), "{e}");
        assert!(plan_op(&t, &stat(st::RUNNING, 777), Op::Restart).is_err());
        // Start has no PID to protect
        assert!(plan_op(&t, &stat(st::STOPPED, 0), Op::Start).is_ok());
        // saw it stopped (pid 0) and it started meanwhile: no PID claim
        assert!(plan_op(&run_t(0), &stat(st::RUNNING, 55), Op::Stop).is_ok());
    }

    #[test]
    fn stop_failure_names_dependents_and_stops_nothing_else() {
        let m = stop_failure("Fixture service", 1051, "x", &["Alpha".into(), "Beta".into()]);
        assert!(m.contains("2 running services depend on it") && m.contains("Alpha, Beta") && m.contains("does not stop them"), "{m}");
        let one = stop_failure("F", 1051, "x", &["Alpha".into()]);
        assert!(one.contains("1 running service depends on it"), "{one}");
        let many: Vec<String> = (0..9).map(|i| format!("S{i}")).collect();
        assert!(stop_failure("F", 1051, "x", &many).contains("S5, ..."));
        assert!(stop_failure("F", 5, "x", &[]).contains("access denied"));
        assert!(stop_failure("F", 99, "boom", &[]).contains("boom"));
        assert!(start_failure(1058, "").contains("Disabled"));
    }

    /// A fake op handle bound to ONE service object. `cfgs` are what successive `config()`
    /// calls through this handle return (the last repeats): a recreated service shows up there.
    struct FakeSvc {
        /// states returned by successive status() calls (last repeats)
        seq: Vec<Status>,
        i: usize,
        cfgs: Vec<Config>,
        fresh: Vec<Status>,
        ci: usize,
        calls: Vec<&'static str>,
        stop_err: Option<(u32, String, Vec<String>)>,
        start_err: Option<(u32, String)>,
        sleeps: u32,
    }
    impl FakeSvc {
        fn new(seq: Vec<Status>) -> Self {
            FakeSvc { seq, i: 0, cfgs: vec![cfg(3, false)], fresh: vec![], ci: 0, calls: vec![], stop_err: None, start_err: None, sleeps: 0 }
        }
    }
    impl Svc for FakeSvc {
        fn status(&mut self) -> Result<Status, ScmError> {
            let s = self.seq[self.i.min(self.seq.len() - 1)];
            self.i += 1;
            Ok(s)
        }
        fn config(&mut self) -> Result<Config, ScmError> {
            let mut c = self.cfgs[self.ci.min(self.cfgs.len() - 1)].clone();
            c.status = if self.fresh.is_empty() { self.seq[self.i.saturating_sub(1).min(self.seq.len() - 1)] } else { self.fresh[self.ci.min(self.fresh.len() - 1)] };
            self.ci += 1;
            Ok(c)
        }
        fn stop(&mut self) -> Result<(), (u32, String, Vec<String>)> {
            self.calls.push("stop");
            self.stop_err.clone().map_or(Ok(()), Err)
        }
        fn start(&mut self) -> Result<(), (u32, String)> {
            self.calls.push("start");
            self.start_err.clone().map_or(Ok(()), Err)
        }
        fn sleep(&mut self) {
            self.sleeps += 1;
        }
    }

    #[test]
    fn restart_stops_waits_then_starts() {
        let mut s = FakeSvc::new(vec![stat(st::RUNNING, 100), stat(st::STOP_PENDING, 100), stat(st::STOPPED, 0), stat(st::START_PENDING, 0), stat(st::RUNNING, 200)]);
        let r = run_op(&mut s, &run_t(100), Op::Restart, 10, &|| false).unwrap();
        assert_eq!(r, "Fixture service: restarted");
        assert_eq!(s.calls, vec!["stop", "start"]);
        assert_eq!(s.ci, 2, "the target was checked on this handle before the stop and before the start");
    }

    #[test]
    fn dispatch_refuses_a_new_process_or_changed_controls_after_planning() {
        for fresh in [stat(st::RUNNING, 777), Status { controls: 0, ..stat(st::RUNNING, 100) }, stat(st::STOP_PENDING, 100)] {
            let mut s = FakeSvc::new(vec![stat(st::RUNNING, 100)]);
            s.fresh = vec![fresh];
            assert!(run_op(&mut s, &run_t(100), Op::Stop, 10, &|| false).is_err());
            assert!(s.calls.is_empty());
        }
        let mut s = FakeSvc::new(vec![stat(st::RUNNING, 100), stat(st::STOPPED, 0)]);
        s.fresh = vec![stat(st::RUNNING, 100), stat(st::RUNNING, 777)];
        let e = run_op(&mut s, &run_t(100), Op::Restart, 10, &|| false).unwrap_err();
        assert!(e.contains("was stopped, but not started again"), "{e}");
        assert_eq!(s.calls, vec!["stop"]);
    }

    #[test]
    fn restart_with_running_dependents_stops_nothing_and_never_starts() {
        let mut s = FakeSvc::new(vec![stat(st::RUNNING, 100)]);
        s.stop_err = Some((1051, "dependents".into(), vec!["Alpha".into()]));
        let e = run_op(&mut s, &run_t(100), Op::Restart, 10, &|| false).unwrap_err();
        assert!(e.contains("Alpha") && e.contains("does not stop them"), "{e}");
        assert_eq!(s.calls, vec!["stop"], "no start, no cascade");
    }

    #[test]
    fn stop_timeout_in_pending_is_reported_not_forced() {
        let mut s = FakeSvc::new(vec![stat(st::RUNNING, 100), stat(st::STOP_PENDING, 100)]);
        let e = run_op(&mut s, &run_t(100), Op::Restart, 8, &|| false).unwrap_err();
        assert!(e.contains("still stopping") && e.contains("does not kill") && e.contains("not started again"), "{e}");
        assert_eq!(s.calls, vec!["stop"]);
        assert_eq!(s.sleeps, 8, "bounded polling");
    }

    #[test]
    fn cancel_ends_the_wait_not_the_service() {
        use std::cell::Cell;
        let polls = Cell::new(0);
        let mut s = FakeSvc::new(vec![stat(st::RUNNING, 100), stat(st::STOP_PENDING, 100)]);
        let e = run_op(&mut s, &run_t(100), Op::Stop, 100, &|| {
            polls.set(polls.get() + 1);
            polls.get() > 3
        })
        .unwrap_err();
        assert!(e.contains("stopped waiting") && e.contains("still working"), "{e}");
        assert_eq!(s.calls, vec!["stop"], "no second request, nothing killed");
        assert!(s.sleeps < 100);
    }

    #[test]
    fn start_that_exits_again_is_a_failure() {
        let mut s = FakeSvc::new(vec![stat(st::STOPPED, 0), stat(st::START_PENDING, 0), stat(st::STOPPED, 0)]);
        let e = run_op(&mut s, &run_t(0), Op::Start, 10, &|| false).unwrap_err();
        assert!(e.contains("stopped again right after starting"), "{e}");
    }

    #[test]
    fn restart_that_cannot_start_again_says_it_is_stopped() {
        let mut s = FakeSvc::new(vec![stat(st::RUNNING, 100), stat(st::STOPPED, 0)]);
        s.start_err = Some((1058, "disabled".into()));
        let e = run_op(&mut s, &run_t(100), Op::Restart, 10, &|| false).unwrap_err();
        assert!(e.contains("was stopped, but starting it again failed") && e.contains("Disabled") && e.contains("now stopped"), "{e}");
    }

    #[test]
    fn already_in_state_is_success_without_a_call() {
        let mut s = FakeSvc::new(vec![stat(st::STOPPED, 0)]);
        let r = run_op(&mut s, &run_t(100), Op::Stop, 10, &|| false).unwrap();
        assert!(r.contains("already stopped"));
        assert!(s.calls.is_empty());
        assert_eq!(s.ci, 0, "nothing to send, nothing to validate");
    }

    // --- ops validate the captured target on their own handle before any request

    /// The Start the user clicked on service X (stopped, Manual, image x.exe) must never reach
    /// a different service that now owns the name. The old code only read the state.
    #[test]
    fn start_of_a_replaced_stopped_service_is_refused_before_the_request() {
        let orig = cfg(3, false);
        let t = target(&orig);
        let mut s = FakeSvc::new(vec![stat(st::STOPPED, 0)]);
        s.cfgs = vec![replacement()];
        let e = run_op(&mut s, &t, Op::Start, 10, &|| false).unwrap_err();
        assert!(e.contains("no longer the service you picked") && e.contains("nothing done") && e.contains("other\\thing.exe"), "{e}");
        assert!(s.calls.is_empty(), "StartService was never sent to the newcomer");
    }

    #[test]
    fn stop_and_restart_of_a_replaced_running_service_are_refused_too() {
        let orig = cfg(2, false);
        let mut t = target(&orig);
        t.pid = 0; // a stop must not rely on the PID check to catch this
        for op in [Op::Stop, Op::Restart] {
            let mut s = FakeSvc::new(vec![stat(st::RUNNING, 55)]);
            s.cfgs = vec![replacement()];
            let e = run_op(&mut s, &t, op, 10, &|| false).unwrap_err();
            assert!(e.contains("no longer the service you picked"), "{op:?}: {e}");
            assert!(s.calls.is_empty(), "{op:?}: ControlService was never sent");
        }
    }

    #[test]
    fn restart_whose_service_is_replaced_after_the_stop_is_not_started_again() {
        let orig = cfg(2, false);
        let mut t = target(&orig);
        t.pid = 100;
        let mut s = FakeSvc::new(vec![stat(st::RUNNING, 100), stat(st::STOPPED, 0)]);
        // first check (before the stop) is the real service, the second (before the start) is a newcomer
        s.cfgs = vec![orig.clone(), replacement()];
        let e = run_op(&mut s, &t, Op::Restart, 10, &|| false).unwrap_err();
        assert_eq!(s.calls, vec!["stop"], "the replacement is not started");
        assert!(e.contains("was stopped, but not started again") && e.contains("no longer the service you picked"), "{e}");
    }

    #[test]
    fn an_op_on_a_service_whose_startup_type_changed_is_refused() {
        let orig = cfg(3, false);
        let t = target(&orig);
        let mut s = FakeSvc::new(vec![stat(st::STOPPED, 0)]);
        s.cfgs = vec![cfg(4, false)];
        let e = run_op(&mut s, &t, Op::Start, 10, &|| false).unwrap_err();
        assert!(e.contains("startup type changed"), "{e}");
        assert!(s.calls.is_empty());
        // and a config that cannot be re-read is a refusal, not a guess
        struct NoCfg(FakeSvc);
        impl Svc for NoCfg {
            fn status(&mut self) -> Result<Status, ScmError> {
                self.0.status()
            }
            fn config(&mut self) -> Result<Config, ScmError> {
                Err(ScmError::Denied)
            }
            fn stop(&mut self) -> Result<(), (u32, String, Vec<String>)> {
                panic!("sent without validating")
            }
            fn start(&mut self) -> Result<(), (u32, String)> {
                panic!("sent without validating")
            }
            fn sleep(&mut self) {}
        }
        let e = run_op(&mut NoCfg(FakeSvc::new(vec![stat(st::STOPPED, 0)])), &t, Op::Start, 10, &|| false).unwrap_err();
        assert!(e.contains("could not re-read it first") && e.contains("access denied"), "{e}");
    }

    // --- one piece of work per service, bounded workers

    static TEST_REG: Registry = Registry(Mutex::new(None));
    static CAP_REG: Registry = Registry(Mutex::new(None));
    static ORDER_REG: Registry = Registry(Mutex::new(None));

    #[test]
    fn only_one_piece_of_work_per_service_whatever_it_is_and_cancel_reaches_it() {
        let key = "busy-test-key";
        let (g, c) = TEST_REG.take(key, Work::Op(Op::Stop), MAX_WORKERS).ok().unwrap();
        assert_eq!(TEST_REG.get("BUSY-test-KEY"), Some(Work::Op(Op::Stop)));
        assert_eq!(TEST_REG.take(key, Work::Op(Op::Start), MAX_WORKERS).err(), Some(Refused::Busy(Work::Op(Op::Stop))), "second op refused");
        assert_eq!(TEST_REG.take("BUSY-Test-Key", Work::Startup, MAX_WORKERS).err(), Some(Refused::Busy(Work::Op(Op::Stop))), "a startup change is refused while an op runs");
        assert!(!c.load(Ordering::Relaxed));
        drop(g);
        assert_eq!(TEST_REG.get(key), None);
        // and the other way round: an op is refused while a startup change runs
        let (g, _) = TEST_REG.take(key, Work::Startup, MAX_WORKERS).ok().unwrap();
        assert_eq!(TEST_REG.take(key, Work::Op(Op::Start), MAX_WORKERS).err(), Some(Refused::Busy(Work::Startup)));
        assert_eq!(TEST_REG.take(key, Work::Startup, MAX_WORKERS).err(), Some(Refused::Busy(Work::Startup)), "a second startup change is refused");
        drop(g);
        assert!(TEST_REG.take(key, Work::Op(Op::Start), MAX_WORKERS).is_ok());
        // the public cancel flag path
        let k = "svcctl-test-cancel-key";
        let (g, c) = BUSY.take(k, Work::Op(Op::Restart), MAX_WORKERS).ok().unwrap();
        cancel_wait("SVCCTL-test-CANCEL-key");
        assert!(c.load(Ordering::Relaxed));
        assert_eq!(busy_work(k), Some(Work::Op(Op::Restart)));
        drop(g);
        assert_eq!(busy_work(k), None);
    }

    /// The public entry points are wired to the shared guard: an op while a startup change runs, and a
    /// startup change while an op runs, are each refused on the calling thread.
    #[test]
    fn the_public_spawns_share_one_guard() {
        let c = Config { name: "WiringSvc".into(), ..cfg(3, false) };
        let t = target(&c);
        let g = hold_for_test(&t.key(), Work::Startup);
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_op(t.clone(), Op::Start, move |r| tx.send(r).unwrap());
        let e = rx.try_recv().expect("answered at once").unwrap_err();
        assert!(e.contains("already busy") && e.contains("changing its startup type"), "{e}");
        drop(g);
        let g = hold_for_test(&t.key(), Work::Op(Op::Restart));
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_startup(t.clone(), StartKind::Manual, StartKind::Disabled, move |r| tx.send(r.map(|_| ())).unwrap());
        let e = rx.try_recv().expect("answered at once").unwrap_err();
        assert!(e.starts_with("not changed") && e.contains("already busy") && e.contains("restarting"), "{e}");
        drop(g);
    }

    #[test]
    fn worker_concurrency_is_bounded() {
        let (a, _) = CAP_REG.take("a", Work::Startup, 2).ok().unwrap();
        let (_b, _) = CAP_REG.take("b", Work::Op(Op::Stop), 2).ok().unwrap();
        assert_eq!(CAP_REG.take("c", Work::Startup, 2).err(), Some(Refused::Full), "a third worker is refused, not queued");
        assert_eq!(CAP_REG.take("a", Work::Startup, 2).err(), Some(Refused::Busy(Work::Startup)), "per-service busy wins over full");
        assert!(Refused::Full.message("Svc").contains("already in progress"));
        drop(a);
        assert!(CAP_REG.take("c", Work::Startup, 2).is_ok());
    }

    /// A startup change that has to roll back is one unit: its write, put-back and read-back all
    /// happen while the service is busy; a second startup change or an op meanwhile is refused AT ONCE
    /// (the answer arrives before the call returns); the service is idle again before the result is
    /// reported. The old code had no guard on startup changes at all.
    #[test]
    fn a_rolling_back_startup_change_blocks_everything_else_until_it_is_finished() {
        use std::sync::mpsc;
        let key = "fixture";
        let c = cfg(3, false);
        let t = target(&c);
        let f = Fake::new(c);
        {
            let mut w = f.w();
            w.fail_delayed = Some(ScmError::Other("invalid parameter".into()));
            w.busy_probe = Some(Box::new(move || ORDER_REG.get(key).is_some()));
        }
        let (in_tx, in_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        f.set_hook(move |_, ev| {
            if ev == "set_delayed" {
                // in the middle of the transaction: start=2 has landed, the flag write is about to fail
                in_tx.send(()).unwrap();
                go_rx.lock().unwrap().recv().unwrap();
            }
        });
        let (done_tx, done_rx) = mpsc::channel::<(Result<StartupDone, String>, Option<Work>)>();
        let mut scm = f.clone();
        let t2 = t.clone();
        spawn_guarded_in(
            &ORDER_REG,
            MAX_WORKERS,
            &t.name,
            t.key(),
            Work::Startup,
            move |_| apply_startup(&mut scm, &t2, StartKind::Manual, StartKind::AutomaticDelayed),
            |m| Err(format!("not changed: {m}")),
            move |r| {
                // reported AFTER the service was freed
                done_tx.send((r, ORDER_REG.get(key))).unwrap();
            },
        );
        in_rx.recv().unwrap();
        assert_eq!(ORDER_REG.get(key), Some(Work::Startup), "busy while the transaction is open");

        // a second startup change (the menu clicked again, or the panel closed and reopened): immediate refusal
        let answered = Arc::new(AtomicBool::new(false));
        let a2 = answered.clone();
        let (r_tx, r_rx) = mpsc::channel::<Result<StartupDone, String>>();
        spawn_guarded_in(&ORDER_REG, MAX_WORKERS, &t.name, t.key(), Work::Startup, |_| panic!("must not run"), |m| Err(format!("not changed: {m}")), move |r| {
            a2.store(true, Ordering::SeqCst);
            r_tx.send(r).unwrap();
        });
        assert!(answered.load(Ordering::SeqCst), "answered on the calling thread: no wait");
        let e = r_rx.recv().unwrap().unwrap_err();
        assert!(e.starts_with("not changed") && e.contains("already busy") && e.contains("changing its startup type"), "{e}");
        // and an op on the same service
        let (o_tx, o_rx) = mpsc::channel::<Result<String, String>>();
        spawn_guarded_in(&ORDER_REG, MAX_WORKERS, &t.name, t.key(), Work::Op(Op::Start), |_| panic!("must not run"), Err, move |r| o_tx.send(r).unwrap());
        assert!(o_rx.try_recv().unwrap().unwrap_err().contains("already busy"));

        go_tx.send(()).unwrap();
        let (res, busy_at_report) = done_rx.recv().unwrap();
        let e = res.unwrap_err();
        assert!(e.contains("put back") && e.contains("now Manual"), "{e}");
        assert_eq!(busy_at_report, None, "the service is idle before the result is reported");
        assert_eq!(ORDER_REG.get(key), None);
        let w = f.w();
        assert_eq!(
            w.writes.iter().map(|(_, s, busy)| (s.as_str(), *busy)).collect::<Vec<_>>(),
            vec![("start=2", true), ("start=3", true)],
            "the write and the put-back both ran while the guard was held; the refused attempts wrote nothing"
        );
        assert_eq!(w.insts[0].cfg.start, 3);
    }

    #[test]
    fn same_service_compares_key_and_image() {
        let c = cfg(2, false);
        let mut t = target(&c);
        assert!(same_service(&t, &c));
        t.path.clear();
        assert!(!same_service(&t, &c), "no captured path: nothing to compare, so not the same");
        assert!(same_name(&t, &c));
        t.path = c.path.clone();
        t.name = "Other".into();
        assert!(!same_service(&t, &c));
    }

    /// Real SCM, real service. Needs a disposable fixture service named by
    /// `BLINK_SVC_FIXTURE` that the current user may configure (the test machine run sets
    /// its own ACL on it); everything else is skipped. See docs/services.md.
    #[test]
    #[ignore = "needs BLINK_SVC_FIXTURE (a disposable service this user may configure)"]
    fn real_scm_startup_round_trip_changes_nothing_else() {
        let Ok(name) = std::env::var("BLINK_SVC_FIXTURE") else { return };
        assert!(name.to_lowercase().starts_with("blinkfixture"), "refusing to touch a service that is not a Hayate fixture");
        let mut scm = Real;
        let c0 = read_once(&mut scm, &name).expect("fixture readable");
        let t = Target { name: name.clone(), display: c0.display.clone(), path: c0.path.clone(), state: c0.status.state, pid: c0.status.pid, can_stop: true, start: c0.start, delayed: c0.delayed.unwrap_or(false), trigger: false };
        let mut kind = c0.kind().expect("fixture has a changeable start type");
        for want in [StartKind::Disabled, StartKind::AutomaticDelayed, StartKind::Manual, StartKind::Automatic, StartKind::Disabled, StartKind::Manual] {
            if want == kind {
                continue;
            }
            let done = apply_startup(&mut scm, &t, kind, want).unwrap_or_else(|e| panic!("{kind:?} → {want:?}: {e}"));
            assert_eq!(done.after.kind(), Some(want));
            assert_eq!(done.after.without_startup(), c0.without_startup(), "{want:?} changed something else");
            kind = want;
        }
        // restore what we found
        let _ = apply_startup(&mut scm, &t, kind, c0.kind().unwrap());
        assert_eq!(read_once(&mut scm, &name).unwrap().start, c0.start);
        // identity: a wrong image is refused on the real SCM too
        let mut bad = t.clone();
        bad.path = "C:\\nope\\nope.exe".into();
        assert!(apply_startup(&mut scm, &bad, kind, StartKind::Disabled).unwrap_err().contains("no longer the service"));
        // a name that does not exist fails closed
        let mut gone = t.clone();
        gone.name = "BlinkFixtureDoesNotExist".into();
        assert!(apply_startup(&mut scm, &gone, kind, StartKind::Disabled).unwrap_err().contains("no longer exists"));
    }
}
