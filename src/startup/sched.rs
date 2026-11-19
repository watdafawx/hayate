//! Deterministic scheduler for the managed startup list.
//!
//! No threads, no clock, no registry: it is a state machine the runner drives
//! with the current time (milliseconds, monotonic) and an [`Env`]. That keeps
//! every rule below unit-testable without waiting for real time.
//!
//! Per entry, in order: wait for `after` -> delay -> idle gate -> "already
//! running?" check -> launch.
//!
//! * `after` is satisfied when the other entry was STARTED or SKIPPED because its
//!   program was already running. If it FAILED, was CANCELLED or was itself
//!   BLOCKED, this entry is BLOCKED (reported, not started): a dependent never
//!   launches into a world its prerequisite did not set up. (Launchpad starts it
//!   anyway after 300 s; this is a deliberate, documented difference.)
//! * An `after` pointing at a disabled entry is not waited for.
//! * Delay counts from the moment the dependency finished, or from logon
//!   (scheduler start) when there is none.
//! * The idle gate needs three consecutive 1 s samples under the limit; after
//!   90 s it gives up and starts anyway (noted). The sampler is only asked for
//!   a value while some entry is inside the gate.
//! * A program is started at most once per run. While a launch is in flight its
//!   (lower-case) exe name is reserved: another ready entry for the same program
//!   is HELD, and when the launch is reported it re-checks a fresh process
//!   snapshot (and counts the program as running when this run already started
//!   it, even if the snapshot cannot see it yet).
//! * Every wait is bounded (delay <= 1 h, gate <= 90 s, launch <= 120 s), so
//!   the whole run always ends. A launch that does not answer in 120 s is
//!   INDETERMINATE, not failed: Windows may still start it later, so its program
//!   stays reserved (no second copy this run) and anything waiting for it is
//!   blocked. The runner decides what else must wait (see `run`).

use super::model::{validate, App, Config};

pub const IDLE_SAMPLE_MS: u64 = 1000;
pub const IDLE_CALM_SAMPLES: u8 = 3;
pub const IDLE_TIMEOUT_MS: u64 = 90_000;
pub const LAUNCH_TIMEOUT_MS: u64 = 120_000;

#[cfg(test)]
thread_local! {
    /// test-only: shorter launch timeout for the thread that runs the scheduler (0 = the real one)
    pub static LAUNCH_TIMEOUT_TEST: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn launch_timeout() -> u64 {
    #[cfg(test)]
    {
        let t = LAUNCH_TIMEOUT_TEST.with(|c| c.get());
        if t > 0 {
            return t;
        }
    }
    LAUNCH_TIMEOUT_MS
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub delay_ms: u32,
    pub after: Option<String>,
    pub cpu_below: Option<u8>,
    /// lower-case exe name to look for before launching (None = never skip)
    pub process: Option<String>,
    /// never started by an automatic (logon) run: it needs a person to press Run now
    /// (Admin entries). Reported, and anything waiting for it is blocked.
    pub manual: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Started,
    AlreadyRunning(String),
    Failed(String),
    Cancelled,
    Blocked(String),
    /// the launcher did not answer in time: the program may still start later
    Indeterminate(String),
}

impl Outcome {
    /// does this satisfy another entry's `after`?
    pub fn satisfies(&self) -> bool {
        matches!(self, Outcome::Started | Outcome::AlreadyRunning(_))
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Phase {
    Waiting,
    Delay { due: u64 },
    Idle { since: u64, calm: u8 },
    Launching { since: u64 },
    /// ready, but another entry for the same program is being launched
    Held,
    Done(Outcome),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// the runner must start item `i` now, then call [`Sched::launched`]
    Launch(usize),
    Note(usize, String),
    Finished(usize, Outcome),
}

/// What the scheduler may ask the machine.
pub trait Env {
    /// Total CPU use in percent from a reading newer than the last one handed
    /// out; None = no new reading (unknown, or the sampler has not ticked yet).
    fn cpu_percent(&mut self) -> Option<f32>;
    fn is_running(&mut self, process: &str) -> bool;
}

pub struct Sched {
    items: Vec<Item>,
    phase: Vec<Phase>,
    done_at: Vec<u64>,
    t0: u64,
    last_sample: Option<u64>,
    cancelled: bool,
    /// programs this run handed to the launcher successfully (lower-case)
    started: std::collections::HashSet<String>,
    /// programs whose launch never answered: they may still appear
    unresolved: std::collections::HashSet<String>,
}

/// The items of a run, in list order: enabled entries only. `Err` = the list is
/// not valid (nothing runs on a broken list).
/// `automatic` = a logon run: Admin entries become `manual` (see [`Item::manual`]).
pub fn plan(cfg: &Config, automatic: bool, process_of: &dyn Fn(&App) -> Option<String>) -> Result<Vec<Item>, Vec<String>> {
    let problems = validate(cfg);
    if !problems.is_empty() {
        return Err(problems);
    }
    // an entry whose take-over was never confirmed still has its original enabled: not runnable
    let on: Vec<&App> = cfg.apps.iter().filter(|a| a.enabled && !a.command.trim().is_empty() && !a.own.as_ref().is_some_and(|o| o.state == super::model::OwnState::Pending)).collect();
    Ok(on
        .iter()
        .map(|a| Item {
            id: a.id.clone(),
            name: a.name.clone(),
            delay_ms: a.delay_ms,
            after: a.after.clone().filter(|d| on.iter().any(|o| &o.id == d)),
            cpu_below: a.cpu_below,
            process: process_of(a),
            manual: automatic && a.admin,
        })
        .collect())
}

impl Sched {
    pub fn new(items: Vec<Item>, now: u64) -> Sched {
        let n = items.len();
        Sched { items, phase: vec![Phase::Waiting; n], done_at: vec![0; n], t0: now, last_sample: None, cancelled: false, started: Default::default(), unresolved: Default::default() }
    }

    pub fn outcome(&self, i: usize) -> Option<&Outcome> {
        match self.phase.get(i)? {
            Phase::Done(o) => Some(o),
            _ => None,
        }
    }

    pub fn finished(&self) -> bool {
        self.phase.iter().all(|p| matches!(p, Phase::Done(_)))
    }

    /// Words for a status column while a run is in progress.
    pub fn status(&self, i: usize) -> String {
        match self.phase.get(i) {
            Some(Phase::Waiting) => "Waiting".into(),
            Some(Phase::Delay { .. }) => "Delay".into(),
            Some(Phase::Idle { .. }) => "Waiting for idle CPU".into(),
            Some(Phase::Launching { .. }) => "Starting".into(),
            Some(Phase::Held) => "Waiting for the same program to finish starting".into(),
            Some(Phase::Done(o)) => outcome_text(o),
            None => String::new(),
        }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|x| x.id == id)
    }

    /// Stop everything not yet started. Entries already handed to the launcher
    /// finish on their own; their dependents become Blocked.
    pub fn cancel(&mut self, now: u64) -> Vec<Event> {
        self.cancelled = true;
        let mut ev = Vec::new();
        for i in 0..self.items.len() {
            if matches!(self.phase[i], Phase::Waiting | Phase::Delay { .. } | Phase::Idle { .. } | Phase::Held) {
                self.finish(i, Outcome::Cancelled, now, &mut ev);
            }
        }
        ev
    }

    fn finish(&mut self, i: usize, o: Outcome, now: u64, ev: &mut Vec<Event>) {
        self.phase[i] = Phase::Done(o.clone());
        self.done_at[i] = now;
        ev.push(Event::Finished(i, o));
    }

    /// The runner reports how launching item `i` went.
    pub fn launched(&mut self, i: usize, r: Result<(), String>, now: u64) -> Vec<Event> {
        let mut ev = Vec::new();
        let late = matches!(self.phase.get(i), Some(Phase::Done(Outcome::Indeterminate(_))));
        if matches!(self.phase.get(i), Some(Phase::Launching { .. })) || late {
            let p = self.proc_of(i);
            if late {
                // the answer came after the timeout: the reservation is over and the row shows the truth
                if let Some(p) = &p {
                    self.unresolved.remove(p);
                }
            }
            let o = match r {
                Ok(()) => {
                    if let Some(p) = p {
                        self.started.insert(p);
                    }
                    Outcome::Started
                }
                Err(e) => Outcome::Failed(e),
            };
            self.finish(i, o, now, &mut ev);
        }
        ev
    }

    /// The launch gate in the worker saw the run cancelled before it asked
    /// Windows to start anything: nothing was started.
    pub fn launch_cancelled(&mut self, i: usize, now: u64) -> Vec<Event> {
        let mut ev = Vec::new();
        if matches!(self.phase.get(i), Some(Phase::Launching { .. })) {
            self.finish(i, Outcome::Cancelled, now, &mut ev);
        }
        ev
    }

    fn proc_of(&self, i: usize) -> Option<String> {
        self.items.get(i).and_then(|x| x.process.as_ref()).map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty())
    }

    fn launching_same(&self, i: usize, p: &str) -> bool {
        (0..self.items.len()).any(|j| j != i && matches!(self.phase[j], Phase::Launching { .. }) && self.proc_of(j).as_deref() == Some(p))
    }

    /// Earliest time `tick` has something to do (None = nothing timed pending).
    pub fn next_wake(&self) -> Option<u64> {
        let mut w: Option<u64> = None;
        let mut min = |t: u64| w = Some(w.map_or(t, |x| x.min(t)));
        let mut idle = false;
        for p in &self.phase {
            match p {
                Phase::Delay { due } => min(*due),
                Phase::Idle { since, .. } => {
                    idle = true;
                    min(since + IDLE_TIMEOUT_MS);
                }
                Phase::Launching { since } => min(since + launch_timeout()),
                _ => {}
            }
        }
        if idle {
            min(self.last_sample.map_or(0, |t| t + IDLE_SAMPLE_MS));
        }
        w
    }

    pub fn tick(&mut self, now: u64, env: &mut dyn Env) -> Vec<Event> {
        let mut ev = Vec::new();
        let mut sampled = false;
        loop {
            let mut changed = false;
            for i in 0..self.items.len() {
                match self.phase[i].clone() {
                    Phase::Waiting => {
                        if self.cancelled {
                            self.finish(i, Outcome::Cancelled, now, &mut ev);
                            changed = true;
                            continue;
                        }
                        let (base, block) = match self.items[i].after.clone().and_then(|d| self.index_of(&d)) {
                            None => (Some(self.t0), None),
                            Some(d) => match &self.phase[d] {
                                Phase::Done(o) if o.satisfies() => (Some(self.done_at[d]), None),
                                Phase::Done(o) => (None, Some(format!("{} {}", self.items[d].name, blocked_why(o)))),
                                _ => (None, None),
                            },
                        };
                        if self.items[i].manual && block.is_none() {
                            self.finish(i, Outcome::Blocked("it runs as administrator, so it only starts when you press Run now".into()), now, &mut ev);
                            changed = true;
                        } else if let Some(why) = block {
                            self.finish(i, Outcome::Blocked(why), now, &mut ev);
                            changed = true;
                        } else if let Some(b) = base {
                            self.phase[i] = Phase::Delay { due: b + self.items[i].delay_ms as u64 };
                            changed = true;
                        }
                    }
                    Phase::Delay { due } if now >= due => {
                        if self.items[i].cpu_below.is_some() {
                            self.phase[i] = Phase::Idle { since: now, calm: 0 };
                        } else {
                            self.go(i, now, env, &mut ev);
                        }
                        changed = true;
                    }
                    Phase::Idle { since, calm } => {
                        // one shared sample per interval, taken only because someone is in the gate
                        if !sampled && self.last_sample.is_none_or(|t| now >= t + IDLE_SAMPLE_MS) {
                            sampled = true;
                            self.last_sample = Some(now);
                            // None = no fresh reading (sampler has not published a new one): nothing counts either way
                            if let Some(p) = env.cpu_percent() {
                                for j in 0..self.items.len() {
                                    if let (Phase::Idle { since: s, calm: c }, Some(limit)) = (self.phase[j].clone(), self.items[j].cpu_below) {
                                        // a sample taken before the entry joined the gate does not count
                                        let calm_now = s < now && p < limit as f32;
                                        self.phase[j] = Phase::Idle { since: s, calm: if calm_now { c + 1 } else { 0 } };
                                    }
                                }
                            }
                            changed = true;
                            continue;
                        }
                        if calm >= IDLE_CALM_SAMPLES {
                            self.go(i, now, env, &mut ev);
                            changed = true;
                        } else if now >= since + IDLE_TIMEOUT_MS {
                            ev.push(Event::Note(i, format!("CPU never stayed under {}% for {} s, starting anyway", self.items[i].cpu_below.unwrap_or(0), IDLE_TIMEOUT_MS / 1000)));
                            self.go(i, now, env, &mut ev);
                            changed = true;
                        }
                    }
                    Phase::Held => {
                        if self.cancelled {
                            self.finish(i, Outcome::Cancelled, now, &mut ev);
                            changed = true;
                        } else if self.proc_of(i).is_none_or(|p| !self.launching_same(i, &p)) {
                            self.go(i, now, env, &mut ev);
                            changed = true;
                        }
                    }
                    Phase::Launching { since } if now >= since + launch_timeout() => {
                        if let Some(p) = self.proc_of(i) {
                            self.unresolved.insert(p);
                        }
                        self.finish(i, Outcome::Indeterminate("the launcher did not answer in 120 s; the program may still start".into()), now, &mut ev);
                        changed = true;
                    }
                    _ => {}
                }
            }
            if !changed {
                break;
            }
        }
        ev
    }

    /// Gates passed: skip when the program already runs, else hand to the runner.
    fn go(&mut self, i: usize, now: u64, env: &mut dyn Env, ev: &mut Vec<Event>) {
        if self.cancelled {
            self.finish(i, Outcome::Cancelled, now, ev);
            return;
        }
        if let Some(p) = self.proc_of(i) {
            if self.unresolved.contains(&p) {
                self.finish(i, Outcome::Blocked(format!("{p} may still be starting from an earlier launch that did not answer; Hayate will not start a second copy")), now, ev);
                return;
            }
            if self.launching_same(i, &p) {
                self.phase[i] = Phase::Held;
                return;
            }
            // this run already started it (a shell start is asynchronous and a snapshot may not see it yet), or a fresh snapshot sees it
            if self.started.contains(&p) || env.is_running(&p) {
                self.finish(i, Outcome::AlreadyRunning(p), now, ev);
                return;
            }
        }
        self.phase[i] = Phase::Launching { since: now };
        ev.push(Event::Launch(i));
    }
}

fn blocked_why(o: &Outcome) -> &'static str {
    match o {
        Outcome::Failed(_) => "failed to start",
        Outcome::Cancelled => "was cancelled",
        Outcome::Indeterminate(_) => "did not answer, so it may or may not have started",
        _ => "was blocked",
    }
}

pub fn outcome_text(o: &Outcome) -> String {
    match o {
        Outcome::Started => "Started".into(),
        Outcome::AlreadyRunning(p) => format!("Skipped: {p} already running"),
        Outcome::Failed(e) => format!("Failed: {e}"),
        Outcome::Cancelled => "Cancelled".into(),
        Outcome::Blocked(w) => format!("Not started: {w}"),
        Outcome::Indeterminate(w) => format!("Unconfirmed: {w}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[derive(Default)]
    struct Fake {
        cpu: Vec<Option<f32>>,
        cpu_calls: usize,
        running: HashSet<String>,
    }
    impl Env for Fake {
        fn cpu_percent(&mut self) -> Option<f32> {
            self.cpu_calls += 1;
            if self.cpu.is_empty() {
                Some(0.0)
            } else {
                self.cpu.remove(0)
            }
        }
        fn is_running(&mut self, p: &str) -> bool {
            self.running.contains(p)
        }
    }

    fn item(id: &str) -> Item {
        Item { id: id.into(), name: id.to_uppercase(), delay_ms: 0, after: None, cpu_below: None, process: None, manual: false }
    }

    fn launches(ev: &[Event]) -> Vec<usize> {
        ev.iter().filter_map(|e| if let Event::Launch(i) = e { Some(*i) } else { None }).collect()
    }

    #[test]
    fn two_ready_entries_for_the_same_program_start_it_once() {
        let mut a = item("a");
        a.process = Some("same.exe".into());
        let mut b = item("b");
        b.process = Some("SAME.exe".into()); // different spelling, same program
        let mut s = Sched::new(vec![a, b], 0);
        let mut env = Fake::default();
        assert_eq!(launches(&s.tick(0, &mut env)), vec![0], "the second waits for the first launch instead of racing it");
        // the launch finished but the process is not visible to a snapshot yet (a shell start is asynchronous)
        s.launched(0, Ok(()), 10);
        let ev = s.tick(10, &mut env);
        assert!(launches(&ev).is_empty(), "no second copy: {ev:?}");
        assert_eq!(s.outcome(1), Some(&Outcome::AlreadyRunning("same.exe".into())));
    }

    #[test]
    fn a_failed_first_launch_lets_the_second_entry_for_the_program_try() {
        let mut a = item("a");
        a.process = Some("same.exe".into());
        let mut b = item("b");
        b.process = Some("same.exe".into());
        let mut s = Sched::new(vec![a, b], 0);
        let mut env = Fake::default();
        assert_eq!(launches(&s.tick(0, &mut env)), vec![0]);
        s.launched(0, Err("no".into()), 10);
        assert_eq!(launches(&s.tick(10, &mut env)), vec![1], "nothing was started, so a fresh check allows the second");
    }

    #[test]
    fn a_program_that_appeared_while_the_first_launch_ran_is_not_started_again() {
        let mut a = item("a");
        a.process = Some("same.exe".into());
        let mut b = item("b");
        b.process = Some("same.exe".into());
        let mut s = Sched::new(vec![a, b], 0);
        let mut env = Fake::default();
        assert_eq!(launches(&s.tick(0, &mut env)), vec![0]);
        s.launched(0, Err("odd".into()), 10);
        env.running.insert("same.exe".into()); // started by something else meanwhile
        assert!(launches(&s.tick(10, &mut env)).is_empty());
        assert_eq!(s.outcome(1), Some(&Outcome::AlreadyRunning("same.exe".into())));
    }

    #[test]
    fn a_launch_that_never_answers_is_indeterminate_and_keeps_its_program_reserved() {
        let mut a = item("a");
        a.process = Some("slow.exe".into());
        let mut b = item("b");
        b.process = Some("slow.exe".into());
        let mut c = item("c");
        c.after = Some("a".into());
        let mut s = Sched::new(vec![a, b, c], 0);
        let mut env = Fake::default();
        assert_eq!(launches(&s.tick(0, &mut env)), vec![0]);
        assert_eq!(s.status(1), "Waiting for the same program to finish starting");
        let ev = s.tick(LAUNCH_TIMEOUT_MS, &mut env);
        assert!(launches(&ev).is_empty(), "no second copy while the first may still start: {ev:?}");
        assert!(matches!(s.outcome(0), Some(Outcome::Indeterminate(_))), "not Failed: {:?}", s.outcome(0));
        assert!(matches!(s.outcome(1), Some(Outcome::Blocked(w)) if w.contains("may still be starting")), "{:?}", s.outcome(1));
        assert!(matches!(s.outcome(2), Some(Outcome::Blocked(_))), "an unconfirmed prerequisite does not satisfy `after`");
        assert!(s.finished(), "the run still ends: every wait is bounded");
        // the late answer is the truth
        s.launched(0, Ok(()), LAUNCH_TIMEOUT_MS + 5);
        assert_eq!(s.outcome(0), Some(&Outcome::Started));
    }

    #[test]
    fn program_names_are_matched_case_insensitively() {
        let mut a = item("a");
        a.process = Some("App.EXE".into());
        let mut env = Fake::default();
        env.running.insert("app.exe".into());
        let mut s = Sched::new(vec![a], 0);
        s.tick(0, &mut env);
        assert!(matches!(s.outcome(0), Some(Outcome::AlreadyRunning(_))));
    }

    #[test]
    fn a_launch_cancelled_at_the_gate_is_cancelled_not_started() {
        let mut s = Sched::new(vec![item("a")], 0);
        let mut env = Fake::default();
        assert_eq!(launches(&s.tick(0, &mut env)), vec![0]);
        s.launch_cancelled(0, 1);
        assert_eq!(s.outcome(0), Some(&Outcome::Cancelled));
    }

    #[test]
    fn independent_items_launch_in_list_order_immediately() {
        let mut s = Sched::new(vec![item("a"), item("b"), item("c")], 1000);
        let mut env = Fake::default();
        assert_eq!(launches(&s.tick(1000, &mut env)), vec![0, 1, 2]);
        assert_eq!(env.cpu_calls, 0, "no idle gate anywhere: the sampler is never asked");
        s.launched(0, Ok(()), 1000);
        s.launched(1, Ok(()), 1000);
        assert!(!s.finished());
        s.launched(2, Ok(()), 1000);
        assert!(s.finished());
        assert_eq!(s.next_wake(), None);
    }

    #[test]
    fn delay_counts_from_logon_or_from_the_dependency() {
        let mut a = item("a");
        a.delay_ms = 2000;
        let mut b = item("b");
        b.after = Some("a".into());
        b.delay_ms = 3000;
        let mut c = item("c");
        c.delay_ms = 5000; // from logon
        let mut s = Sched::new(vec![a, b, c], 10_000);
        let mut env = Fake::default();
        assert!(launches(&s.tick(10_000, &mut env)).is_empty());
        assert_eq!(s.next_wake(), Some(12_000));
        assert_eq!(launches(&s.tick(12_000, &mut env)), vec![0]);
        // a's launch call returns late, at 12_500: b's delay counts from THAT moment
        s.launched(0, Ok(()), 12_500);
        assert!(launches(&s.tick(12_500, &mut env)).is_empty());
        assert_eq!(s.next_wake(), Some(15_000), "c is due at logon+5 s, b at 12.5 s + 3 s = 15.5 s");
        assert_eq!(launches(&s.tick(15_000, &mut env)), vec![2]);
        assert_eq!(launches(&s.tick(15_500, &mut env)), vec![1]);
    }

    #[test]
    fn after_is_satisfied_by_skipped_already_running_but_not_by_failure() {
        let mut a = item("a");
        a.process = Some("a.exe".into());
        let mut b = item("b");
        b.after = Some("a".into());
        let mut env = Fake::default();
        env.running.insert("a.exe".into());
        let mut s = Sched::new(vec![a.clone(), b.clone()], 0);
        let ev = s.tick(0, &mut env);
        assert_eq!(s.outcome(0), Some(&Outcome::AlreadyRunning("a.exe".into())));
        assert_eq!(launches(&ev), vec![1], "skipped counts as done: b goes");

        // a fails -> b is blocked, never launched
        let mut s = Sched::new(vec![item("a"), b.clone()], 0);
        let mut env = Fake::default();
        assert_eq!(launches(&s.tick(0, &mut env)), vec![0]);
        let ev = s.launched(0, Err("no such file".into()), 5);
        assert!(matches!(ev[0], Event::Finished(0, Outcome::Failed(_))));
        let ev = s.tick(5, &mut env);
        assert!(launches(&ev).is_empty());
        assert!(matches!(s.outcome(1), Some(Outcome::Blocked(w)) if w.contains("failed to start")));
        assert!(s.finished());
    }

    #[test]
    fn blocking_propagates_down_a_chain() {
        let mut b = item("b");
        b.after = Some("a".into());
        let mut c = item("c");
        c.after = Some("b".into());
        let mut s = Sched::new(vec![item("a"), b, c], 0);
        let mut env = Fake::default();
        s.tick(0, &mut env);
        s.launched(0, Err("x".into()), 1);
        s.tick(1, &mut env);
        assert!(matches!(s.outcome(2), Some(Outcome::Blocked(_))));
    }

    #[test]
    fn cancel_stops_the_unstarted_and_blocks_their_dependents() {
        let mut a = item("a");
        a.delay_ms = 60_000;
        let mut b = item("b");
        b.after = Some("a".into());
        let mut s = Sched::new(vec![a, b], 0);
        let mut env = Fake::default();
        s.tick(0, &mut env);
        let ev = s.cancel(10);
        assert!(ev.contains(&Event::Finished(0, Outcome::Cancelled)));
        assert!(ev.contains(&Event::Finished(1, Outcome::Cancelled)));
        assert!(s.tick(70_000, &mut env).is_empty(), "nothing launches after cancel");
        assert!(s.finished());
    }

    #[test]
    fn an_entry_already_launching_survives_cancel() {
        let mut b = item("b");
        b.after = Some("a".into());
        let mut s = Sched::new(vec![item("a"), b], 0);
        let mut env = Fake::default();
        s.tick(0, &mut env); // a Launching
        s.cancel(1);
        assert!(matches!(s.outcome(1), Some(Outcome::Cancelled)));
        assert!(s.launched(0, Ok(()), 2).iter().any(|e| matches!(e, Event::Finished(0, Outcome::Started))));
    }

    #[test]
    fn idle_gate_needs_three_calm_samples_in_a_row() {
        let mut a = item("a");
        a.cpu_below = Some(20);
        let mut s = Sched::new(vec![a], 0);
        // 1st sample is taken the moment the gate opens and never counts; then
        // calm, BUSY (resets), calm, calm, calm
        let mut env = Fake { cpu: vec![Some(5.0), Some(5.0), Some(70.0), Some(5.0), Some(5.0), Some(5.0)], ..Fake::default() };
        let mut t = 0;
        let mut started_at = None;
        for _ in 0..20 {
            if !launches(&s.tick(t, &mut env)).is_empty() {
                started_at = Some(t);
                break;
            }
            t += 500;
        }
        assert_eq!(started_at, Some(5000), "samples at 0 (ignored), 1 s, 2 s, 3 s (busy), 4 s, 5 s");
        assert_eq!(env.cpu_calls, 6);
    }

    #[test]
    fn idle_gate_gives_up_after_90_seconds_and_says_so() {
        let mut a = item("a");
        a.cpu_below = Some(10);
        let mut s = Sched::new(vec![a], 0);
        let mut env = Fake { cpu: vec![Some(95.0); 200], ..Fake::default() };
        let mut t = 0;
        let mut ev_all = Vec::new();
        while t <= 91_000 {
            ev_all.extend(s.tick(t, &mut env));
            if s.outcome(0).is_some() || ev_all.iter().any(|e| matches!(e, Event::Launch(_))) {
                break;
            }
            t += 1000;
        }
        assert!(ev_all.iter().any(|e| matches!(e, Event::Note(0, n) if n.contains("starting anyway"))), "{ev_all:?}");
        assert_eq!(launches(&ev_all), vec![0]);
        assert!((90_000..=91_000).contains(&t), "t={t}");
    }

    #[test]
    fn unknown_cpu_is_never_treated_as_calm() {
        let mut a = item("a");
        a.cpu_below = Some(50);
        let mut s = Sched::new(vec![a], 0);
        let mut env = Fake { cpu: vec![None; 200], ..Fake::default() };
        let mut t = 0;
        while t < 89_000 {
            assert!(launches(&s.tick(t, &mut env)).is_empty(), "t={t}");
            t += 1000;
        }
    }

    #[test]
    fn the_sampler_runs_only_while_someone_is_in_the_gate() {
        let mut a = item("a");
        a.cpu_below = Some(50);
        let mut b = item("b");
        b.delay_ms = 30_000;
        let mut s = Sched::new(vec![a, b], 0);
        let mut env = Fake::default();
        let mut t = 0;
        while t <= 31_000 && !s.finished() {
            for e in s.tick(t, &mut env) {
                if let Event::Launch(i) = e {
                    s.launched(i, Ok(()), t);
                }
            }
            t += 1000;
        }
        assert!(s.finished());
        let calls = env.cpu_calls;
        s.tick(40_000, &mut env);
        s.tick(50_000, &mut env);
        assert_eq!(env.cpu_calls, calls, "a finished (or gate-free) run never samples");
        assert!(calls <= 5, "gate lasted ~4 s, got {calls} samples");
    }

    #[test]
    fn a_launcher_that_never_answers_is_indeterminate_after_120_seconds() {
        let mut b = item("b");
        b.after = Some("a".into());
        let mut s = Sched::new(vec![item("a"), b], 0);
        let mut env = Fake::default();
        s.tick(0, &mut env);
        assert_eq!(s.next_wake(), Some(LAUNCH_TIMEOUT_MS));
        s.tick(LAUNCH_TIMEOUT_MS, &mut env);
        assert!(matches!(s.outcome(0), Some(Outcome::Indeterminate(_))), "not Failed: the program may still start");
        assert!(matches!(s.outcome(1), Some(Outcome::Blocked(_))));
        // a late answer from the slow launcher is the truth for its own row; the dependent stays blocked
        s.launched(0, Err("denied".into()), LAUNCH_TIMEOUT_MS + 5);
        assert!(matches!(s.outcome(0), Some(Outcome::Failed(_))));
        assert!(matches!(s.outcome(1), Some(Outcome::Blocked(_))));
    }

    #[test]
    fn plan_skips_disabled_entries_and_does_not_wait_for_them() {
        let mut cfg = Config::default();
        let mut a = App::new("a1".into(), "A".into(), "a.exe".into());
        a.enabled = false;
        let mut b = App::new("a2".into(), "B".into(), "b.exe".into());
        b.after = Some("a1".into());
        cfg.apps = vec![a, b];
        let items = plan(&cfg, false, &|_| None).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].after, None, "a disabled prerequisite is not waited for");
    }

    #[test]
    fn plan_refuses_an_invalid_list() {
        let mut cfg = Config::default();
        let mut a = App::new("a1".into(), "A".into(), "a.exe".into());
        a.after = Some("a2".into());
        let mut b = App::new("a2".into(), "B".into(), "b.exe".into());
        b.after = Some("a1".into());
        cfg.apps = vec![a, b];
        assert!(plan(&cfg, false, &|_| None).is_err(), "a cycle runs nothing at all");
    }
}
