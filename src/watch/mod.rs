//! Watch: services/drivers, new/updated/unsigned apps, camera/mic/location
//! (AppControl replacement). All push-driven, ~0 CPU idle.
//!
//! Threads:
//! - `hayate-watch` worker (THREAD_MODE_BACKGROUND): owns service cache, seen
//!   table, signature cache; turns raw messages into events + toasts.
//! - `hayate-watch-notify`: alertable wait for SCM created/deleted APCs and
//!   the 6 ConsentStore registry notifications.
//! - EvtSubscribe callback (system thread pool) and the ETW thread only post
//!   messages to the worker.
#![allow(dead_code)]

pub mod apps;
pub mod attrib;
pub mod catchup;
pub mod consent;
pub mod evt;
pub mod scm;
pub mod sig;
pub mod store;
pub mod toast;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::*;

pub use store::{Event, Kind, SigState};
pub use toast::{AlertType, Toast};

use crate::etw::{self, ProcEvent, ProcStart};
use crate::osutil;
use consent::{Cap, CapChange};

pub enum Msg {
    Proc(ProcStart),
    Scm(evt::ScmEvent),
    ScmNames(Vec<(bool, String)>),
    Cap { cap: Cap, machine: bool, change: CapChange },
    Quit,
}

// ---- settings --------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub alerts: HashMap<AlertType, bool>,
    /// dev-build / temp ignore globs for new-app tracking (`%VAR%` allowed)
    pub ignore: Vec<String>,
    /// unix s; toasts dropped until then
    pub dnd_until: u64,
    /// unix s of the first Watch run
    pub first_run: u64,
    /// first N hours: new-app events are logged but not toasted (every app is "new" on day one)
    pub learning_hours: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            alerts: AlertType::ALL.iter().map(|a| (*a, a.default_notify())).collect(),
            ignore: apps::DEFAULT_IGNORE.iter().map(|s| s.to_string()).collect(),
            dnd_until: 0,
            first_run: 0,
            learning_hours: 24,
        }
    }
}

impl Config {
    pub fn to_text(&self) -> String {
        let mut s = String::from("; Hayate Watch settings\n\n[watch]\n");
        s.push_str(&format!("first_run = {}\ndnd_until = {}\nlearning_hours = {}\n\n[alerts]\n", self.first_run, self.dnd_until, self.learning_hours));
        for a in AlertType::ALL {
            s.push_str(&format!("{} = {}\n", a.key(), if self.alerts.get(&a).copied().unwrap_or(true) { "notify" } else { "log" }));
        }
        s.push_str("\n[ignore]\n");
        for g in &self.ignore {
            s.push_str(&format!("glob = {}\n", g));
        }
        s
    }

    pub fn parse(t: &str) -> Config {
        let mut c = Config::default();
        let mut sec = "";
        let mut globs: Option<Vec<String>> = None;
        for line in t.lines().map(str::trim) {
            if line.is_empty() || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') {
                sec = match line {
                    "[watch]" => "watch",
                    "[alerts]" => "alerts",
                    "[ignore]" => {
                        globs.get_or_insert_with(Vec::new);
                        "ignore"
                    }
                    _ => "",
                };
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match sec {
                "watch" => match k {
                    "first_run" => c.first_run = v.parse().unwrap_or(0),
                    "dnd_until" => c.dnd_until = v.parse().unwrap_or(0),
                    "learning_hours" => c.learning_hours = v.parse().unwrap_or(24),
                    _ => {}
                },
                "alerts" => {
                    if let Some(a) = AlertType::parse(k) {
                        c.alerts.insert(a, v != "log");
                    }
                }
                "ignore" if k == "glob" && !v.is_empty() => globs.get_or_insert_with(Vec::new).push(v.to_string()),
                _ => {}
            }
        }
        if let Some(g) = globs {
            c.ignore = g;
        }
        c
    }

    fn expanded_ignore(&self) -> Vec<String> {
        self.ignore.iter().map(|g| apps::expand(g)).collect()
    }
}

// ---- facade ------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Status {
    /// None = OK
    pub eventlog_error: Option<String>,
    pub scm_notify_error: Option<String>,
    /// how many of the 6 ConsentStore keys are watched
    pub consent_keys: usize,
    pub services_cached: usize,
    pub apps_seen: usize,
}

/// Event list filter for the UI.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// empty = all kinds
    pub kinds: Vec<Kind>,
    pub unread_only: bool,
    /// case-insensitive substring of name / path / actor / publisher
    pub text: String,
    pub unsigned_only: bool,
}

impl Filter {
    fn hit(&self, e: &Event) -> bool {
        if !self.kinds.is_empty() && !self.kinds.contains(&e.kind()) {
            return false;
        }
        if self.unread_only && !e.unread {
            return false;
        }
        if self.unsigned_only && e.sig.is_trusted() {
            return false;
        }
        if !self.text.is_empty() {
            let t = self.text.to_lowercase();
            let any = [&e.name, &e.path, &e.actor, &e.publisher, &e.key].iter().any(|s| s.to_lowercase().contains(&t));
            if !any {
                return false;
            }
        }
        true
    }
}

type Cb = Arc<dyn Fn() + Send + Sync>;

pub struct Watch {
    store: Mutex<store::Store>,
    cfg: Mutex<Config>,
    cfg_file: PathBuf,
    toaster: toast::Toaster,
    on_change: Mutex<Option<Cb>>,
    status: Mutex<Status>,
    tx: Mutex<Sender<Msg>>,
    quit: HANDLE,
}

unsafe impl Send for Watch {}
unsafe impl Sync for Watch {}

impl Watch {
    // ---- events

    /// Newest first, at most the last 500.
    pub fn events(&self, f: &Filter) -> Vec<Event> {
        self.store.lock().unwrap().iter().filter(|e| f.hit(e)).cloned().collect()
    }

    pub fn event(&self, id: u64) -> Option<Event> {
        self.store.lock().unwrap().get(id).cloned()
    }

    pub fn unread(&self) -> usize {
        self.store.lock().unwrap().unread()
    }

    pub fn mark_read(&self, id: u64) {
        self.store.lock().unwrap().mark_read(id);
        self.changed();
    }

    pub fn mark_all_read(&self) {
        self.store.lock().unwrap().mark_all_read();
        self.changed();
    }

    // ---- settings

    pub fn config(&self) -> Config {
        self.cfg.lock().unwrap().clone()
    }

    pub fn set_config(&self, c: Config) {
        let mut g = self.cfg.lock().unwrap();
        *g = c;
        // the I/O worker writes it: a click on the UI thread never waits on the disk
        crate::bgio::write_file(self.cfg_file.clone(), g.to_text().into_bytes());
    }

    fn edit_cfg(&self, f: impl FnOnce(&mut Config)) {
        let mut c = self.config();
        f(&mut c);
        self.set_config(c);
    }

    pub fn alerts(&self) -> Vec<(AlertType, bool)> {
        let c = self.cfg.lock().unwrap();
        AlertType::ALL.iter().map(|a| (*a, c.alerts.get(a).copied().unwrap_or(true))).collect()
    }

    pub fn set_alert(&self, a: AlertType, notify: bool) {
        self.edit_cfg(|c| {
            c.alerts.insert(a, notify);
        });
    }

    /// Do not disturb for `secs` (UI offers 3 h); 0 clears.
    pub fn set_dnd(&self, secs: u64) {
        let until = if secs == 0 { 0 } else { osutil::now_unix() + secs };
        self.edit_cfg(|c| c.dnd_until = until);
        if secs > 0 {
            self.toaster.clear();
        }
    }

    pub fn dnd_until(&self) -> u64 {
        self.cfg.lock().unwrap().dnd_until
    }

    pub fn ignore_globs(&self) -> Vec<String> {
        self.cfg.lock().unwrap().ignore.clone()
    }

    pub fn set_ignore_globs(&self, g: Vec<String>) {
        self.edit_cfg(|c| c.ignore = g);
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    // ---- callbacks

    /// Called with each toast to show (tray balloon). Runs on a Watch thread.
    pub fn set_toast_sink(&self, f: impl Fn(Toast) + Send + Sync + 'static) {
        self.toaster.set_sink(Arc::new(f));
    }

    /// Called after events are added or read flags change (e.g. PostMessage a repaint).
    pub fn set_on_change(&self, f: impl Fn() + Send + Sync + 'static) {
        *self.on_change.lock().unwrap() = Some(Arc::new(f));
    }

    fn changed(&self) {
        let cb = self.on_change.lock().unwrap().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    pub fn shutdown(&self) {
        evt::stop();
        unsafe {
            let _ = SetEvent(self.quit);
        }
        let _ = self.tx.lock().unwrap().send(Msg::Quit);
        self.toaster.stop();
    }

    // ---- internals

    fn add(&self, e: Event) -> u64 {
        self.add_as(e, true)
    }

    fn add_as(&self, e: Event, unread: bool) -> u64 {
        let id = self.store.lock().unwrap().push_as(e, unread);
        self.changed();
        id
    }

    /// Memory warning from the sampler (leak suspect / pressure): logs an event
    /// and shows a toast, subject to the same per-type routing and snooze.
    pub fn memory_alert(&self, leak: bool, name: &str, title: &str, text: &str) {
        let (kind, alert) = if leak { (Kind::MemoryLeak, AlertType::MemoryLeak) } else { (Kind::MemoryPressure, AlertType::MemoryPressure) };
        let id = self.add(Event { time: osutil::now_unix(), kind: Some(kind), name: name.into(), detail: text.into(), unread: true, ..Default::default() });
        self.notify(alert, title, text, name, "", Some(id));
    }

    /// A network incident started (the verdict stayed bad long enough, see `netincident`): one
    /// event, no toast. `start` is when the verdict first went bad. Returns the event id.
    pub fn net_incident_open(&self, info: &crate::netincident::Info, start: u64, detail: String) -> u64 {
        self.add(Event { time: start, since: start, kind: Some(Kind::Network), name: info.name(), key: info.remote.clone(), path: info.path.clone(), detail, unread: true, ..Default::default() })
    }

    /// The incident cleared: the same event gets its end time.
    pub fn net_incident_close(&self, id: u64, detail: &str) {
        if self.store.lock().unwrap().update_detail(id, detail) {
            self.changed();
        }
    }

    fn notify(&self, alert: AlertType, title: &str, text: &str, subject: &str, actor: &str, id: Option<u64>) {
        let c = self.cfg.lock().unwrap();
        if !c.alerts.get(&alert).copied().unwrap_or(true) || c.dnd_until > osutil::now_unix() {
            return;
        }
        drop(c);
        self.toaster.push(Toast::new(alert, title, text, subject, actor, id));
    }

    fn learning(&self) -> bool {
        let c = self.cfg.lock().unwrap();
        osutil::now_unix() < c.first_run + c.learning_hours * 3600
    }
}

fn sig_text(s: &sig::Sig) -> String {
    match s.state {
        SigState::Signed | SigState::Catalog if !s.publisher.is_empty() => format!("{} · signed", s.publisher),
        SigState::Signed | SigState::Catalog => "signed".into(),
        SigState::Invalid => format!("{} · INVALID signature", if s.publisher.is_empty() { "?" } else { &s.publisher }),
        SigState::Unsigned => "UNSIGNED".into(),
        SigState::Unknown => "signature unknown".into(),
    }
}

// ---- worker ------------------------------------------------------------------------

struct Worker {
    w: Arc<Watch>,
    services: HashMap<String, scm::SvcInfo>,
    apps: apps::Apps,
    sigs: sig::Cache,
    ignore: Vec<String>,
    last_record: u64,
    sysroot: String,
    /// decides which caught-up System-log events may toast
    gate: catchup::Gate,
}

impl Worker {
    /// Log a System-log event and toast it as the catch-up gate allows:
    /// live now, recent caught-up ones batched, old ones never (logged read).
    #[allow(clippy::too_many_arguments)]
    fn emit(&mut self, event_ms: u64, e: Event, alert: AlertType, title: &str, text: &str, subject: &str, actor: &str) {
        let v = self.gate.classify(event_ms);
        let id = self.w.add_as(e, v != catchup::Verdict::Old);
        let now = osutil::now_ms();
        match v {
            catchup::Verdict::Live => {
                self.flush_catchup();
                self.w.notify(alert, title, text, subject, actor, Some(id));
            }
            catchup::Verdict::CatchUp => self.gate.hold(now, catchup::Held { alert, title: title.into(), text: text.into(), subject: subject.into(), actor: actor.into(), id: Some(id) }),
            catchup::Verdict::Old => self.gate.skip_old(now),
        }
    }

    /// End of a catch-up batch: release its toasts, or one summary if too many.
    fn flush_catchup(&mut self) {
        if !self.gate.pending() {
            return;
        }
        let f = self.gate.flush();
        let n = f.toasts.len();
        for h in f.toasts {
            self.w.notify(h.alert, &h.title, &h.text, &h.subject, &h.actor, h.id);
        }
        if let Some((count, alert)) = f.summary {
            self.w.notify(alert, &format!("Watch: {} events while Hayate was closed", count), "Logged in Watch (unread), not shown one by one.", "Watch catch-up", "", None);
        }
        osutil::log(&format!("watch catch-up: {} toasted, {} summarized, {} old logged silently", n, f.summary.map_or(0, |s| s.0), f.old));
    }
    fn attribution(&self, path: &str, event_ms: u64) -> Option<attrib::Attribution> {
        if osutil::now_ms().saturating_sub(event_ms) > 60_000 {
            return None; // catch-up event from before Hayate ran
        }
        attrib::attribute(path, event_ms, &etw::recent_starts(), &self.sysroot, &|pid| osutil::image_path(pid))
    }

    fn actor_text(&mut self, a: &Option<attrib::Attribution>) -> (String, bool, String) {
        match a {
            Some(a) => {
                let s = self.sig(&a.actor_path);
                let p = if s.publisher.is_empty() { String::new() } else { format!(" · {}", s.publisher) };
                (a.actor.clone(), a.likely, p)
            }
            None => (String::new(), false, String::new()),
        }
    }

    fn on_proc(&mut self, p: ProcStart) {
        if !p.path.contains('\\') || apps::ignored(&p.path, &self.ignore) {
            return;
        }
        if !self.apps.should_check(&p.path, osutil::now_ms()) {
            return;
        }
        let Some(st) = sig::stat(&p.path) else { return };
        let Some(change) = self.apps.compare(&p.path, st) else { return };
        let version = sig::file_version(&p.path);
        let s = self.sigs.get(&p.path);
        self.apps.record(apps::Seen { path: p.path.clone(), size: st.0, mtime: st.1, version: version.clone(), sig: s.clone() });
        let name = osutil::file_name(&p.path).to_string();
        let parent = etw::recent_starts().into_iter().rev().find(|q| q.pid == p.ppid).map(|q| q.path).or_else(|| osutil::image_path(p.ppid)).map(|x| osutil::file_name(&x).to_string()).unwrap_or_default();
        let (kind, detail) = match &change {
            apps::Change::New => (Kind::NewApp, if version.is_empty() { String::new() } else { format!("v{}", version) }),
            apps::Change::Updated(old, _) if !old.is_empty() && old != &version => (Kind::AppUpdated, format!("{} → {}", old, version)),
            apps::Change::Updated(..) => (Kind::AppUpdated, "file changed".into()),
        };
        let id = self.w.add(Event {
            time: p.time / 1000,
            kind: Some(kind),
            name: name.clone(),
            path: p.path.clone(),
            detail: detail.clone(),
            actor: parent.clone(),
            publisher: s.publisher.clone(),
            sig: s.state,
            ..Default::default()
        });
        if kind == Kind::NewApp && self.w.learning() {
            return;
        }
        let bad = matches!(s.state, SigState::Unsigned | SigState::Invalid);
        let title = format!("{}: {}", kind.label(), name);
        let text = format!("{}{}", if detail.is_empty() { String::new() } else { format!("{} · ", detail) }, sig_text(&s));
        let base = if kind == Kind::NewApp { AlertType::NewApp } else { AlertType::AppUpdated };
        let cfg = self.w.config();
        let on = |a: AlertType| cfg.alerts.get(&a).copied().unwrap_or(true);
        if bad && on(AlertType::Unsigned) {
            self.w.notify(AlertType::Unsigned, &format!("Unsigned app: {}", name), &text, &name, &parent, Some(id));
        } else {
            self.w.notify(base, &title, &text, &name, &parent, Some(id));
        }
    }

    fn on_scm(&mut self, e: evt::ScmEvent) {
        if e.record != 0 && e.record <= self.last_record {
            return;
        }
        self.last_record = e.record.max(self.last_record);
        let time = if e.time == 0 { osutil::now_unix() } else { e.time / 1000 };
        match e.id {
            7045 => {
                let name = e.get("ServiceName").to_string();
                let info = scm::read_service(&name).unwrap_or_else(|| scm::SvcInfo {
                    name: name.clone(),
                    display: name.clone(),
                    path: osutil::clean_image_path(e.get("ImagePath")),
                    kind: if e.get("ServiceType").to_lowercase().contains("driver") { 1 } else { 0x10 },
                    start: u32::MAX,
                });
                self.services.insert(name.to_lowercase(), info.clone());
                let driver = info.is_driver();
                let a = self.attribution(&info.path, e.time);
                let (actor, likely, actor_pub) = self.actor_text(&a);
                let s = self.sig(&info.path);
                let acct = e.get("AccountName");
                let detail = format!("{} · {} start{}", if driver { "kernel driver" } else { "service" }, e.get("StartType"), if acct.is_empty() { String::new() } else { format!(" · {}", acct) });
                let kind = if driver { Kind::DriverInstalled } else { Kind::ServiceInstalled };
                let ev = Event {
                    time,
                    kind: Some(kind),
                    name: info.display.clone(),
                    key: name.clone(),
                    path: info.path.clone(),
                    detail,
                    actor: actor.clone(),
                    actor_likely: likely,
                    publisher: s.publisher.clone(),
                    sig: s.state,
                    ..Default::default()
                };
                let by = if actor.is_empty() { String::new() } else { format!("by {}{}{} · ", actor, actor_pub, if likely { " (likely)" } else { "" }) };
                self.emit(e.time, ev, AlertType::ServiceInstalled, &format!("{}: {}", kind.label(), info.display), &format!("{}{}", by, sig_text(&s)), &info.display, &actor);
            }
            7040 => {
                let key = e.get("param4").to_string();
                let info = scm::read_service(&key).or_else(|| self.services.get(&key.to_lowercase()).cloned()).unwrap_or_default();
                if !key.is_empty() && !info.name.is_empty() {
                    self.services.insert(key.to_lowercase(), info.clone());
                }
                let display = if e.get("param1").is_empty() { info.display.clone() } else { e.get("param1").to_string() };
                let detail = format!("{} → {}", e.get("param2"), e.get("param3"));
                let a = self.attribution(&info.path, e.time);
                let (actor, likely, _) = self.actor_text(&a);
                let s = if info.path.is_empty() { sig::Sig::default() } else { self.sig(&info.path) };
                let ev = Event {
                    time,
                    kind: Some(Kind::StartTypeChanged),
                    name: display.clone(),
                    key,
                    path: info.path.clone(),
                    detail: detail.clone(),
                    actor: actor.clone(),
                    actor_likely: likely,
                    publisher: s.publisher,
                    sig: s.state,
                    ..Default::default()
                };
                let by = if actor.is_empty() { String::new() } else { format!(" · by {}{}", actor, if likely { " (likely)" } else { "" }) };
                self.emit(e.time, ev, AlertType::StartTypeChanged, &format!("Start type changed: {}", display), &format!("{}{}", detail, by), &display, &actor);
            }
            7036 => {
                let Some((key, state)) = e.state_binary() else { return };
                let kind = match state {
                    4 => Kind::ServiceStarted,
                    1 => Kind::ServiceStopped,
                    _ => return,
                };
                let info = self.services.get(&key.to_lowercase()).cloned().unwrap_or_default();
                let display = if e.get("param1").is_empty() { key.clone() } else { e.get("param1").to_string() };
                let ev = Event { time, kind: Some(kind), name: display.clone(), key, path: info.path, ..Default::default() };
                self.emit(e.time, ev, AlertType::ServiceStartStop, &format!("{}: {}", kind.label(), display), "", &display, "");
            }
            7034 => {
                let display = e.get("param1").to_string();
                let info = self.services.values().find(|s| s.display == display).cloned().unwrap_or_default();
                let detail = format!("terminated unexpectedly ({} times)", e.get("param2"));
                let ev = Event { time, kind: Some(Kind::ServiceCrashed), name: display.clone(), key: info.name, path: info.path, detail: detail.clone(), ..Default::default() };
                self.emit(e.time, ev, AlertType::ServiceCrashed, &format!("Service crashed: {}", display), &detail, &display, "");
            }
            _ => {}
        }
    }

    fn on_names(&mut self, v: Vec<(bool, String)>) {
        let now = osutil::now_ms();
        for (created, name) in v {
            if created {
                if let Some(i) = scm::read_service(&name) {
                    self.services.insert(name.to_lowercase(), i);
                }
                continue;
            }
            let info = self.services.remove(&name.to_lowercase()).unwrap_or_else(|| scm::SvcInfo { name: name.clone(), display: name.clone(), ..Default::default() });
            let driver = info.is_driver();
            let a = self.attribution(&info.path, now);
            let (actor, likely, _) = self.actor_text(&a);
            let kind = if driver { Kind::DriverRemoved } else { Kind::ServiceRemoved };
            let id = self.w.add(Event {
                time: now / 1000,
                kind: Some(kind),
                name: info.display.clone(),
                key: name.clone(),
                path: info.path.clone(),
                detail: if info.start != u32::MAX && info.kind != 0 { format!("was {} start", scm::start_label(info.start)) } else { String::new() },
                actor: actor.clone(),
                actor_likely: likely,
                ..Default::default()
            });
            let by = if actor.is_empty() { info.path.clone() } else { format!("by {}{}", actor, if likely { " (likely)" } else { "" }) };
            self.w.notify(AlertType::ServiceRemoved, &format!("{}: {}", kind.label(), info.display), &by, &info.display, &actor, Some(id));
        }
    }

    fn on_cap(&mut self, cap: Cap, machine: bool, change: CapChange) {
        let (alert, kind) = match cap {
            Cap::Camera => (AlertType::Camera, Kind::Camera),
            Cap::Microphone => (AlertType::Microphone, Kind::Microphone),
            Cap::Location => (AlertType::Location, Kind::Location),
        };
        let sys = if machine { " (system)" } else { "" };
        match change {
            CapChange::Started { app } => {
                let (name, _) = consent::app_name(&app);
                self.w.notify(alert, &format!("{}{} is using the {}", name, sys, cap.noun()), "", &name, "", None);
            }
            CapChange::Stopped { app, start, stop } => {
                let (name, path) = consent::app_name(&app);
                let s = osutil::filetime_to_ms(start as i64) / 1000;
                let t = osutil::filetime_to_ms(stop as i64) / 1000;
                let dur = t.saturating_sub(s);
                let d = if dur < 60 { format!("{} s", dur) } else { format!("{} min", dur / 60) };
                self.w.add(Event { time: t, since: s, kind: Some(kind), name: format!("{}{}", name, sys), key: app.clone(), path, detail: format!("used {} for {}", cap.noun(), d), ..Default::default() });
            }
        }
    }

    /// First run: put everything running now into the seen table silently.
    fn seed_running(&mut self) -> usize {
        seed(&mut self.apps, running_images(), &self.ignore)
    }

    /// Signature of `path` (cached). A verdict for a seeded seen-table entry
    /// (stored "unknown") is written back, so the next start has it too.
    fn sig(&mut self, path: &str) -> sig::Sig {
        let s = self.sigs.get(path);
        if self.apps.wants_sig(path) {
            if let Some(st) = sig::stat(path) {
                self.apps.learn_sig(path, st, &s);
            }
        }
        s
    }

    fn run(mut self, rx: Receiver<Msg>) {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
        self.services = crate::timing::time("watch worker: services read_all", scm::read_all);
        for s in self.apps.entries() {
            self.sigs.seed(&s.path, (s.size, s.mtime), s.sig.clone());
        }
        if self.apps.first_run {
            let n = crate::timing::time("watch worker: seed_running (first run, no signature checks)", || self.seed_running());
            osutil::log(&format!("watch: first run, {} running images seeded (signatures checked lazily)", n));
        }
        {
            let mut st = self.w.status.lock().unwrap();
            st.services_cached = self.services.len();
            st.apps_seen = self.apps.len();
        }
        loop {
            let m = if self.gate.pending() {
                // a catch-up batch is open: wake up to close it when idle
                match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                    Ok(m) => Some(m),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => break,
                }
            };
            if self.gate.due(osutil::now_ms()) {
                self.flush_catchup();
            }
            let Some(m) = m else { continue };
            match m {
                Msg::Proc(p) => {
                    // ignore list may have been edited in the UI
                    self.ignore = self.w.config().expanded_ignore();
                    self.on_proc(p)
                }
                Msg::Scm(e) => self.on_scm(e),
                Msg::ScmNames(v) => self.on_names(v),
                Msg::Cap { cap, machine, change } => self.on_cap(cap, machine, change),
                Msg::Quit => break,
            }
            self.w.status.lock().unwrap().apps_seen = self.apps.len();
        }
    }
}

/// Image paths of every running process (plus starts ETW saw in the last
/// minute), one per seen-table key: 340 processes are ~100 distinct images
/// (svchost, conhost, RuntimeBroker ... share one).
fn running_images() -> Vec<String> {
    let mut paths: Vec<String> = etw::recent_starts().into_iter().map(|p| p.path).collect();
    // EnumProcesses would be cheaper to declare, but Toolhelp is already linked
    unsafe {
        use windows::Win32::System::Diagnostics::ToolHelp::*;
        if let Ok(h) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let h = osutil::Handle(h);
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(h.0, &mut e).is_ok() {
                loop {
                    if let Some(p) = osutil::image_path(e.th32ProcessID) {
                        paths.push(p);
                    }
                    if Process32NextW(h.0, &mut e).is_err() {
                        break;
                    }
                }
            }
        }
    }
    dedup_by_key(paths)
}

/// Record `paths` in the seen table by identity only (size, mtime, version),
/// signature "unknown": no WinVerifyTrust. Seeding only answers "was this
/// image here before?"; Authenticode + catalog hashing of every running
/// image cost ~8 s CPU for 160 images. Signatures are checked when an event
/// needs one (`Worker::sig`): a change to a seeded file is an update, and
/// on_proc verifies the new file then. Returns how many were added.
fn seed(apps: &mut apps::Apps, paths: Vec<String>, ignore: &[String]) -> usize {
    let mut v = Vec::new();
    for p in paths {
        if apps::ignored(&p, ignore) || apps.get(&p).is_some() {
            continue;
        }
        if let Some(st) = sig::stat(&p) {
            let version = sig::file_version(&p);
            v.push(apps::Seen { path: p, size: st.0, mtime: st.1, version, sig: sig::Sig::default() });
        }
    }
    let n = v.len();
    apps.record_all(v);
    n
}

/// Keep the first path per seen-table key (case-insensitive, hash-collapsed).
fn dedup_by_key(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    paths.into_iter().filter(|p| seen.insert(apps::key(p))).collect()
}

// ---- notify thread ---------------------------------------------------------------------

fn notify_thread(w: Arc<Watch>, tx: Sender<Msg>) {
    let notifier = match scm::Notifier::open() {
        Ok(n) => Some(n),
        Err(e) => {
            w.status.lock().unwrap().scm_notify_error = Some(e);
            None
        }
    };
    let mut keys: Vec<consent::Watched> = Vec::new();
    for cap in Cap::ALL {
        for machine in [false, true] {
            if let Some(k) = consent::Watched::open(cap, machine) {
                keys.push(k);
            }
        }
    }
    w.status.lock().unwrap().consent_keys = keys.len();
    loop {
        let mut hs = vec![w.quit];
        hs.extend(keys.iter().map(|k| k.event));
        let r = unsafe { WaitForMultipleObjectsEx(&hs, false, INFINITE, true) };
        if r == WAIT_IO_COMPLETION {
            let names: Vec<(bool, String)> = std::mem::take(&mut *scm::PENDING.lock().unwrap());
            if let Some(n) = &notifier {
                if let Err(e) = n.arm() {
                    w.status.lock().unwrap().scm_notify_error = Some(e);
                }
            }
            if !names.is_empty() {
                let _ = tx.send(Msg::ScmNames(names));
            }
            continue;
        }
        let i = r.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
        if i == 0 || i >= hs.len() {
            break; // quit or failure
        }
        let k = &mut keys[i - 1];
        for change in k.changed() {
            let _ = tx.send(Msg::Cap { cap: k.cap, machine: k.machine, change });
        }
    }
    drop(notifier);
}

/// Load store + settings, start the worker, notify thread, System-log
/// subscription, and hook ETW process starts. Call before `etw::start()`.
pub fn start() -> Arc<Watch> {
    let start_ms = osutil::now_ms();
    let dir = osutil::data_dir();
    let cfg_file = dir.join("watch.ini");
    let mut cfg = std::fs::read_to_string(&cfg_file).map(|t| Config::parse(&t)).unwrap_or_default();
    let first = cfg.first_run == 0;
    if first {
        cfg.first_run = osutil::now_unix();
        let _ = osutil::write_atomic(&cfg_file, cfg.to_text().as_bytes());
    }
    let (tx, rx) = channel();
    let quit = unsafe { CreateEventW(None, true, false, None).unwrap_or_default() };
    let w = Arc::new(Watch {
        store: Mutex::new(crate::timing::time("watch store open", || store::Store::open(dir.join("events.log"), osutil::now_unix()))),
        cfg: Mutex::new(cfg.clone()),
        cfg_file,
        toaster: toast::Toaster::new(),
        on_change: Mutex::new(None),
        status: Mutex::new(Status::default()),
        tx: Mutex::new(tx.clone()),
        quit,
    });

    // Subscribe before the worker runs: caught-up events queue in the channel,
    // and the worker needs to know where the subscription resumed from.
    let saved_ms = match evt::start(tx.clone()) {
        Ok(r) => {
            osutil::log(&format!("watch eventlog: {:?}", r));
            r.saved_ms()
        }
        Err(e) => {
            osutil::log(&format!("watch eventlog error: {}", e));
            w.status.lock().unwrap().eventlog_error = Some(e);
            None
        }
    };

    let worker = Worker {
        gate: catchup::Gate::new(start_ms, saved_ms),
        w: w.clone(),
        services: HashMap::new(),
        apps: crate::timing::time("watch apps open", || apps::Apps::open(dir.join("seen.tsv"))),
        sigs: sig::Cache::default(),
        ignore: cfg.expanded_ignore(),
        last_record: 0,
        sysroot: std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()),
    };
    let _ = std::thread::Builder::new().name("hayate-watch".into()).stack_size(512 * 1024).spawn(move || worker.run(rx));

    let (w2, tx2) = (w.clone(), tx.clone());
    let _ = std::thread::Builder::new().name("hayate-watch-notify".into()).stack_size(128 * 1024).spawn(move || notify_thread(w2, tx2));

    let ptx = Mutex::new(tx);
    etw::subscribe(move |ev| {
        if let ProcEvent::Start(p) = ev {
            let _ = ptx.lock().unwrap().send(Msg::Proc(p.clone()));
        }
    });
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let mut c = Config::default();
        assert!(c.alerts[&AlertType::ServiceInstalled]);
        assert!(!c.alerts[&AlertType::ServiceStartStop]);
        c.alerts.insert(AlertType::Camera, false);
        c.ignore = vec!["*\\target\\*".into(), "C:\\Tools\\*".into()];
        c.dnd_until = 123;
        c.first_run = 99;
        let c2 = Config::parse(&c.to_text());
        assert_eq!(c2, c);
        // missing [ignore] keeps defaults
        let c3 = Config::parse("[alerts]\ncamera = log\n");
        assert_eq!(c3.ignore.len(), apps::DEFAULT_IGNORE.len());
        assert!(!c3.alerts[&AlertType::Camera]);
    }

    #[test]
    fn filter() {
        let e = Event { kind: Some(Kind::NewApp), name: "Zen.exe".into(), sig: SigState::Unsigned, unread: true, ..Default::default() };
        assert!(Filter { text: "zen".into(), ..Default::default() }.hit(&e));
        assert!(!Filter { kinds: vec![Kind::Camera], ..Default::default() }.hit(&e));
        assert!(Filter { unsigned_only: true, unread_only: true, ..Default::default() }.hit(&e));
    }

    #[test]
    fn dedup_images() {
        let v = vec![r"C:\Windows\System32\svchost.exe".to_string(), r"c:\windows\system32\SVCHOST.EXE".into(), r"C:\A\b.exe".into(), r"C:\Windows\System32\svchost.exe".into()];
        assert_eq!(dedup_by_key(v), vec![r"C:\Windows\System32\svchost.exe".to_string(), r"C:\A\b.exe".into()]);
    }

    /// Seeding records identity + version, never a signature, and skips
    /// ignored, missing and already-seen paths.
    #[test]
    fn seed_without_signatures() {
        let d = std::env::temp_dir().join(format!("hayate-seed-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let f = d.join("seen.tsv");
        let _ = std::fs::remove_file(&f);
        let mut a = apps::Apps::open(f.clone());
        let root = std::env::var("SystemRoot").unwrap_or("C:\\Windows".into());
        let cmd = format!("{}\\System32\\cmd.exe", root);
        let note = format!("{}\\notepad.exe", root);
        let ign = vec![format!("{}\\notepad*", root)];
        let n = seed(&mut a, vec![cmd.clone(), note.clone(), format!("{}\\no-such.exe", root)], &ign);
        assert_eq!(n, 1);
        let e = a.get(&cmd).unwrap().clone();
        assert_eq!(e.sig.state, SigState::Unknown);
        assert!(!e.version.is_empty());
        assert_eq!(a.compare(&cmd, sig::stat(&cmd).unwrap()), None);
        assert!(a.get(&note).is_none());
        assert_eq!(seed(&mut a, vec![cmd.clone()], &ign), 0);
        assert_eq!(apps::Apps::open(f).get(&cmd), Some(&e));
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    fn cpu_ms() -> f64 {
        let (mut a, mut b, mut k, mut u) = Default::default();
        unsafe {
            let _ = GetThreadTimes(GetCurrentThread(), &mut a, &mut b, &mut k, &mut u);
        }
        let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) as f64 / 10_000.0;
        f(k) + f(u)
    }

    /// `cargo test bench_seed -- --ignored --nocapture`: per-phase CPU of a first-run seed.
    #[test]
    #[ignore]
    fn bench_seed() {
        let t = cpu_ms();
        let paths = running_images();
        let t_enum = cpu_ms() - t;
        let t = cpu_ms();
        let st: Vec<_> = paths.iter().map(|p| sig::stat(p)).collect();
        let t_stat = cpu_ms() - t;
        let t = cpu_ms();
        for p in &paths {
            let _ = sig::file_version(p);
        }
        let t_ver = cpu_ms() - t;
        let t = cpu_ms();
        let mut n = HashMap::new();
        for p in &paths {
            *n.entry(sig::check(p).state.key()).or_insert(0) += 1;
        }
        let t_sig = cpu_ms() - t;
        println!("{} paths ({} stat ok): enum {:.0} ms, stat {:.0} ms, version {:.0} ms, sig {:.0} ms {:?}", paths.len(), st.iter().flatten().count(), t_enum, t_stat, t_ver, t_sig, n);
        let f = std::env::temp_dir().join(format!("hayate-bench-seen-{}.tsv", std::process::id()));
        let _ = std::fs::remove_file(&f);
        let mut a = apps::Apps::open(f.clone());
        let t = cpu_ms();
        let n = seed(&mut a, running_images(), &[]);
        println!("seed (enum + stat + version + write): {} images, {:.0} ms CPU", n, cpu_ms() - t);
        let _ = std::fs::remove_file(&f);
    }
}
