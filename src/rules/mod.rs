//! Rules: per-process "always" settings (Process Lasso replacement).
//!
//! Rules live in `%LOCALAPPDATA%\Hayate\rules.ini` (plain text, see model.rs).
//! They apply in the ETW ProcessStart callback, once per process start, and
//! to already-running processes when Hayate starts or a rule is added/edited.
//! Never retried: a refusal is recorded in the rule's last result.
#![allow(dead_code)]

pub mod actions;
pub mod apply;
pub mod cpu;
pub mod disallow;
pub mod glob;
pub mod model;
pub mod protect;
pub mod view;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use apply::Outcome;
#[allow(unused_imports)]
pub use model::{Actions, Awake, IoPriority, LastResult, MatchKind, Priority, Rule, Settings};

use crate::etw::{self, ProcEvent, ProcStart};
use crate::osutil;

/// A CPU preset offered by the UI (only when `ccd_presets_enabled`).
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub label: String,
    pub mask: u64,
}

struct Inner {
    persistence: Option<SaveStatus>,
    rules: Vec<Rule>,
    settings: Settings,
    next_id: u32,
    /// lowercase exe name → rule indexes (name rules only)
    by_name: HashMap<String, Vec<usize>>,
    stats_dirty: bool,
    last_save: u64,
    warnings: Vec<String>,
}

pub struct Engine {
    inner: Mutex<Inner>,
    file: PathBuf,
    /// Disallow work (process starts, sweeps) runs on its own worker, never on the ETW or UI thread
    jobs: std::sync::OnceLock<std::sync::mpsc::SyncSender<disallow::Job>>,
    /// per-rule attempt bounds, recovery deadlines and the queue-overflow mark (see `disallow::Guard`)
    guards: disallow::Guards,
    /// why the Disallow worker is not running (spawn failed, thread stopped): nothing is ended while set
    worker_err: Mutex<Option<String>>,
    writes: Arc<RuleWrites>,
}

/// Serialize only disk work, never the UI. A timed-out older job cannot overwrite a
/// newer acknowledged save, even when bgio starts a replacement worker.
#[derive(Default)]
struct RuleWrites {
    epoch: std::sync::atomic::AtomicU64,
    gate: Mutex<()>,
}

impl RuleWrites {
    fn next(&self) -> u64 {
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    fn write(&self, epoch: u64, file: &std::path::Path, data: &[u8]) -> Result<(), String> {
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.epoch.load(std::sync::atomic::Ordering::SeqCst) != epoch {
            return Err("Superseded by a newer rules save".into());
        }
        osutil::write_atomic(file, data).map_err(|e| e.to_string())
    }
}

pub type SaveStatus = Arc<Mutex<Option<Result<(), String>>>>;

impl Inner {
    /// A visible note in the Rules tab (kept short: the newest 20, no repeats).
    fn warn(&mut self, w: String) {
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
        let n = self.warnings.len();
        if n > 20 {
            self.warnings.drain(..n - 20);
        }
    }

    fn reindex(&mut self) {
        self.by_name.clear();
        for (i, r) in self.rules.iter().enumerate() {
            if r.kind() == MatchKind::Name {
                self.by_name.entry(r.pattern.to_lowercase()).or_default().push(i);
            }
        }
    }

    /// Most specific enabled rule: exact path > exe name > glob.
    fn find(&self, name: &str, path: &str) -> Option<usize> {
        let en = |i: &usize| self.rules[*i].enabled;
        if let Some(i) = self.rules.iter().position(|r| r.enabled && r.kind() == MatchKind::Path && r.matches(name, path)) {
            return Some(i);
        }
        if let Some(v) = self.by_name.get(&name.to_lowercase()) {
            if let Some(i) = v.iter().copied().find(en) {
                return Some(i);
            }
        }
        self.rules.iter().position(|r| r.enabled && r.kind() == MatchKind::Glob && r.matches(name, path))
    }
}

impl Engine {
    fn load(file: PathBuf) -> Engine {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let (mut rules, settings, mut warnings) = model::parse(&text);
        for (i, r) in rules.iter_mut().enumerate() {
            r.id = i as u32 + 1;
            // a hand-edited or old file may hold a Disallow flag no safeguard would ever honour:
            // say so, never show it as ON
            if let Some(w) = sanitize(r) {
                warnings.push(w);
            }
        }
        let next_id = rules.len() as u32 + 1;
        if warnings.len() > 20 {
            warnings.drain(..warnings.len() - 20);
        }
        let mut inner = Inner { persistence: None, rules, settings, next_id, by_name: HashMap::new(), stats_dirty: false, last_save: osutil::now_unix(), warnings };
        inner.reindex();
        Engine { inner: Mutex::new(inner), file, jobs: std::sync::OnceLock::new(), guards: Default::default(), worker_err: Mutex::new(None), writes: Default::default() }
    }

    /// Start the Disallow worker: one thread, a bounded queue. Call once, before ETW is hooked.
    /// A thread that cannot start is surfaced through `worker_problem` (no rule may then claim
    /// to be ON), never ignored.
    fn start_worker(self: &Arc<Engine>) {
        self.start_worker_with(&|body| std::thread::Builder::new().name("hayate-disallow".into()).stack_size(512 * 1024).spawn(body).map(|_| ()));
    }

    fn start_worker_with(self: &Arc<Engine>, spawn: &dyn Fn(Box<dyn FnOnce() + Send>) -> std::io::Result<()>) {
        let (tx, rx) = std::sync::mpsc::sync_channel::<disallow::Job>(disallow::QUEUE);
        let e = self.clone();
        match spawn(Box::new(move || {
            let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.worker_loop(rx)));
            e.fail_worker(if stopped.is_err() { "the Disallow worker panicked; nothing will be ended" } else { "the Disallow worker stopped; nothing will be ended" }.into());
        })) {
            Ok(()) => {
                let _ = self.jobs.set(tx);
            }
            Err(err) => self.fail_worker(format!("the Disallow worker thread could not start ({err}); nothing will be ended")),
        }
    }

    /// The worker: wait for a job until the next recovery deadline (or the idle limit), run it,
    /// then run every recovery that is due. The deadline check follows EVERY job, so a
    /// continuous stream of jobs cannot starve a recovery.
    fn worker_loop(&self, rx: std::sync::mpsc::Receiver<disallow::Job>) {
        use std::sync::mpsc::RecvTimeoutError;
        loop {
            let now = disallow::now_ms();
            let wait = self.next_deadline().map_or(disallow::IDLE_WAIT_MS, |d| d.saturating_sub(now).min(disallow::IDLE_WAIT_MS)).max(1);
            match rx.recv_timeout(std::time::Duration::from_millis(wait)) {
                Ok(job) => self.guarded(|| self.run_job(job)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.guarded(|| self.run_due(disallow::now_ms()));
        }
    }

    /// A panic in one job must not end enforcement for good.
    fn guarded(&self, f: impl FnOnce()) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
            osutil::log("disallow: a worker job panicked (continuing)");
        }
    }

    fn save_locked(&self, g: &mut Inner) {
        // guards live only as long as a blocking rule does (disabled / removed / edited: gone)
        self.prune_guards(g);
        // written by the I/O worker: the lock (read by the UI and ETW threads) is never held across disk I/O
        let status: SaveStatus = Arc::new(Mutex::new(None));
        g.persistence = Some(status.clone());
        let file = self.file.clone();
        let data = model::write_all(&g.rules, &g.settings).into_bytes();
        let writes = self.writes.clone();
        let epoch = writes.next();
        let write = move || {
            *status.lock().unwrap() = Some(writes.write(epoch, &file, &data));
        };
        if cfg!(test) { write(); } else { crate::bgio::submit_fast("save rules", write); }
        g.stats_dirty = false;
        g.last_save = osutil::now_unix();
    }

    // ---- queries -------------------------------------------------------------

    pub fn persistence(&self) -> Option<SaveStatus> {
        self.inner.lock().unwrap().persistence.clone()
    }

    pub fn cpu_rule_note(&self, name: &str) -> &'static str {
        let g = self.inner.lock().unwrap();
        if g.rules.iter().any(|r| r.pattern.eq_ignore_ascii_case(name) && !r.enabled) {
            "Rule disabled; enable it in Rules to apply."
        } else {
            "Enabled rules apply by priority; check results in Rules."
        }
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.inner.lock().unwrap().rules.clone()
    }

    pub fn get(&self, id: u32) -> Option<Rule> {
        self.inner.lock().unwrap().rules.iter().find(|r| r.id == id).cloned()
    }

    /// Rule that would apply to this exe (for the Processes view badge).
    pub fn rule_for(&self, path: &str) -> Option<Rule> {
        let g = self.inner.lock().unwrap();
        g.find(osutil::file_name(path), path).map(|i| g.rules[i].clone())
    }

    /// Copy of the rule list for read-only matching in the UI (no lock held).
    pub fn view(&self) -> view::RuleView {
        view::RuleView::new(self.rules())
    }

    /// Parse warnings from the last load of rules.ini.
    pub fn warnings(&self) -> Vec<String> {
        self.inner.lock().unwrap().warnings.clone()
    }

    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }

    pub fn set_settings(&self, s: Settings) {
        let mut g = self.inner.lock().unwrap();
        g.settings = s;
        self.save_locked(&mut g);
    }

    /// CCD0 · V-Cache / CCD1 · Frequency presets from the L3 layout. Empty
    /// unless `ccd_presets_enabled` (user opt-in; default off).
    pub fn cpu_presets(&self) -> Vec<Preset> {
        if !self.settings().ccd_presets_enabled || cpu::topology().supported().is_err() {
            return Vec::new();
        }
        ccd_presets(&apply::l3_groups())
    }

    /// Set (`Some`) or drop (`None`) the saved CPU affinity / CPU sets of the
    /// per-exe-name rule `name`, creating the rule when needed and removing it
    /// when nothing is left. The mask passes the topology gate first. The caller
    /// applies running matches in the background. Returns the id (0 = removed).
    pub fn set_cpu_field(&self, name: &str, kind: cpu::Kind, value: Option<u64>) -> Result<u32, String> {
        let name = name.trim();
        if name.is_empty() || model::kind_of(name) != MatchKind::Name {
            return Err("A saved rule needs a plain exe name.".into());
        }
        if let Some(m) = value {
            cpu::topology().validate(m)?;
        }
        let mut g = self.inner.lock().unwrap();
        let existing = g.rules.iter().find(|r| r.pattern.eq_ignore_ascii_case(name)).cloned();
        let mut r = existing.unwrap_or_else(|| Rule::new(name, Actions::default()));
        match kind {
            cpu::Kind::Affinity => r.actions.affinity = value,
            cpu::Kind::CpuSets => r.actions.cpu_sets = value,
        }
        if r.actions.is_empty() {
            if r.id != 0 {
                g.rules.retain(|old| old.id != r.id);
            }
            r.id = 0;
        } else if r.id == 0 {
            r.id = g.next_id;
            g.next_id += 1;
            g.rules.push(r.clone());
        } else {
            *g.rules.iter_mut().find(|old| old.id == r.id).unwrap() = r.clone();
        }
        g.reindex();
        self.save_locked(&mut g);
        Ok(r.id)
    }

    /// The saved mask of `kind` for the rule on exactly this exe name.
    pub fn saved_cpu(&self, name: &str, kind: cpu::Kind) -> Option<u64> {
        let g = self.inner.lock().unwrap();
        let r = g.rules.iter().find(|r| r.pattern.eq_ignore_ascii_case(name.trim()))?;
        match kind {
            cpu::Kind::Affinity => r.actions.affinity,
            cpu::Kind::CpuSets => r.actions.cpu_sets,
        }
    }

    // ---- edits ---------------------------------------------------------------

    /// Add a rule (id assigned) and apply it to matching running processes.
    pub fn add(&self, mut r: Rule) -> u32 {
        r.pattern = r.pattern.trim().to_string();
        let warn = sanitize(&mut r);
        let blocks = r.blocks();
        let id = {
            let mut g = self.inner.lock().unwrap();
            if let Some(w) = warn {
                g.warn(w);
            }
            r.id = g.next_id;
            g.next_id += 1;
            g.rules.push(r);
            g.reindex();
            self.save_locked(&mut g);
            g.next_id - 1
        };
        self.after_edit(id, blocks);
        id
    }

    /// A rule changed: Disallow rules are swept on the worker (never here: this runs on the
    /// UI thread), all others apply as before.
    fn after_edit(&self, id: u32, blocks: bool) {
        if blocks {
            self.submit(disallow::Job::Sweep(Some(id)));
        } else {
            self.apply_running(Some(id));
        }
    }

    /// Replace pattern/enabled/actions of an existing rule (stats kept).
    pub fn update(&self, mut r: Rule) -> bool {
        r.pattern = r.pattern.trim().to_string();
        let warn = sanitize(&mut r);
        let blocks = r.blocks();
        let ok = {
            let mut g = self.inner.lock().unwrap();
            if let Some(w) = warn {
                g.warn(w);
            }
            match g.rules.iter_mut().find(|x| x.id == r.id) {
                Some(x) => {
                    // an edited rule starts over: pause, deadline and settled answers do not carry across
                    self.forget(r.id);
                    x.pattern = r.pattern.clone();
                    x.enabled = r.enabled;
                    x.actions = r.actions.clone();
                    g.reindex();
                    self.save_locked(&mut g);
                    true
                }
                None => false,
            }
        };
        if ok {
            self.after_edit(r.id, blocks);
        }
        ok
    }

    pub fn remove(&self, id: u32) -> bool {
        let mut g = self.inner.lock().unwrap();
        let n = g.rules.len();
        g.rules.retain(|r| r.id != id);
        let removed = g.rules.len() != n;
        if removed {
            g.reindex();
            self.save_locked(&mut g);
        }
        removed
    }

    /// Move a rule up/down in the list (order breaks ties within a kind).
    pub fn reorder(&self, id: u32, to: usize) -> bool {
        let mut g = self.inner.lock().unwrap();
        let Some(i) = g.rules.iter().position(|r| r.id == id) else { return false };
        let r = g.rules.remove(i);
        let to = to.min(g.rules.len());
        g.rules.insert(to, r);
        g.reindex();
        self.save_locked(&mut g);
        true
    }

    /// Rule as clipboard text (no stats).
    pub fn copy(&self, id: u32) -> Option<String> {
        let r = self.get(id)?;
        let mut s = String::new();
        model::write_rule(&mut s, &r, false);
        Some(s)
    }

    /// Paste clipboard text. With `onto = Some(pattern)` the first pasted
    /// rule's actions go onto that pattern (existing rule replaced, else new):
    /// "copy rules from A, paste onto B". Returns affected rule ids.
    pub fn paste(&self, text: &str, onto: Option<&str>) -> Result<Vec<u32>, String> {
        let (rules, _, _) = model::parse(text);
        if rules.is_empty() {
            return Err("clipboard has no Hayate rule".into());
        }
        let mut ids = Vec::new();
        match onto {
            Some(p) => {
                let existing = self.inner.lock().unwrap().rules.iter().find(|r| r.pattern.eq_ignore_ascii_case(p.trim())).map(|r| r.id);
                let actions = rules[0].actions.clone();
                // Disallow is never carried by "paste onto": it neither turns a block on for B
                // nor drops one B already has
                let mut actions = actions;
                match existing {
                    Some(id) => {
                        let mut r = self.get(id).unwrap();
                        actions.disallow = r.actions.disallow;
                        r.actions = actions;
                        self.update(r);
                        ids.push(id);
                    }
                    None => {
                        actions.disallow = false;
                        ids.push(self.add(Rule::new(p, actions)));
                    }
                }
            }
            None => {
                for mut r in rules {
                    r.hits = 0;
                    r.last = None;
                    // clipboard text is not consent to end programs: an imported Disallow rule arrives OFF
                    if r.actions.disallow {
                        r.enabled = false;
                    }
                    ids.push(self.add(r));
                }
            }
        }
        Ok(ids)
    }

    // ---- applying --------------------------------------------------------------

    /// "This time only": apply to one process, nothing saved. `create` is the
    /// creation time the list showed; a PID reused since is refused.
    pub fn apply_once(&self, pid: u32, create: i64, a: &Actions) -> Outcome {
        let name = osutil::open_same(pid, create, windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS(0))
            .and_then(|h| osutil::image_of_handle(h.0))
            .map(|p| osutil::file_name(&p).to_string())
            .unwrap_or_else(|| format!("pid {}", pid));
        apply::apply(pid, create, a, &format!("Hayate: {} running", name))
    }

    fn on_start(&self, p: &ProcStart) {
        // Events may be delayed past PID reuse. Match only the live image read
        // with this creation identity, never the event's old path or basename.
        let Some((create, Some(path))) = osutil::identity(p.pid) else { return };
        let name = osutil::file_name(&path);
        let (rule_id, actions, pattern) = {
            let g = self.inner.lock().unwrap();
            match g.find(name, &path) {
                // block wins: no other action is applied to a process that is about to be ended
                Some(i) if g.rules[i].blocks() => {
                    drop(g);
                    self.submit(disallow::Job::Start { pid: p.pid, create });
                    return;
                }
                Some(i) => (g.rules[i].id, g.rules[i].actions.clone(), name.to_string()),
                None => return,
            }
        };
        let out = apply::apply(p.pid, create, &actions, &format!("Hayate: {} running", pattern));
        self.record(rule_id, p.pid, &out);
    }

    fn record(&self, id: u32, pid: u32, out: &Outcome) {
        if out.gone {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        if let Some(r) = g.rules.iter_mut().find(|r| r.id == id) {
            r.hits += 1;
            r.last = Some(LastResult { time: osutil::now_unix(), pid, ok: out.ok, protected: out.protected, text: out.text.clone() });
        }
        g.stats_dirty = true;
        // stats are cheap to lose: save at most once a minute
        if osutil::now_unix().saturating_sub(g.last_save) >= 60 {
            self.save_locked(&mut g);
        }
    }

    /// Apply rules to processes already running (all rules, or just `only`).
    pub fn apply_running(&self, only: Option<u32>) {
        for (pid, _) in running() {
            // creation time and image path from the same handle: the pair we match the
            // rule on is the pair every later handle is verified against
            let (create, path) = match osutil::identity(pid) {
                Some((c, Some(live))) => (c, live),
                _ => continue,
            };
            let name = osutil::file_name(&path).to_string();
            let hit = {
                let g = self.inner.lock().unwrap();
                // a Disallow rule is the worker's business (`disallow::Job::Sweep`), and it replaces every other action
                g.find(&name, &path).filter(|&i| !g.rules[i].blocks()).map(|i| (g.rules[i].id, g.rules[i].actions.clone()))
            };
            if let Some((id, a)) = hit {
                if only.map_or(true, |o| o == id) {
                    let out = apply::apply(pid, create, &a, &format!("Hayate: {} running", name));
                    self.record(id, pid, &out);
                }
            }
        }
    }

    /// Hayate exit: restore power plan, drop sleep requests, flush stats.
    pub fn shutdown(&self) {
        apply::release_all();
        let mut g = self.inner.lock().unwrap();
        if g.stats_dirty {
            self.save_locked(&mut g);
        }
    }
}

/// (pid, Win32 path) of running processes; bare exe name if the path is unreadable.
fn running() -> Vec<(u32, String)> {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let mut out = Vec::new();
    unsafe {
        let Ok(h) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let h = osutil::Handle(h);
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(h.0, &mut e).is_ok() {
            loop {
                let pid = e.th32ProcessID;
                if pid > 4 {
                    let name = osutil::from_wide(&e.szExeFile);
                    out.push((pid, osutil::image_path(pid).unwrap_or(name)));
                }
                if Process32NextW(h.0, &mut e).is_err() {
                    break;
                }
            }
        }
    }
    out
}

/// Pure: L3 groups (largest first) → (name, mask) per CCD, in cpu order.
/// Empty with fewer than two groups.
pub fn ccd_names(groups: &[(u64, u32)]) -> Vec<(String, u64)> {
    if groups.len() < 2 {
        return Vec::new();
    }
    let vcache = groups[0].1 > groups[1].1;
    let mut v: Vec<(u64, u32)> = groups.to_vec();
    // stable label order: by first cpu
    v.sort_by_key(|(m, _)| m.trailing_zeros());
    v.iter()
        .enumerate()
        .map(|(i, (m, l3))| {
            let kind = if vcache && *l3 == groups[0].1 { " · V-Cache" } else if vcache { " · Frequency" } else { "" };
            (format!("CCD{}{}", i, kind), *m)
        })
        .collect()
}

/// Pure: L3 groups (largest first) → labelled presets.
pub fn ccd_presets(groups: &[(u64, u32)]) -> Vec<Preset> {
    ccd_names(groups).into_iter().map(|(n, m)| Preset { label: format!("{n} ({})", model::format_cpus(m)), mask: m }).collect()
}

/// Load rules, recover a power plan left by a crash, hook ETW, apply to
/// running processes (background thread). Call after `etw::start()`.
pub fn start() -> Arc<Engine> {
    apply::recover_power_plan();
    let e = Arc::new(Engine::load(osutil::data_dir().join("rules.ini")));
    e.start_worker();
    let e2 = e.clone();
    etw::subscribe(move |ev| {
        if let ProcEvent::Start(p) = ev {
            e2.on_start(p);
        }
    });
    let e3 = e.clone();
    let _ = std::thread::Builder::new().name("hayate-rules-init".into()).stack_size(256 * 1024).spawn(move || {
        e3.apply_running(None);
        // processes already running when Hayate starts (rules ending them were saved earlier)
        e3.submit(disallow::Job::Sweep(None));
    });
    e
}

/// Disallow ends processes, so it only ever lives on an exact name or path that no safeguard
/// would always refuse. Anything else loses the flag (the rest of the rule stays; a rule left
/// with nothing is switched OFF) and the returned text says so.
fn sanitize(r: &mut Rule) -> Option<String> {
    if !r.actions.disallow {
        return None;
    }
    let why = if r.kind() == MatchKind::Glob {
        "a pattern with * or ? can never be disallowed".to_string()
    } else {
        protect::pattern_refusal(&r.pattern)?.text().trim_start_matches("refused: ").to_string()
    };
    r.actions.disallow = false;
    let left = if r.actions.is_empty() {
        r.enabled = false;
        "nothing else is set, so the rule is OFF"
    } else {
        "its other settings are unchanged"
    };
    Some(format!("rule '{}': Disallow ignored ({why}); {left}", r.pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_delayed_rules_write_cannot_replace_a_newer_acknowledged_save() {
        let e = engine_with(&[]);
        let old = e.writes.next();
        let new = e.writes.next();
        e.writes.write(new, &e.file, b"new confirmed rules").unwrap();
        assert!(e.writes.write(old, &e.file, b"old stalled rules").unwrap_err().contains("Superseded"));
        assert_eq!(std::fs::read(&e.file).unwrap(), b"new confirmed rules");
        let retry = e.writes.next();
        e.writes.write(retry, &e.file, b"retried rules").unwrap();
        assert_eq!(std::fs::read(&e.file).unwrap(), b"retried rules");
    }

    fn engine_with(rules: &[Rule]) -> Engine {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("hayate-rules-test-{}-{}", std::process::id(), n));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("rules.ini");
        std::fs::write(&f, model::write_all(rules, &Settings::default())).unwrap();
        Engine::load(f)
    }

    #[test]
    fn specificity() {
        let hi = Actions { priority: Some(Priority::High), ..Default::default() };
        let lo = Actions { priority: Some(Priority::Idle), ..Default::default() };
        let mid = Actions { priority: Some(Priority::AboveNormal), ..Default::default() };
        let e = engine_with(&[Rule::new("X:\\Games\\*", lo.clone()), Rule::new("game.exe", mid.clone()), Rule::new("X:\\Games\\a\\game.exe", hi.clone())]);
        assert_eq!(e.rule_for("X:\\Games\\a\\game.exe").unwrap().actions, hi);
        assert_eq!(e.rule_for("X:\\Games\\b\\game.exe").unwrap().actions, mid);
        assert_eq!(e.rule_for("X:\\Games\\b\\other.exe").unwrap().actions, lo);
        assert!(e.rule_for("C:\\x\\other.exe").is_none());
    }

    #[test]
    fn the_view_agrees_with_the_engine() {
        use view::Mark;
        let a = Actions { priority: Some(Priority::High), ..Default::default() };
        let mut off = Rule::new("game.exe", a.clone());
        off.enabled = false;
        let mut off2 = Rule::new("X:\\Games\\a\\game.exe", a.clone());
        off2.enabled = false;
        let e = engine_with(&[Rule::new("X:\\Games\\*", a.clone()), off, Rule::new("Game.EXE", a.clone()), off2]);
        let v = e.view();
        for p in ["X:\\Games\\a\\game.exe", "X:\\Games\\b\\game.exe", "C:\\game.exe", "X:\\Games\\b\\other.exe", "C:\\Y\\other.exe"] {
            let engine = e.rule_for(p).map(|r| r.id);
            let mark = v.mark(Some(p));
            let from_view = if let Mark::Rule(id) = mark { Some(id) } else { None };
            assert_eq!(engine, from_view, "{p}");
            // the first row of `matching` is the effective rule exactly when the engine has one
            let m = v.matching(p);
            let eff = m.iter().find(|m| m.standing == view::Standing::Effective).map(|m| m.rule.id);
            assert_eq!(engine, eff, "{p}");
        }
        // only-disabled shows as Off, not as "no rule"
        assert!(matches!(v.mark(Some("C:\\Z\\game.exe")), Mark::Rule(_)));
    }

    #[test]
    fn disabled_rule_skipped() {
        let mut r = Rule::new("game.exe", Actions { priority: Some(Priority::High), ..Default::default() });
        r.enabled = false;
        let e = engine_with(&[r]);
        assert!(e.rule_for("C:\\game.exe").is_none());
        assert!(e.cpu_rule_note("game.exe").contains("disabled"));
    }

    #[test]
    fn delayed_start_never_matches_stale_path_with_same_basename() {
        let exe = std::env::current_exe().unwrap();
        let stale = format!("C:\\hayate-stale-event\\{}", exe.file_name().unwrap().to_string_lossy());
        // Empty actions avoid changing the test process even on the old code.
        let e = engine_with(&[Rule::new(&stale, Actions::default())]);
        e.on_start(&ProcStart { pid: std::process::id(), ppid: 0, session: 0, path: stale, time: 0 });
        assert_eq!(e.rules()[0].hits, 0);
    }

    #[test]
    fn persistence_reports_failure_and_retry() {
        let e = engine_with(&[]);
        // An existing directory at the temporary-file path makes the write fail.
        let tmp = e.file.with_extension("tmp");
        std::fs::create_dir(&tmp).unwrap();
        e.set_settings(Settings::default());
        assert!(matches!(&*e.persistence().unwrap().lock().unwrap(), Some(Err(_))));
        std::fs::remove_dir(&tmp).unwrap();
        e.set_settings(Settings::default());
        assert!(matches!(&*e.persistence().unwrap().lock().unwrap(), Some(Ok(()))));
        assert!(std::fs::read_to_string(&e.file).unwrap().contains("ccd_presets_enabled"));
    }

    #[test]
    fn copy_paste_onto() {
        let e = engine_with(&[Rule::new("a.exe", Actions { io: Some(IoPriority::Low), ..Default::default() })]);
        let t = e.copy(1).unwrap();
        // paste onto a pattern that has no rule yet → new rule (apply_running finds nothing named zz_*.exe)
        let ids = e.paste(&t, Some("zz_hayate_test.exe")).unwrap();
        assert_eq!(ids, vec![2]);
        assert_eq!(e.get(2).unwrap().actions.io, Some(IoPriority::Low));
        assert!(e.paste("hello", None).is_err());
        assert!(e.remove(2));
        assert_eq!(e.rules().len(), 1);
    }

    #[test]
    fn saved_cpu_rule_validates_and_cleans_up() {
        let e = engine_with(&[]);
        let t = cpu::topology();
        if t.supported().is_err() {
            return;
        }
        let name = "zz_hayate_cpu_test.exe";
        let one = 1u64 << t.valid.trailing_zeros();
        // invalid selections never reach rules.ini
        assert!(e.set_cpu_field(name, cpu::Kind::Affinity, Some(0)).is_err());
        assert!(e.set_cpu_field(name, cpu::Kind::CpuSets, Some(!t.valid)).is_err() || t.valid == u64::MAX);
        assert!(e.set_cpu_field(r"C:\x\a.exe", cpu::Kind::Affinity, Some(one)).is_err());
        assert!(e.set_cpu_field("*.exe", cpu::Kind::Affinity, Some(one)).is_err());
        assert!(e.rules().is_empty());
        // affinity and CPU sets live side by side in one rule
        let id = e.set_cpu_field(name, cpu::Kind::Affinity, Some(one)).unwrap();
        assert_eq!(e.set_cpu_field(name.to_uppercase().as_str(), cpu::Kind::CpuSets, Some(t.valid)).unwrap(), id);
        assert_eq!(e.rules().len(), 1);
        assert_eq!(e.saved_cpu(name, cpu::Kind::Affinity), Some(one));
        assert_eq!(e.saved_cpu(name, cpu::Kind::CpuSets), Some(t.valid));
        // other actions survive removing one CPU field; the rule goes when it is empty
        let mut r = e.get(id).unwrap();
        r.actions.priority = Some(Priority::BelowNormal);
        e.update(r);
        e.set_cpu_field(name, cpu::Kind::Affinity, None).unwrap();
        assert_eq!(e.get(id).unwrap().actions.affinity, None);
        assert_eq!(e.get(id).unwrap().actions.priority, Some(Priority::BelowNormal));
        let mut r = e.get(id).unwrap();
        r.actions.priority = None;
        e.update(r);
        assert_eq!(e.set_cpu_field(name, cpu::Kind::CpuSets, None).unwrap(), 0);
        assert!(e.rules().is_empty());
        // and the file on disk round-trips with the unchanged format
        let text = model::write_all(&[Rule::new(name, Actions { affinity: Some(one), cpu_sets: Some(t.valid), ..Default::default() })], &Settings::default());
        assert!(text.contains("affinity = ") && text.contains("cpusets = "), "{text}");
    }

    // ---- Disallow running ----------------------------------------------------

    fn dis() -> Actions {
        Actions { disallow: true, ..Default::default() }
    }

    /// A disposable program: a copy of ping.exe under a unique name in a unique temp folder,
    /// so a rule on it can never reach anything else. Children are killed and the folder
    /// removed when it drops.
    struct Fixture {
        dir: PathBuf,
        kids: Vec<std::process::Child>,
    }
    impl Fixture {
        fn new() -> Fixture {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("hayate-disallow-fixture-{}-{}", std::process::id(), n));
            std::fs::create_dir_all(&dir).unwrap();
            Fixture { dir, kids: Vec::new() }
        }
        /// copy the sleeper to `<dir>\<sub>\<name>` and return its path
        fn exe(&self, sub: Option<&str>, name: &str) -> String {
            let sys = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
            let d = match sub {
                Some(s) => self.dir.join(s),
                None => self.dir.clone(),
            };
            std::fs::create_dir_all(&d).unwrap();
            let to = d.join(name);
            std::fs::copy(format!("{sys}\\System32\\PING.EXE"), &to).unwrap();
            to.to_string_lossy().into_owned()
        }
        fn spawn(&mut self, exe: &str) -> (u32, i64) {
            let c = crate::osutil::quiet_cmd(exe).args(["-n", "120", "127.0.0.1"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
            let pid = c.id();
            self.kids.push(c);
            let (create, path) = osutil::identity(pid).expect("fixture identity");
            assert!(path.is_some_and(|p| p.eq_ignore_ascii_case(exe)), "live image is the fixture");
            (pid, create)
        }
        fn alive(&mut self, pid: u32) -> bool {
            self.kids.iter_mut().find(|c| c.id() == pid).is_some_and(|c| matches!(c.try_wait(), Ok(None)))
        }
        /// exit code, waiting a little for the end to land
        fn exit_code(&mut self, pid: u32) -> Option<i32> {
            let c = self.kids.iter_mut().find(|c| c.id() == pid)?;
            for _ in 0..60 {
                if let Ok(Some(st)) = c.try_wait() {
                    return st.code();
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            None
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for c in &mut self.kids {
                let _ = c.kill();
                let _ = c.wait();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const ENABLE: disallow::DisallowMode = disallow::DisallowMode::Enable { confirm_whole_rule: true };
    const KEEP_OFF: disallow::DisallowMode = disallow::DisallowMode::KeepOff;

    /// Put the Disallow flag on an existing rule WITHOUT the load/edit checks (simulates a flag that
    /// got past them, so the run-time layers are tested on their own).
    fn force_flag(e: &Engine, pattern: &str) {
        let mut g = e.inner.lock().unwrap();
        for r in g.rules.iter_mut().filter(|r| r.pattern.eq_ignore_ascii_case(pattern)) {
            r.actions.disallow = true;
            r.enabled = true;
        }
        g.reindex();
    }

    fn arc(rules: &[Rule]) -> Arc<Engine> {
        Arc::new(engine_with(rules))
    }

    fn std_spawn(body: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
        std::thread::Builder::new().name("test-disallow".into()).spawn(body).map(|_| ())
    }

    /// Put rule `id`'s guard into "paused, survivors due `due_in_ms` from now" the way a storm would
    /// have left it.
    fn pause_ending_in(e: &Engine, id: u32, due_in_ms: u64) {
        let now = disallow::now_ms();
        let t = (now + due_in_ms).saturating_sub(disallow::RECOVER_SLACK_MS + disallow::STORM_PAUSE_MS);
        let mut r = e.guards.lock().unwrap();
        let g = r.rules.entry(id).or_default();
        for _ in 0..disallow::STORM_MAX {
            assert_eq!(g.gate(t), disallow::Gate::Go);
        }
        assert_eq!(g.gate(t), disallow::Gate::PauseStarts);
        assert_eq!(g.recover_at(), Some(t + disallow::STORM_PAUSE_MS + disallow::RECOVER_SLACK_MS));
    }

    fn wait_dead(f: &mut Fixture, pids: &[u32], ms: u64) -> bool {
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_millis(ms) {
            if pids.iter().all(|p| !f.alive(*p)) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        false
    }

    fn blocking(e: &Engine, path: &str) -> Option<u32> {
        e.inner.lock().unwrap().blocking(osutil::file_name(path), path)
    }

    #[test]
    fn block_follows_the_engines_precedence_and_the_view_agrees() {
        let keep = Actions { priority: Some(Priority::High), ..Default::default() };
        let mut off = Rule::new("c.exe", dis());
        off.enabled = false;
        let e = engine_with(&[
            Rule::new("a.exe", dis()),                     // name: block every a.exe...
            Rule::new("X:\\Allowed\\a.exe", keep.clone()), // ...except this exact path (more specific, no Disallow)
            Rule::new("X:\\Games\\b.exe", dis()),          // exact path: block only there
            Rule::new("b.exe", keep.clone()),              // the name rule of b.exe does not block
            off,                                           // disabled: never blocks
            Rule::new("d.exe", dis()),
        ]);
        assert_eq!(blocking(&e, "C:\\x\\a.exe"), Some(1));
        assert_eq!(blocking(&e, "X:\\Allowed\\a.exe"), None, "a more specific enabled rule without Disallow lets it run");
        assert_eq!(blocking(&e, "X:\\Games\\b.exe"), Some(3));
        assert_eq!(blocking(&e, "X:\\Other\\b.exe"), None);
        assert_eq!(blocking(&e, "C:\\x\\c.exe"), None, "disabled rule");
        assert_eq!(blocking(&e, "C:\\x\\d.exe"), Some(6));
        assert_eq!(blocking(&e, "C:\\x\\unrelated.exe"), None);
        // the read-only view (Processes badge, rule peek) names the same effective rule
        let v = e.view();
        for p in ["C:\\x\\a.exe", "X:\\Allowed\\a.exe", "X:\\Games\\b.exe", "X:\\Other\\b.exe", "C:\\x\\c.exe", "C:\\x\\d.exe"] {
            let eff = e.rule_for(p);
            assert_eq!(eff.as_ref().is_some_and(|r| r.blocks()), blocking(&e, p).is_some(), "{p}");
            let from_view = v.matching(p).into_iter().find(|m| m.standing == view::Standing::Effective).map(|m| m.rule);
            assert_eq!(eff.map(|r| r.id), from_view.map(|r| r.id), "{p}");
        }
        assert!(e.has_blocking());
    }

    #[test]
    fn turning_the_only_block_off_or_removing_it_stops_enforcement_at_once() {
        let e = engine_with(&[Rule::new("zz_off.exe", dis())]);
        assert!(e.has_blocking());
        assert!(e.set_enabled(1, false));
        assert!(!e.has_blocking() && blocking(&e, "C:\\x\\zz_off.exe").is_none());
        assert!(e.set_enabled(1, true));
        assert!(e.has_blocking());
        assert!(e.remove(1));
        assert!(!e.has_blocking());
    }

    #[test]
    fn exact_path_scope_ends_that_file_only_through_a_verified_handle() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_a.exe", std::process::id());
        let target = f.exe(Some("one"), &name);
        let same_name_elsewhere = f.exe(Some("two"), &name);
        let other = f.exe(None, &format!("zz_hayate_disallow_{}_b.exe", std::process::id()));
        let e = engine_with(&[Rule::new(&target, dis())]);
        let (pa, ca) = f.spawn(&target);
        let (pe, ce) = f.spawn(&same_name_elsewhere);
        let (pb, _) = f.spawn(&other);
        // a PID whose creation time is not the one we identified: nothing happens
        assert!(e.enforce(pa, ca + 1).is_none());
        assert!(e.enforce(pa, 0).is_none(), "no start time, no action");
        assert!(f.alive(pa));
        // a process the rule does not match (same name, other folder): nothing happens
        assert!(e.enforce(pe, ce).is_none());
        assert!(f.alive(pe) && f.alive(pb));
        // the right identity
        let (id, out) = e.enforce(pa, ca).expect("matched");
        assert_eq!(id, 1);
        assert!(out.ok, "{out:?}");
        assert!(out.text.contains("ended") && out.text.contains(&name), "{}", out.text);
        assert_eq!(f.exit_code(pa), Some(1));
        assert!(f.alive(pe) && f.alive(pb), "nothing else was touched");
        // a second attempt on the dead process is silent
        assert!(e.enforce(pa, ca).is_none());
    }

    #[test]
    fn exe_name_scope_ends_every_copy_and_the_sweep_finds_them_without_touching_others() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_n.exe", std::process::id());
        let c1 = f.exe(Some("one"), &name);
        let c2 = f.exe(Some("two"), &name);
        let other = f.exe(None, &format!("zz_hayate_disallow_{}_o.exe", std::process::id()));
        let (p1, _) = f.spawn(&c1);
        let (p2, _) = f.spawn(&c2);
        let (po, _) = f.spawn(&other);
        let e = engine_with(&[Rule::new(&name, dis())]);
        e.run_job(disallow::Job::Sweep(None));
        assert_eq!(f.exit_code(p1), Some(1));
        assert_eq!(f.exit_code(p2), Some(1));
        assert!(f.alive(po), "an unrelated program is never ended");
        let r = e.rules().remove(0);
        assert_eq!(r.hits, 2);
        assert!(r.last.as_ref().is_some_and(|l| l.ok && l.text.contains("ended")), "{:?}", r.last);
    }

    #[test]
    fn a_disabled_or_overridden_rule_ends_nothing_even_on_a_sweep() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_d.exe", std::process::id());
        let exe = f.exe(None, &name);
        let (pid, create) = f.spawn(&exe);
        let mut off = Rule::new(&name, dis());
        off.enabled = false;
        let e = engine_with(&[off]);
        e.run_job(disallow::Job::Sweep(None));
        assert!(e.enforce(pid, create).is_none());
        assert!(f.alive(pid), "disabled rule");
        // enabled, but an exact-path rule without Disallow overrides it
        let e = engine_with(&[Rule::new(&name, dis()), Rule::new(&exe, Actions { priority: Some(Priority::Idle), ..Default::default() })]);
        e.run_job(disallow::Job::Sweep(None));
        assert!(f.alive(pid), "overridden by a more specific rule");
    }

    #[test]
    fn a_start_event_is_only_a_hint_the_live_image_decides() {
        let mut f = Fixture::new();
        let a = f.exe(None, &format!("zz_hayate_disallow_{}_ev_a.exe", std::process::id()));
        let b = f.exe(None, &format!("zz_hayate_disallow_{}_ev_b.exe", std::process::id()));
        let (pa, _) = f.spawn(&a);
        let (pb, _) = f.spawn(&b);
        let e = engine_with(&[Rule::new(&a, dis())]);
        // a stale / forged event: B's pid with A's path. B's live image is not A, so B lives.
        e.on_start(&ProcStart { pid: pb, ppid: 0, session: 0, path: a.clone(), time: 0 });
        assert!(f.alive(pb) && f.alive(pa));
        // A's pid with a bogus path: the live image matches, so A is ended
        e.on_start(&ProcStart { pid: pa, ppid: 0, session: 0, path: "C:\\bogus\\other.exe".into(), time: 0 });
        assert_eq!(f.exit_code(pa), Some(1));
        assert!(f.alive(pb));
    }

    #[test]
    fn essential_hayate_and_own_processes_are_never_ended_even_if_a_flag_gets_past_the_load_check() {
        let mut f = Fixture::new();
        // programs named like essential system / Hayate programs, as disposable copies
        let svc = f.exe(Some("a"), "svchost.exe");
        let hayate = f.exe(Some("b"), "hayate-fixture.exe");
        let (ps, cs) = f.spawn(&svc);
        let (pb, cb) = f.spawn(&hayate);
        let me = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        let my_create = osutil::identity(std::process::id()).unwrap().0;
        // loading drops these flags (see the load test); defence in depth: even with the flag forced on
        // in memory every ending is refused at run time
        let e = engine_with(&[Rule::new("svchost.exe", dis()), Rule::new("hayate-fixture.exe", dis()), Rule::new(&me, dis())]);
        for p in ["svchost.exe", "hayate-fixture.exe", me.as_str()] {
            force_flag(&e, p);
        }
        let (_, out) = e.enforce(ps, cs).expect("matched, refused");
        assert!(!out.ok && out.protected && out.text.contains("essential"), "{out:?}");
        let (_, out) = e.enforce(pb, cb).expect("matched, refused");
        assert!(!out.ok && out.protected && out.text.contains("Hayate"), "{out:?}");
        let (_, out) = e.enforce(std::process::id(), my_create).expect("matched, refused");
        assert!(!out.ok && out.protected, "{out:?}");
        assert!(f.alive(ps) && f.alive(pb));
        // and no such rule can be created through the editing API
        let e2 = engine_with(&[]);
        assert!(e2.set_disallow("svchost.exe", ENABLE).is_err());
        assert!(e2.set_disallow("Hayate.exe", ENABLE).is_err());
        assert!(e2.set_disallow(&me, ENABLE).is_err());
        assert!(e2.set_disallow(r"C:\Windows\System32\notepad.exe", ENABLE).is_err());
        assert!(e2.rules().is_empty());
    }

    #[test]
    fn a_restart_storm_is_bounded_and_says_so() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_storm.exe", std::process::id());
        let exe = f.exe(None, &name);
        let e = engine_with(&[Rule::new(&name, dis())]);
        // fill the window as a restart loop would
        let now = disallow::now_ms();
        {
            let mut g = e.guards.lock().unwrap();
            let guard = g.rules.entry(1).or_default();
            for i in 0..disallow::STORM_MAX as u64 {
                assert_eq!(guard.gate(now + i), disallow::Gate::Go);
            }
        }
        let (pid, create) = f.spawn(&exe);
        let (_, out) = e.enforce(pid, create).expect("pause is reported once");
        assert!(!out.ok && out.text.contains("paused") && out.text.contains("keeps restarting"), "{out:?}");
        assert!(f.alive(pid), "a paused rule does not end it");
        let (pid2, create2) = f.spawn(&exe);
        assert!(e.enforce(pid2, create2).is_none(), "silent while paused");
        assert!(f.alive(pid2));
    }

    #[test]
    fn set_disallow_validates_scope() {
        let e = engine_with(&[]);
        assert!(e.set_disallow("", ENABLE).is_err());
        assert!(e.set_disallow("*.exe", ENABLE).is_err(), "no wildcards");
        assert!(e.set_disallow("game?.exe", ENABLE).is_err());
        assert!(e.set_disallow(r"games\g.exe", ENABLE).is_err(), "a path rule needs a full path");
        assert!(e.rules().is_empty());
        // off stays off; nothing is ended
        let id = e.set_disallow("zz_hayate_off_fixture.exe", KEEP_OFF).unwrap();
        assert!(id.created);
        assert!(!e.get(id.id).unwrap().enabled && e.get(id.id).unwrap().actions.disallow);
        assert!(!e.has_blocking());
        assert!(e.clear_disallow(id.id, None));
        assert!(e.get(id.id).is_none(), "a Disallow-only rule goes entirely");
        assert!(!e.clear_disallow(999, None));
    }

    #[test]
    fn keep_off_never_touches_an_enabled_rule_and_keeps_a_disabled_rules_settings() {
        let e = engine_with(&[]);
        // REGRESSION: an enabled priority rule + "Save, keep OFF" must not disable or alter the rule
        let kid = e.add(Rule::new("zz_hayate_prio_fixture.exe", Actions { priority: Some(Priority::High), ..Default::default() }));
        let before = e.get(kid).unwrap();
        assert!(before.enabled);
        let err = e.set_disallow("ZZ_HAYATE_PRIO_FIXTURE.EXE", KEEP_OFF).unwrap_err();
        assert!(err.contains("enabled rule") && err.contains("nothing was changed"), "{err}");
        assert_eq!(e.get(kid).unwrap(), before, "priority rule still on, still High, no Disallow flag");
        assert!(e.rule_for(r"C:\x\zz_hayate_prio_fixture.exe").is_some_and(|r| r.actions.priority == Some(Priority::High)));
        assert_eq!(e.rules().len(), 1, "same pattern, one rule");
        // a DISABLED rule: the flag is stored, the rule stays disabled, its settings stay saved
        let mut r = Rule::new("zz_hayate_keep_fixture.exe", Actions { priority: Some(Priority::Idle), ..Default::default() });
        r.enabled = false;
        let did = e.add(r);
        let saved = e.set_disallow("zz_hayate_keep_fixture.exe", KEEP_OFF).unwrap();
        assert_eq!((saved.id, saved.created, saved.was_enabled), (did, false, false));
        let r = e.get(did).unwrap();
        assert!(!r.enabled && r.actions.disallow && r.actions.priority == Some(Priority::Idle));
        assert!(!e.has_blocking());
        // an already-enabled Disallow rule cannot be "kept off" either: it says it is ON
        e.set_enabled(did, true);
        assert!(e.set_disallow("zz_hayate_keep_fixture.exe", KEEP_OFF).unwrap_err().contains("it is ON"));
        assert!(e.get(did).unwrap().enabled);
    }

    #[test]
    fn enabling_disallow_on_an_existing_rule_needs_an_explicit_whole_rule_confirmation() {
        let e = engine_with(&[]);
        let mut r = Rule::new("zz_hayate_whole_fixture.exe", Actions { priority: Some(Priority::Idle), ..Default::default() });
        r.enabled = false;
        let off = e.add(r);
        let ask = disallow::DisallowMode::Enable { confirm_whole_rule: false };
        let ctx = e.disallow_context("zz_hayate_whole_fixture.exe");
        assert_eq!((ctx.id, ctx.enabled, ctx.flag, ctx.other.as_str()), (Some(off), false, false, "Idle"));
        let err = e.set_disallow("zz_hayate_whole_fixture.exe", ask).unwrap_err();
        assert!(err.contains("WHOLE rule") && err.contains("currently OFF"), "{err}");
        assert!(!e.get(off).unwrap().enabled && !e.get(off).unwrap().actions.disallow, "an unconfirmed ask changed nothing");
        let saved = e.set_disallow("zz_hayate_whole_fixture.exe", ENABLE).unwrap();
        assert!(!saved.was_enabled);
        let r = e.get(off).unwrap();
        assert!(r.enabled && r.actions.disallow && r.actions.priority == Some(Priority::Idle), "settings stay saved");
        // the rule now blocks: block wins, the saved priority is not applied (and not lost)
        assert!(e.rule_for(r"C:\x\zz_hayate_whole_fixture.exe").is_some_and(|r| r.blocks()));
        // UNDO with the remembered switch: Disallow goes, the rule goes back to OFF, its priority does NOT start applying
        assert!(e.clear_disallow(off, Some(saved.was_enabled)));
        let r = e.get(off).unwrap();
        assert!(!r.actions.disallow && !r.enabled && r.actions.priority == Some(Priority::Idle));
        assert!(e.rule_for(r"C:\x\zz_hayate_whole_fixture.exe").is_none(), "undo did not silently turn the other settings on");
        // an ENABLED rule with settings needs the same confirmation (its priority stops applying)
        let on = e.add(Rule::new("zz_hayate_whole2_fixture.exe", Actions { priority: Some(Priority::High), ..Default::default() }));
        assert!(e.set_disallow("zz_hayate_whole2_fixture.exe", ask).unwrap_err().contains("stops those settings"));
        assert!(e.get(on).unwrap().actions.priority == Some(Priority::High) && !e.get(on).unwrap().actions.disallow);
        let s2 = e.set_disallow("zz_hayate_whole2_fixture.exe", ENABLE).unwrap();
        assert!(s2.was_enabled);
        assert!(e.clear_disallow(on, Some(true)));
        assert!(e.get(on).unwrap().enabled && e.get(on).unwrap().actions.priority == Some(Priority::High));
        // a rule that is only Disallow needs no ask (nothing else to change)
        let only = e.set_disallow("zz_hayate_only_fixture.exe", KEEP_OFF).unwrap();
        assert!(e.set_disallow("zz_hayate_only_fixture.exe", ask).is_ok());
        assert!(e.get(only.id).unwrap().enabled);
    }

    #[test]
    fn disallow_rules_in_the_clipboard_arrive_off_and_never_move_between_programs() {
        let e = engine_with(&[Rule::new("zz_hayate_src.exe", Actions { disallow: true, io: Some(IoPriority::Low), ..Default::default() })]);
        let text = e.copy(1).unwrap();
        assert!(text.contains("disallow = true"));
        // paste as new rules: imported OFF
        let ids = e.paste(&text, None).unwrap();
        let pasted = e.get(ids[0]).unwrap();
        assert!(pasted.actions.disallow && !pasted.enabled, "{pasted:?}");
        // paste onto another program: the settings move, the block does not
        let ids = e.paste(&text, Some("zz_hayate_dst.exe")).unwrap();
        let dst = e.get(ids[0]).unwrap();
        assert!(!dst.actions.disallow && dst.actions.io == Some(IoPriority::Low));
        // paste onto a program that already has a block keeps it
        let kid = e.add(Rule::new("zz_hayate_keep2.exe", dis()));
        e.paste(&text, Some("zz_hayate_keep2.exe")).unwrap();
        assert!(e.get(kid).unwrap().actions.disallow);
    }

    #[test]
    fn a_wildcard_or_protected_name_cannot_be_given_a_block_through_add_or_update() {
        let e = engine_with(&[]);
        let id = e.add(Rule::new("*.exe", dis()));
        assert!(!e.get(id).unwrap().actions.disallow, "glob");
        let id = e.add(Rule::new("csrss.exe", dis()));
        assert!(!e.get(id).unwrap().actions.disallow, "essential");
        let id = e.add(Rule::new("zz_hayate_ok.exe", Actions { io: Some(IoPriority::Low), ..Default::default() }));
        let mut r = e.get(id).unwrap();
        r.pattern = "zz_hayate_*.exe".into();
        r.actions.disallow = true;
        assert!(e.update(r));
        assert!(!e.get(id).unwrap().actions.disallow);
        assert!(!e.has_blocking());
    }

    #[test]
    fn disallow_save_reports_a_failed_write_and_a_retry_before_anything_claims_success() {
        let e = engine_with(&[]);
        let tmp = e.file.with_extension("tmp");
        std::fs::create_dir(&tmp).unwrap();
        e.set_disallow("zz_hayate_persist.exe", KEEP_OFF).unwrap();
        let st = e.persistence().unwrap();
        assert!(matches!(&*st.lock().unwrap(), Some(Err(_))), "the failure is visible, not swallowed");
        assert!(!std::fs::read_to_string(&e.file).unwrap_or_default().contains("zz_hayate_persist"), "nothing reached the disk");
        std::fs::remove_dir(&tmp).unwrap();
        e.set_disallow("zz_hayate_persist.exe", KEEP_OFF).unwrap();
        assert!(matches!(&*e.persistence().unwrap().lock().unwrap(), Some(Ok(()))));
        let text = std::fs::read_to_string(&e.file).unwrap();
        assert!(text.contains("pattern = zz_hayate_persist.exe") && text.contains("disallow = true") && text.contains("enabled = false"), "{text}");
        // and a fresh engine reading that file sees the same rule, still off
        let e2 = Engine::load(e.file.clone());
        let r = e2.rules().remove(0);
        assert!(r.actions.disallow && !r.enabled && !e2.has_blocking());
    }

    // ---- overload recovery (items: cooldown, overflow, disable race, worker failure) -------------------

    #[test]
    fn a_paused_rules_survivors_are_ended_when_the_pause_ends_with_no_new_event() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_cool.exe", std::process::id());
        let exe = f.exe(None, &name);
        let e = arc(&[Rule::new(&name, dis())]);
        e.start_worker_with(&std_spawn);
        assert!(e.worker_problem().is_none());
        let (p1, _) = f.spawn(&exe);
        let (p2, _) = f.spawn(&exe);
        // the storm guard paused the rule; the pause ends in a moment. NO start event will ever come.
        pause_ending_in(&e, 1, 700);
        e.submit(disallow::Job::Start { pid: 0, create: 0 }); // wakes the worker so it sees the new deadline (a no-op job)
        assert!(f.alive(p1) && f.alive(p2), "nothing ended during the pause");
        assert!(wait_dead(&mut f, &[p1, p2], 8_000), "the survivors were swept at the end of the pause by the worker's own deadline");
        let r = e.rules().remove(0);
        assert_eq!(r.hits, 2, "{r:?}");
        assert!(e.guards.lock().unwrap().rules.get(&1).is_some_and(|g| g.recover_at().is_none()), "the deadline is used up");
    }

    #[test]
    fn a_continuous_stream_of_jobs_cannot_starve_the_recovery() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_flood.exe", std::process::id());
        let exe = f.exe(None, &name);
        let e = arc(&[Rule::new(&name, dis())]);
        e.start_worker_with(&std_spawn);
        let (p1, _) = f.spawn(&exe);
        pause_ending_in(&e, 1, 500);
        // jobs arrive every few ms for the whole time: the worker's recv never times out
        let t0 = std::time::Instant::now();
        let mut ended = false;
        while t0.elapsed() < std::time::Duration::from_secs(8) {
            e.submit(disallow::Job::Start { pid: 0, create: 0 });
            std::thread::sleep(std::time::Duration::from_millis(5));
            if !f.alive(p1) {
                ended = true;
                break;
            }
        }
        assert!(ended, "recovery ran between jobs");
    }

    #[test]
    fn repeated_overload_requeues_the_cooldown_and_never_ends_more_than_the_rate_allows() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_again.exe", std::process::id());
        let exe = f.exe(None, &name);
        let e = engine_with(&[Rule::new(&name, dis())]);
        let (a, _) = f.spawn(&exe);
        let (b, _) = f.spawn(&exe);
        let (c, _) = f.spawn(&exe);
        let now = disallow::now_ms();
        {
            // a pause that is already over (recovery due) and a window with exactly one attempt left
            let mut r = e.guards.lock().unwrap();
            let g = r.rules.entry(1).or_default();
            let t0 = now.saturating_sub(disallow::STORM_PAUSE_MS + 2_000);
            for _ in 0..disallow::STORM_MAX {
                g.gate(t0);
            }
            assert_eq!(g.gate(t0), disallow::Gate::PauseStarts);
            for _ in 0..disallow::STORM_MAX - 1 {
                assert_eq!(g.gate(now), disallow::Gate::Go);
            }
        }
        e.run_due(now + 60_000);
        let dead = [a, b, c].iter().filter(|p| !f.alive(**p)).count();
        assert_eq!(dead, 1, "the usual rate path allowed ONE more ending, then paused again");
        let again = e.guards.lock().unwrap().rules.get(&1).and_then(|g| g.recover_at());
        assert!(again.is_some_and(|t| t > now + 10_000), "the overload armed a fresh cooldown deadline: {again:?}");
        // the next due sweep (real clock still inside the new pause) ends nothing and re-arms again
        e.run_due(u64::MAX);
        assert_eq!([a, b, c].iter().filter(|p| !f.alive(**p)).count(), 1);
        assert!(e.guards.lock().unwrap().rules.get(&1).and_then(|g| g.recover_at()).is_some());
    }

    #[test]
    fn disabling_or_deleting_the_rule_cancels_its_cooldown_and_guard() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_cancel.exe", std::process::id());
        let exe = f.exe(None, &name);
        let e = engine_with(&[Rule::new(&name, dis())]);
        let (p1, _) = f.spawn(&exe);
        pause_ending_in(&e, 1, 10);
        e.note_overflow(disallow::now_ms());
        assert!(e.next_deadline().is_some());
        // disable: guard, deadline and the overflow mark are all gone; a late sweep ends nothing
        assert!(e.set_enabled(1, false));
        assert!(e.guards.lock().unwrap().rules.is_empty());
        assert!(e.guards.lock().unwrap().overflow_at.is_none());
        assert_eq!(e.next_deadline(), None);
        e.run_due(u64::MAX);
        assert!(f.alive(p1));
        // re-enable starts from a fresh guard (no leftover pause or deadline). Enabling sweeps what runs
        // now (that is the rule doing its job), so the first survivor is cleared away before.
        f.kids.iter_mut().find(|k| k.id() == p1).unwrap().kill().unwrap();
        let _ = f.exit_code(p1);
        assert!(e.set_enabled(1, true));
        assert_eq!(e.next_deadline(), None);
        // ...and deleting the rule drops a new guard and its deadline
        let (p2, _) = f.spawn(&exe);
        pause_ending_in(&e, 1, 10);
        assert!(e.remove(1));
        assert!(e.guards.lock().unwrap().rules.is_empty());
        e.run_due(u64::MAX);
        assert!(f.alive(p2));
        // a guard for a rule that does not exist is pruned by the next save
        e.guards.lock().unwrap().rules.entry(77).or_default();
        e.set_settings(Settings::default());
        assert!(e.guards.lock().unwrap().rules.is_empty());
    }

    #[test]
    fn guards_follow_enable_edit_clear_and_never_outgrow_the_rule_list() {
        let e = engine_with(&[Rule::new("zz_hayate_g1.exe", dis()), Rule::new("zz_hayate_g2.exe", dis())]);
        {
            let mut r = e.guards.lock().unwrap();
            r.rules.entry(1).or_default().settle((9, 9));
            r.rules.entry(2).or_default().settle((8, 8));
        }
        // editing a rule starts it over (its settled answers do not carry across)
        let mut r = e.get(1).unwrap();
        r.pattern = "zz_hayate_g1b.exe".into();
        assert!(e.update(r));
        assert!(!e.guards.lock().unwrap().rules.contains_key(&1));
        assert!(e.guards.lock().unwrap().rules.contains_key(&2), "the other rule's guard is untouched");
        // clearing Disallow from the rule drops its guard
        assert!(e.clear_disallow(2, None));
        assert!(e.guards.lock().unwrap().rules.is_empty());
        // many unrelated ids never accumulate
        for id in 100..400 {
            e.guards.lock().unwrap().rules.entry(id).or_default();
        }
        e.set_settings(Settings::default());
        assert!(e.guards.lock().unwrap().rules.is_empty());
    }

    #[test]
    fn a_full_queue_marks_one_coalesced_recovery_that_reconciles_through_the_usual_rate_path() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_over.exe", std::process::id());
        let exe = f.exe(None, &name);
        let e = engine_with(&[Rule::new(&name, dis())]);
        // a worker that never drains: capacity 1
        let (tx, rx) = std::sync::mpsc::sync_channel::<disallow::Job>(1);
        e.jobs.set(tx).unwrap();
        let (a, ca) = f.spawn(&exe);
        let (b, cb) = f.spawn(&exe);
        let (c, cc) = f.spawn(&exe);
        e.submit(disallow::Job::Start { pid: a, create: ca }); // fits
        assert!(e.guards.lock().unwrap().overflow_at.is_none());
        e.submit(disallow::Job::Start { pid: b, create: cb }); // dropped
        let mark = e.guards.lock().unwrap().overflow_at.expect("dropped job leaves a mark");
        e.submit(disallow::Job::Start { pid: c, create: cc }); // dropped again: same mark (coalesced)
        e.submit(disallow::Job::Sweep(None));
        assert_eq!(e.guards.lock().unwrap().overflow_at, Some(mark));
        assert!(mark > disallow::now_ms(), "the recovery is in the future, not immediate");
        assert!(f.alive(a) && f.alive(b) && f.alive(c), "nothing ended by a dropped job");
        // silence: nobody sends anything. Before the mark nothing is due...
        e.run_due(mark - 1);
        assert!(f.alive(b) && f.alive(c));
        // ...at the mark the worker sweeps every running process through the normal gate (here one attempt is left)
        {
            let mut r = e.guards.lock().unwrap();
            let g = r.rules.entry(1).or_default();
            for _ in 0..disallow::STORM_MAX - 1 {
                g.gate(disallow::now_ms());
            }
        }
        e.run_due(mark);
        assert_eq!([a, b, c].iter().filter(|p| !f.alive(**p)).count(), 1, "rate path honoured");
        assert!(e.guards.lock().unwrap().overflow_at.is_none(), "the mark is consumed");
        assert!(e.guards.lock().unwrap().rules.get(&1).and_then(|g| g.recover_at()).is_some(), "the survivors are queued for the cooldown sweep");
        drop(rx);
    }

    #[test]
    fn a_worker_that_cannot_start_or_has_stopped_is_surfaced_not_silent() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_nowork.exe", std::process::id());
        let exe = f.exe(None, &name);
        let (p1, _) = f.spawn(&exe);
        let e = arc(&[Rule::new(&name, dis())]);
        e.start_worker_with(&|_body| Err(std::io::Error::other("no threads left")));
        let why = e.worker_problem().expect("the failure is recorded");
        assert!(why.contains("could not start") && why.contains("nothing will be ended"), "{why}");
        // nothing runs on the caller's thread, nothing is ended, and a rule cannot be turned ON
        e.submit(disallow::Job::Sweep(None));
        e.submit(disallow::Job::Start { pid: p1, create: osutil::identity(p1).unwrap().0 });
        assert!(f.alive(p1));
        let err = e.set_disallow("zz_hayate_newrule.exe", ENABLE).unwrap_err();
        assert!(err.contains("cannot run"), "{err}");
        assert!(e.set_disallow("zz_hayate_newrule.exe", KEEP_OFF).is_ok(), "saving OFF is still honest");
        // a worker thread that dies is noticed on the next job
        let e2 = arc(&[]);
        let (tx, rx) = std::sync::mpsc::sync_channel::<disallow::Job>(4);
        e2.jobs.set(tx).unwrap();
        drop(rx);
        e2.submit(disallow::Job::Sweep(None));
        assert!(e2.worker_problem().is_some_and(|w| w.contains("stopped")));
    }

    #[test]
    fn access_denied_and_protected_answers_are_final_for_that_instance_and_pruned_when_it_exits() {
        let mut f = Fixture::new();
        // a Hayate-named program is always refused (a unique name, so no real process matches the rule)
        let name = format!("hayate-zz-disallow-{}.exe", std::process::id());
        let svc = f.exe(Some("a"), &name);
        let (p, c) = f.spawn(&svc);
        let e = engine_with(&[Rule::new(&name, dis())]);
        force_flag(&e, &name);
        e.run_job(disallow::Job::Sweep(None));
        assert_eq!(e.rules()[0].hits, 1);
        assert!(e.guards.lock().unwrap().rules.get(&1).is_some_and(|g| g.is_settled((p, c))));
        // a recovery sweep (or any later sweep) does not try again
        e.run_job(disallow::Job::Sweep(None));
        e.run_job(disallow::Job::Sweep(Some(1)));
        assert_eq!(e.rules()[0].hits, 1, "no permanent retries of a protected process");
        assert!(f.alive(p));
        // The instance exits: periodic query-only pruning forgets it once exit is proven.
        f.kids.iter_mut().find(|k| k.id() == p).unwrap().kill().unwrap();
        let _ = f.exit_code(p);
        e.run_due(disallow::now_ms() + disallow::IDLE_WAIT_MS);
        assert!(e.guards.lock().unwrap().rules.get(&1).is_some_and(|g| !g.is_settled((p, c))));
    }

    #[test]
    fn disabling_before_the_final_check_ends_nothing() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_race1.exe", std::process::id());
        let exe = f.exe(None, &name);
        let e = arc(&[Rule::new(&name, dis())]);
        let (p, c) = f.spawn(&exe);
        let out = e.enforce_with(p, c, &|st| {
            if st == disallow::Stage::BeforeFinalCheck {
                assert!(e.set_enabled(1, false));
            }
        });
        assert!(out.is_none(), "{out:?}");
        assert!(f.alive(p), "a rule switched off after the safeguards but before the final check ends nothing");
    }

    #[test]
    fn a_disable_cannot_slip_between_the_final_check_and_the_terminate_call() {
        let mut f = Fixture::new();
        let name = format!("zz_hayate_disallow_{}_race2.exe", std::process::id());
        let exe = f.exe(None, &name);
        let e = arc(&[Rule::new(&name, dis())]);
        let (p, c) = f.spawn(&exe);
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
        let out = e.enforce_with(p, c, &|st| {
            if st == disallow::Stage::InsideLock {
                // the final check passed and the engine lock is held: a disable started now must WAIT
                let (e2, d2) = (e.clone(), done.clone());
                *handle.lock().unwrap() = Some(std::thread::spawn(move || {
                    e2.set_enabled(1, false);
                    d2.store(true, std::sync::atomic::Ordering::SeqCst);
                }));
                std::thread::sleep(std::time::Duration::from_millis(400));
                assert!(!done.load(std::sync::atomic::Ordering::SeqCst), "the disable ran while the terminate decision was being made");
            }
        });
        let (id, o) = out.expect("terminated");
        assert!(id == 1 && o.ok, "{o:?}");
        handle.lock().unwrap().take().unwrap().join().unwrap();
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(f.exit_code(p), Some(1));
        assert!(!e.has_blocking(), "and the disable then took effect");
    }

    // ---- load / edit checks (items: protected patterns, visible refusals) ------------------------------

    #[test]
    fn a_protected_or_glob_disallow_in_the_file_is_dropped_at_load_with_a_visible_warning_never_shown_on() {
        let dir = std::env::temp_dir().join(format!("hayate-rules-load-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("rules.ini");
        std::fs::write(
            &f,
            "[rule]\npattern = svchost.exe\nenabled = true\ndisallow = true\n\n[rule]\npattern = C:\\Windows\\System32\\notepad.exe\nenabled = true\ndisallow = true\npriority = high\n\n[rule]\npattern = hayate-x.exe\nenabled = true\ndisallow = true\n\n[rule]\npattern = zz_hayate_fine.exe\nenabled = true\ndisallow = true\n\n[rule]\npattern = C:\\Windows\\.\\System32\\.\\calc.exe\nenabled = true\ndisallow = true\n",
        )
        .unwrap();
        let e = Engine::load(f);
        let rules = e.rules();
        assert!(rules.iter().filter(|r| r.blocks()).count() == 1 && rules[3].blocks(), "only the ordinary program blocks: {rules:?}");
        assert!(!rules[0].enabled && !rules[0].actions.disallow, "an empty rule is left OFF, not 'ON doing nothing'");
        assert!(rules[1].enabled && rules[1].actions.priority == Some(Priority::High) && !rules[1].actions.disallow, "other settings survive");
        assert!(!rules[4].actions.disallow, "path forms with dots are judged canonically");
        let w = e.warnings();
        assert_eq!(w.iter().filter(|w| w.contains("Disallow ignored")).count(), 4, "{w:?}");
        assert!(w.iter().any(|w| w.contains("svchost.exe") && w.contains("essential") && w.contains("rule is OFF")), "{w:?}");
        assert!(w.iter().any(|w| w.contains("Windows folder") && w.contains("unchanged")), "{w:?}");
    }

    #[test]
    fn an_edit_that_loses_the_flag_says_so_in_the_visible_warnings() {
        let e = engine_with(&[]);
        let id = e.add(Rule::new("csrss.exe", dis()));
        assert!(!e.get(id).unwrap().actions.disallow);
        assert!(e.warnings().iter().any(|w| w.contains("csrss.exe") && w.contains("Disallow ignored")), "{:?}", e.warnings());
        let id = e.add(Rule::new("zz_hayate_*.exe", dis()));
        assert!(e.warnings().iter().any(|w| w.contains("zz_hayate_*.exe") && w.contains("never be disallowed")), "{:?}", e.warnings());
        let mut r = e.get(id).unwrap();
        r.pattern = "hayate-update.exe".into();
        r.actions.disallow = true;
        e.update(r);
        assert!(e.warnings().iter().any(|w| w.contains("hayate-update.exe")));
        // the list stays short and has no repeats
        for _ in 0..50 {
            e.add(Rule::new("csrss.exe", dis()));
        }
        assert!(e.warnings().len() <= 20);
        assert_eq!(e.warnings().iter().filter(|w| w.contains("rule 'csrss.exe'")).count(), 1);
    }

    #[test]
    fn a_failed_write_is_retried_by_a_real_write_even_when_the_change_is_already_on() {
        let e = engine_with(&[]);
        let tmp = e.file.with_extension("tmp");
        std::fs::create_dir(&tmp).unwrap();
        let saved = e.set_disallow("zz_hayate_retry_fixture.exe", ENABLE).unwrap();
        assert!(e.get(saved.id).unwrap().blocks(), "on in memory");
        assert!(matches!(&*e.persistence().unwrap().lock().unwrap(), Some(Err(_))));
        assert!(!std::fs::read_to_string(&e.file).unwrap_or_default().contains("zz_hayate_retry_fixture"));
        std::fs::remove_dir(&tmp).unwrap();
        // nothing about the rule changes; the retry still performs an actual write
        let status = e.save_again();
        assert!(matches!(&*status.lock().unwrap(), Some(Ok(()))));
        let text = std::fs::read_to_string(&e.file).unwrap();
        assert!(text.contains("pattern = zz_hayate_retry_fixture.exe") && text.contains("disallow = true") && text.contains("enabled = true"), "{text}");
    }

    #[test]
    fn presets() {
        let p = ccd_presets(&[(0xFFFF, 96 << 20), (0xFFFF_0000, 32 << 20)]);
        assert_eq!(p[0], Preset { label: "CCD0 · V-Cache (0-15)".into(), mask: 0xFFFF });
        assert_eq!(p[1].label, "CCD1 · Frequency (16-31)");
        assert!(ccd_presets(&[(0xFF, 1)]).is_empty());
    }
}
