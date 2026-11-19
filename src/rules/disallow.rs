//! "Disallow running": end a process that matches an enabled Disallow rule.
//!
//! This is detection followed by termination, NOT launch prevention and NOT a
//! security sandbox: the program may run for a moment before Blink ends it, it is
//! only ended while Blink runs with enough rights, and anything that can start
//! faster or stronger than Blink wins. No driver, service, scheduled task,
//! Image File Execution Options key, firewall rule or elevation is involved.
//!
//! How one process is ended ([`Engine::enforce`], always on the blink-disallow
//! worker, never on the UI thread and never on the ETW callback):
//! 1. a handle opened against PID **and creation time** (`osutil::open_verified`)
//!    is the only way the process is touched; while it is open the PID cannot be
//!    reused;
//! 2. the program file comes from that handle (`image_of_handle`), never from the
//!    start event or a list: no image, no action;
//! 3. the live image is matched with the engine's own precedence (exact path, then
//!    name; globs never end anything);
//! 4. every [`protect`] layer must pass, with no override;
//! 5. the storm guard ([`Guard`]) bounds the attempts per rule;
//! 6. a second verified handle with terminate rights is opened; the final
//!    enabled/match check and the `TerminateProcess` call then run under ONE hold of
//!    the rules lock (a disable cannot slip between them); only the 1.5 s wait on the
//!    handle is outside it.
//!
//! Overload never loses work for good: a rule paused by the storm guard arms a
//! per-rule recovery deadline (pause end + 0.5 s) and the worker then sweeps the
//! survivors through steps 1-6 again; a full queue marks one coalesced recovery that
//! re-checks every running process. The worker checks its deadlines after every job
//! (a continuous stream cannot starve them) and wakes by timeout when idle. Denied /
//! protected answers are final for that process instance (never retried); a disabled,
//! edited or removed rule drops its guard, deadline and memory.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::TrySendError;

use windows::Win32::Foundation::{WAIT_OBJECT_0, ERROR_ACCESS_DENIED};
use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE};

use super::apply::Outcome;
use super::model::{Actions, MatchKind, Rule};
use super::protect;
use super::{Engine, Inner, SaveStatus};
use crate::osutil::{self, OpenFail};

// ---- storm guard ------------------------------------------------------------

/// More than this many attempts for one rule inside `STORM_WINDOW_MS` is a restart
/// loop (an app or a service that relaunches itself every moment).
pub const STORM_MAX: usize = 30;
pub const STORM_WINDOW_MS: u64 = 10_000;
/// How long the rule then stands aside. Copies that start during the pause are not ended
/// by their start event, but the recovery sweep below picks the survivors up afterwards.
pub const STORM_PAUSE_MS: u64 = 15_000;
/// A rule writes at most one log line per this long (counts are kept in the rule's stats).
pub const LOG_EVERY_MS: u64 = 30_000;
/// After a pause ends, the worker waits this much longer, then sweeps the survivors.
pub const RECOVER_SLACK_MS: u64 = 500;
/// A full queue marks ONE coalesced recovery this long after the first dropped job: the
/// worker then re-checks every running process through the usual rate path.
pub const OVERFLOW_RECOVER_MS: u64 = 3_000;
/// The worker wakes at least this often even when nothing is due.
pub const IDLE_WAIT_MS: u64 = 30_000;
/// Monotonic time for recovery deadlines, unaffected by wall-clock adjustments.
pub fn now_ms() -> u64 {
    static START: std::sync::OnceLock<(u64, std::time::Instant)> = std::sync::OnceLock::new();
    let (base, start) = START.get_or_init(|| (osutil::now_ms(), std::time::Instant::now()));
    base.saturating_add(start.elapsed().as_millis() as u64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Go,
    /// the pause starts now: say so once
    PauseStarts,
    /// still pausing: do nothing, say nothing
    Paused,
}

/// Per rule: bounds the attempts and the log lines, and owns the rule's recovery deadline
/// and the instances that were already given a final (non-retryable) answer. Pure (time is
/// passed in). One `Guard` exists only while its rule exists and is a blocking rule.
#[derive(Debug, Default)]
pub struct Guard {
    stamps: VecDeque<u64>,
    paused_until: u64,
    last_log: u64,
    /// when the worker must sweep this rule's survivors (set by a pause, one per pause)
    recover_at: Option<u64>,
    /// (pid, creation time) of processes that were access-denied / protected: never retried
    /// by a recovery sweep. Kept until exit rather than evicting still-live answers.
    settled: HashSet<(u32, i64)>,
}

impl Guard {
    pub fn gate(&mut self, now: u64) -> Gate {
        if now < self.paused_until {
            self.arm();
            return Gate::Paused;
        }
        while self.stamps.front().is_some_and(|t| now.saturating_sub(*t) > STORM_WINDOW_MS) {
            self.stamps.pop_front();
        }
        if self.stamps.len() >= STORM_MAX {
            self.stamps.clear();
            self.paused_until = now + STORM_PAUSE_MS;
            self.arm();
            return Gate::PauseStarts;
        }
        self.stamps.push_back(now);
        Gate::Go
    }

    /// Schedule the survivor sweep for the end of the current pause (once per pause).
    fn arm(&mut self) {
        if self.recover_at.is_none() {
            self.recover_at = Some(self.paused_until + RECOVER_SLACK_MS);
        }
    }

    /// When the sweep is due (None: nothing pending).
    pub fn recover_at(&self) -> Option<u64> {
        self.recover_at
    }

    /// True once when the sweep is due (and disarms it; a new pause arms it again).
    pub fn take_due(&mut self, now: u64) -> bool {
        if self.recover_at.is_some_and(|t| t <= now) {
            self.recover_at = None;
            true
        } else {
            false
        }
    }

    /// True at most once per `LOG_EVERY_MS`.
    pub fn may_log(&mut self, now: u64) -> bool {
        if self.last_log == 0 || now.saturating_sub(self.last_log) >= LOG_EVERY_MS {
            self.last_log = now.max(1);
            true
        } else {
            false
        }
    }

    pub fn is_settled(&self, who: (u32, i64)) -> bool {
        self.settled.contains(&who)
    }

    pub fn settle(&mut self, who: (u32, i64)) {
        self.settled.insert(who);
    }

    /// Forget settled instances that no longer run.
    pub fn keep_settled(&mut self, live: &HashSet<(u32, i64)>) {
        self.settled.retain(|w| live.contains(w));
    }
}

/// Everything the worker needs to recover from overload: per-rule guards (keyed by rule id,
/// pruned with the rules) and the coalesced queue-overflow mark.
#[derive(Debug, Default)]
pub struct Recov {
    pub rules: HashMap<u32, Guard>,
    /// a job was dropped because the queue was full: reconcile everything at this time
    pub overflow_at: Option<u64>,
    overflow_log: Guard,
    pruned_at: u64,
}

pub type Guards = std::sync::Mutex<Recov>;

/// Test seam for the final-check race (production passes a no-op).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// terminate handle open, final enabled/match check not yet done
    BeforeFinalCheck,
    /// the final check passed; the engine lock is held and `TerminateProcess` is next
    InsideLock,
}

// ---- jobs -------------------------------------------------------------------

/// Work for the blink-disallow worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    /// a process started: identity read at the event
    Start { pid: u32, create: i64 },
    /// every running process (all rules, or one rule)
    Sweep(Option<u32>),
}

pub const QUEUE: usize = 256;

// ---- outcome text -----------------------------------------------------------

fn denied_text(is_admin: bool) -> String {
    if is_admin {
        "access denied: Windows would not let Hayate end this process (protected process)".into()
    } else {
        "access denied: Hayate is not running as administrator, so it cannot end elevated or protected processes".into()
    }
}

/// Map a refused open of the terminate handle to the recorded outcome (pure).
pub fn open_failure(f: &OpenFail, is_admin: bool) -> Option<Outcome> {
    match f {
        OpenFail::Denied => Some(Outcome { ok: false, protected: true, gone: false, text: denied_text(is_admin) }),
        OpenFail::Gone | OpenFail::NoIdentity => None,
        OpenFail::Other(e) => Some(Outcome { ok: false, protected: false, gone: false, text: format!("failed: {e}") }),
    }
}

/// What the Rules tab and the panel need to know before Disallow is saved for a pattern.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisallowContext {
    /// a rule with exactly this pattern exists
    pub id: Option<u32>,
    pub enabled: bool,
    /// that rule already carries Disallow
    pub flag: bool,
    /// summary of its OTHER settings ("High · I/O low"); empty = none
    pub other: String,
}

/// How [`Engine::set_disallow`] saves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisallowMode {
    /// store the flag, enforce nothing. An existing ENABLED rule cannot hold a dormant flag
    /// (one switch per rule), so that case is refused and the rule is left exactly as it is.
    KeepOff,
    /// enforce. `confirm_whole_rule` must be true when the rule exists and turning Disallow on
    /// changes what it does (it is off, or it has other settings that stop being applied).
    Enable { confirm_whole_rule: bool },
}

/// What a successful save changed, so an undo can put it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisallowSaved {
    pub id: u32,
    /// the rule was created by this save
    pub created: bool,
    /// the rule's enabled switch before this save (false for a created rule)
    pub was_enabled: bool,
}

// ---- engine -----------------------------------------------------------------

impl Inner {
    /// The enabled name / exact-path Disallow rule that is EFFECTIVE for this image:
    /// the same precedence as every other action (exact path > name > glob). A more
    /// specific enabled rule without Disallow lets the program run.
    pub(super) fn blocking(&self, name: &str, path: &str) -> Option<u32> {
        let i = self.find(name, path)?;
        let r = &self.rules[i];
        r.blocks().then_some(r.id)
    }

    pub(super) fn any_blocking(&self) -> bool {
        self.rules.iter().any(Rule::blocks)
    }
}

fn has_other(a: &Actions) -> bool {
    !Actions { disallow: false, ..a.clone() }.is_empty()
}

impl Engine {
    /// Is any Disallow rule enabled? (cheap gate for the process-start path)
    pub fn has_blocking(&self) -> bool {
        self.inner.lock().unwrap().any_blocking()
    }

    /// Why Disallow cannot work right now (the worker thread could not start or has stopped).
    /// While this is `Some`, nothing is ended, so no rule may claim to be ON.
    pub fn worker_problem(&self) -> Option<String> {
        self.worker_err.lock().unwrap().clone()
    }

    pub(super) fn fail_worker(&self, why: String) {
        let mut e = self.worker_err.lock().unwrap();
        if e.is_none() {
            osutil::log(&format!("disallow: {why}"));
            *e = Some(why);
        }
    }

    /// Hand a job to the worker. Without a worker (tests) it runs here. A full queue
    /// drops the job but marks ONE coalesced recovery ([`OVERFLOW_RECOVER_MS`] later the
    /// worker re-checks every running process), so a dropped start is not lost for good.
    pub(super) fn submit(&self, job: Job) {
        match self.jobs.get() {
            Some(tx) => match tx.try_send(job) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => self.note_overflow(now_ms()),
                Err(TrySendError::Disconnected(_)) => self.fail_worker("the Disallow worker stopped; nothing is ended any more (restart Hayate)".into()),
            },
            // the worker never started: surfaced through `worker_problem`, nothing may run on this thread
            None if self.worker_problem().is_some() => {}
            None => self.run_job(job),
        }
    }

    pub(super) fn note_overflow(&self, now: u64) {
        let say = {
            let mut r = self.guards.lock().unwrap();
            r.overflow_at.get_or_insert(now + OVERFLOW_RECOVER_MS);
            r.overflow_log.may_log(now)
        };
        if say {
            osutil::log(&format!("disallow: worker queue full, start events were dropped; every running process is re-checked in {} s", OVERFLOW_RECOVER_MS / 1000));
        }
    }

    pub(super) fn run_job(&self, job: Job) {
        match job {
            Job::Start { pid, create } => self.run_one(pid, create),
            Job::Sweep(only) => {
                if !self.has_blocking() {
                    return;
                }
                for (pid, _) in super::running() {
                    // creation time + image from one handle, as the start path does
                    let Some((create, Some(path))) = osutil::identity(pid) else { continue };
                    let hit = {
                        let g = self.inner.lock().unwrap();
                        g.blocking(osutil::file_name(&path), &path)
                    };
                    let Some(id) = hit else { continue };
                    if only.is_some_and(|o| o != id) || self.is_settled(id, (pid, create)) {
                        continue;
                    }
                    self.run_one(pid, create);
                }
            }
        }
    }

    /// Enforce one process and record the result; a denied / protected answer is final for
    /// that instance (recovery sweeps never retry it).
    fn run_one(&self, pid: u32, create: i64) {
        if let Some((id, out)) = self.enforce(pid, create) {
            if out.protected {
                self.guards.lock().unwrap().rules.entry(id).or_default().settle((pid, create));
            }
            self.record(id, pid, &out);
        }
    }

    fn is_settled(&self, id: u32, who: (u32, i64)) -> bool {
        self.guards.lock().unwrap().rules.get(&id).is_some_and(|g| g.is_settled(who))
    }

    /// The worker's clock: run every recovery that is due. Called after EVERY job and every
    /// timeout, so a continuous stream of jobs cannot starve a recovery.
    pub(super) fn run_due(&self, now: u64) {
        // Query only settled identities; unreadable processes remain settled. Never retry a
        // refusal merely because a fixed-size cache filled up, and remove proven exits.
        let check: HashSet<_> = {
            let mut r = self.guards.lock().unwrap();
            if now.saturating_sub(r.pruned_at) >= IDLE_WAIT_MS {
                r.pruned_at = now;
                r.rules.values().flat_map(|g| g.settled.iter().copied()).collect()
            } else {
                HashSet::new()
            }
        };
        let gone: HashSet<_> = check.into_iter().filter(|&(pid, create)| matches!(osutil::open_verified(pid, create, PROCESS_QUERY_LIMITED_INFORMATION), Err(OpenFail::Gone))).collect();
        if !gone.is_empty() {
            self.guards.lock().unwrap().rules.values_mut().for_each(|g| g.settled.retain(|w| !gone.contains(w)));
        }
        let (all, ids) = {
            let mut r = self.guards.lock().unwrap();
            let all = r.overflow_at.is_some_and(|t| t <= now);
            if all {
                r.overflow_at = None;
            }
            let ids: Vec<u32> = r.rules.iter_mut().filter_map(|(id, g)| g.take_due(now).then_some(*id)).collect();
            (all, ids)
        };
        if all {
            // covers every rule, survivors of a pause included
            self.run_job(Job::Sweep(None));
            return;
        }
        for id in ids {
            let alive = self.inner.lock().unwrap().rules.iter().any(|r| r.id == id && r.blocks());
            if alive {
                // verified handle, protections, precedence and the rate gate all run again
                self.run_job(Job::Sweep(Some(id)));
            } else {
                self.forget(id);
            }
        }
    }

    /// When the worker next has to wake (monotonic ms), if anything is pending.
    pub(super) fn next_deadline(&self) -> Option<u64> {
        let r = self.guards.lock().unwrap();
        r.rules.values().filter_map(Guard::recover_at).chain(r.overflow_at).min()
    }

    /// Drop a rule's guard: its pause, deadline and settled list (rule edited, switched, removed).
    pub(super) fn forget(&self, id: u32) {
        self.guards.lock().unwrap().rules.remove(&id);
    }

    /// Keep guards only for rules that exist and block. Called with the rules lock held
    /// (lock order: rules, then guards; nothing takes them the other way round).
    pub(super) fn prune_guards(&self, g: &Inner) {
        let mut r = self.guards.lock().unwrap();
        r.rules.retain(|id, _| g.rules.iter().any(|x| x.id == *id && x.blocks()));
        if !g.any_blocking() {
            r.overflow_at = None;
        }
    }

    fn may_log(&self, id: u32, now: u64) -> bool {
        self.guards.lock().unwrap().rules.entry(id).or_default().may_log(now)
    }

    /// End `pid` (identified by `create`) if an enabled Disallow rule matches its live
    /// image and every safeguard passes. `None`: nothing to say (not matched, gone,
    /// rule off, paused storm). Otherwise the rule id and what happened.
    pub(super) fn enforce(&self, pid: u32, create: i64) -> Option<(u32, Outcome)> {
        self.enforce_with(pid, create, &|_| {})
    }

    pub(super) fn enforce_with(&self, pid: u32, create: i64, hook: &dyn Fn(Stage)) -> Option<(u32, Outcome)> {
        // 1+2: verified handle, image from the handle
        let q = osutil::open_verified(pid, create, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE).ok()?;
        let path = osutil::image_of_handle(q.0)?;
        let name = osutil::file_name(&path).to_string();
        // 3: the engine's own precedence, on the live image
        let id = self.inner.lock().unwrap().blocking(&name, &path)?;
        let fail = |text: String, protected: bool| Some((id, Outcome { ok: false, protected, gone: false, text }));
        // 4: safeguards (no override)
        if let Err(why) = protect::check(q.0, pid, Some(&path)) {
            return fail(format!("{name}: {}", why.text()), true);
        }
        // 5: bounded attempts per rule; a pause also arms the recovery sweep
        let now = now_ms();
        let gate = self.guards.lock().unwrap().rules.entry(id).or_default().gate(now);
        match gate {
            Gate::Go => {}
            Gate::PauseStarts => {
                if self.may_log(id, now) {
                    osutil::log(&format!("disallow: rule {id} ({name}) restarts too fast; pausing {} s, survivors are swept afterwards", STORM_PAUSE_MS / 1000));
                }
                return fail(format!("paused {} s: {STORM_MAX}+ starts in {} s (the program keeps restarting); copies started meanwhile are ended by a sweep right after the pause", STORM_PAUSE_MS / 1000, STORM_WINDOW_MS / 1000), false);
            }
            Gate::Paused => return None,
        }
        // 6: terminate through a second verified handle
        let t = match osutil::open_verified(pid, create, PROCESS_TERMINATE | PROCESS_SYNCHRONIZE) {
            Ok(h) => h,
            Err(f) => {
                let out = open_failure(&f, osutil::is_elevated())?;
                if self.may_log(id, now) {
                    osutil::log(&format!("disallow: {name} pid {pid}: {}", out.text));
                }
                return Some((id, Outcome { text: format!("{name}: {}", out.text), ..out }));
            }
        };
        hook(Stage::BeforeFinalCheck);
        // The final enabled/match check and the TerminateProcess call happen under ONE hold of
        // the rules lock, so a rule switched off or removed cannot slip in between them: once a
        // disable returns, no later termination starts. Only the 1.5 s wait is outside the lock.
        let ended = {
            let g = self.inner.lock().unwrap();
            if g.blocking(&name, &path) != Some(id) {
                return None; // turned off, removed or overridden since: nothing is ended
            }
            hook(Stage::InsideLock);
            unsafe { TerminateProcess(t.0, 1) }
        };
        let out = match ended {
            Ok(()) => {
                if unsafe { WaitForSingleObject(t.0, 1500) } == WAIT_OBJECT_0 {
                    Outcome { ok: true, protected: false, gone: false, text: format!("ended {name} (PID {pid})") }
                } else {
                    Outcome { ok: false, protected: false, gone: false, text: format!("asked Windows to end {name} (PID {pid}) but it was still running after 1.5 s") }
                }
            }
            Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                Outcome { ok: false, protected: true, gone: false, text: format!("{name}: {}", denied_text(osutil::is_elevated())) }
            }
            Err(e) => Outcome { ok: false, protected: false, gone: false, text: format!("{name}: could not end PID {pid}: {}", e.message()) },
        };
        if self.may_log(id, now) {
            osutil::log(&format!("disallow: rule {id}: {}", out.text));
        }
        Some((id, out))
    }

    // ---- editing --------------------------------------------------------------

    /// What exists for `pattern` before a save (the panel shows it, the engine enforces it).
    pub fn disallow_context(&self, pattern: &str) -> DisallowContext {
        let g = self.inner.lock().unwrap();
        match g.rules.iter().find(|r| r.pattern.eq_ignore_ascii_case(pattern.trim())) {
            Some(r) => DisallowContext { id: Some(r.id), enabled: r.enabled, flag: r.actions.disallow, other: if has_other(&r.actions) { Actions { disallow: false, ..r.actions.clone() }.summary() } else { String::new() } },
            None => DisallowContext::default(),
        }
    }

    /// Save a Disallow rule for exactly this exe name or full path (not a pattern),
    /// creating the rule or setting the flag on the existing rule with that pattern.
    ///
    /// Never changes what an existing rule does without being told so:
    /// - `KeepOff` on an existing DISABLED rule stores the flag and leaves it disabled; on an
    ///   existing ENABLED rule it refuses and changes nothing (that rule's priority, affinity
    ///   etc. keep applying; a dormant flag cannot live in an enabled rule).
    /// - `Enable` on an existing rule that is off, or that has other settings (which stop being
    ///   applied while Disallow is on: block wins, they stay saved), needs `confirm_whole_rule`.
    pub fn set_disallow(&self, pattern: &str, mode: DisallowMode) -> Result<DisallowSaved, String> {
        let pattern = pattern.trim();
        if pattern.is_empty() || super::model::kind_of(pattern) == MatchKind::Glob {
            return Err("Disallow needs an exact program name or full path, not a pattern with * or ?.".into());
        }
        if super::model::kind_of(pattern) == MatchKind::Path && !is_full_path(pattern) {
            return Err("A path rule needs a full path like C:\\Games\\game.exe.".into());
        }
        if let Some(why) = protect::pattern_refusal(pattern) {
            return Err(format!("{pattern}: {}", why.text()));
        }
        let enable = matches!(mode, DisallowMode::Enable { .. });
        if enable {
            if let Some(why) = self.worker_problem() {
                return Err(format!("Disallow cannot run: {why}"));
            }
        }
        let saved = {
            let mut g = self.inner.lock().unwrap();
            let saved = match g.rules.iter().position(|r| r.pattern.eq_ignore_ascii_case(pattern)) {
                Some(i) => {
                    let (was_enabled, flag, other) = (g.rules[i].enabled, g.rules[i].actions.disallow, has_other(&g.rules[i].actions));
                    match mode {
                        DisallowMode::KeepOff if was_enabled => {
                            return Err(if flag {
                                format!("{pattern} already has an enabled rule with Disallow: it is ON. Switch it off in Rules to keep it off.")
                            } else {
                                format!("{pattern} already has an enabled rule ({}). A saved-but-off Disallow cannot live inside an enabled rule, so nothing was changed and that rule keeps applying. Turn the rule off in Rules first, or choose Disallow to turn Disallow on.", g.rules[i].actions.summary())
                            });
                        }
                        // turning Disallow on changes what this rule does when it has other settings
                        // (they stop being applied) or is off (the whole rule goes on): say so first
                        DisallowMode::Enable { confirm_whole_rule: false } if other && !(was_enabled && flag) => {
                            return Err(format!("{pattern} already has a rule with other settings ({}){}. Turning Disallow on turns the WHOLE rule on and stops those settings from being applied while Disallow is on (they stay saved). Confirm that first.", Actions { disallow: false, ..g.rules[i].actions.clone() }.summary(), if was_enabled { "" } else { ", currently OFF" }));
                        }
                        _ => {}
                    }
                    g.rules[i].actions.disallow = true;
                    if enable {
                        g.rules[i].enabled = true;
                    }
                    DisallowSaved { id: g.rules[i].id, created: false, was_enabled }
                }
                None => {
                    let mut r = Rule::new(pattern, Actions { disallow: true, ..Default::default() });
                    r.enabled = enable;
                    r.id = g.next_id;
                    g.next_id += 1;
                    let id = r.id;
                    g.rules.push(r);
                    DisallowSaved { id, created: true, was_enabled: false }
                }
            };
            g.reindex();
            self.forget(saved.id);
            self.save_locked(&mut g);
            saved
        };
        if enable {
            self.submit(Job::Sweep(Some(saved.id)));
        }
        Ok(saved)
    }

    /// Drop Disallow from a rule (the rule goes too when nothing else is left in it).
    /// Immediate: nothing more is ended. `restore_enabled`: the rule's enabled switch before
    /// Disallow was saved (from [`DisallowSaved`]); applied when other settings remain, so an
    /// undo never quietly turns a previously OFF rule's other settings on. Returns false when
    /// the rule is unknown.
    pub fn clear_disallow(&self, id: u32, restore_enabled: Option<bool>) -> bool {
        let mut g = self.inner.lock().unwrap();
        let Some(i) = g.rules.iter().position(|r| r.id == id) else { return false };
        g.rules[i].actions.disallow = false;
        if g.rules[i].actions.is_empty() {
            g.rules.remove(i);
        } else if let Some(e) = restore_enabled {
            g.rules[i].enabled = e;
        }
        g.reindex();
        self.forget(id);
        self.save_locked(&mut g);
        true
    }

    /// Write the rules file again, whatever the current state (a failed or unconfirmed write
    /// is retried by a real write, not by re-sending the same in-memory change).
    pub fn save_again(&self) -> SaveStatus {
        let mut g = self.inner.lock().unwrap();
        self.save_locked(&mut g);
        g.persistence.clone().expect("save_locked sets the status")
    }

    /// Turn a rule on or off. Off is immediate and ends nothing; on ends matching copies,
    /// so the UI asks first for a Disallow rule.
    pub fn set_enabled(&self, id: u32, on: bool) -> bool {
        match self.get(id) {
            Some(mut r) => {
                r.enabled = on;
                self.update(r)
            }
            None => false,
        }
    }
}

/// `C:\dir\x.exe` or `\\server\share\x.exe`.
pub fn is_full_path(p: &str) -> bool {
    let b = p.as_bytes();
    (b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || p.starts_with("\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_refusals_survive_more_than_256_live_instances() {
        let mut g = Guard::default();
        let live: HashSet<_> = (100..500).map(|pid| (pid, pid as i64)).collect();
        for &who in &live {
            g.settle(who);
        }
        g.keep_settled(&live);
        assert!(live.iter().all(|&who| g.is_settled(who)));
        let remaining: HashSet<_> = live.into_iter().filter(|(pid, _)| pid % 2 == 0).collect();
        g.keep_settled(&remaining);
        assert_eq!(g.settled.len(), remaining.len());
        assert!(!g.is_settled((101, 101)));
        assert!(g.is_settled((100, 100)));
    }

    #[test]
    fn guard_allows_a_normal_restart_rate_and_pauses_a_storm() {
        let mut g = Guard::default();
        // one restart per second for a minute never trips it
        for s in 0..60u64 {
            assert_eq!(g.gate(1_000 + s * 1_000), Gate::Go, "second {s}");
        }
        // a burst does
        let mut g = Guard::default();
        let t = 100_000;
        for i in 0..STORM_MAX as u64 {
            assert_eq!(g.gate(t + i), Gate::Go);
        }
        assert_eq!(g.gate(t + 40), Gate::PauseStarts);
        // while paused: silent, nothing attempted
        assert_eq!(g.gate(t + 5_000), Gate::Paused);
        assert_eq!(g.gate(t + 40 + STORM_PAUSE_MS - 1), Gate::Paused);
        // after the pause it starts over
        assert_eq!(g.gate(t + 40 + STORM_PAUSE_MS), Gate::Go);
    }

    #[test]
    fn guard_logs_at_most_once_per_interval() {
        let mut g = Guard::default();
        assert!(g.may_log(5_000));
        assert!(!g.may_log(5_001));
        assert!(!g.may_log(5_000 + LOG_EVERY_MS - 1));
        assert!(g.may_log(5_000 + LOG_EVERY_MS));
        // a clock at 0 still logs once
        let mut z = Guard::default();
        assert!(z.may_log(0));
        assert!(!z.may_log(1));
    }

    #[test]
    fn denied_is_honest_about_elevation_and_gone_says_nothing() {
        let o = open_failure(&OpenFail::Denied, false).unwrap();
        assert!(!o.ok && o.protected && o.text.contains("not running as administrator"), "{o:?}");
        let o = open_failure(&OpenFail::Denied, true).unwrap();
        assert!(o.text.contains("protected process") && !o.text.contains("not running as administrator"), "{o:?}");
        assert!(open_failure(&OpenFail::Gone, false).is_none());
        assert!(open_failure(&OpenFail::NoIdentity, false).is_none());
        assert!(open_failure(&OpenFail::Other("boom".into()), false).unwrap().text.contains("boom"));
    }

    #[test]
    fn full_paths() {
        assert!(is_full_path(r"C:\Games\g.exe"));
        assert!(is_full_path("c:/games/g.exe"));
        assert!(is_full_path(r"\\srv\share\g.exe"));
        assert!(!is_full_path(r"games\g.exe"));
        assert!(!is_full_path("g.exe"));
        assert!(!is_full_path(r"C:g.exe"));
    }
}
