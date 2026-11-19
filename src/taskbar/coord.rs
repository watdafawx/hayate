//! The one owner of every change to Hayate's Jump List.
//!
//! Three things can ask for a change at about the same time: the 8 s start
//! sync, the Settings switch, and uninstall. Windows keeps the list outside
//! Hayate, and a COM call into the shell can hang, so the rules are:
//!
//! * **One worker, one operation at a time.** A request (`request`) only sets
//!   `dirty` and makes sure a worker runs. The worker clears `dirty`, reads the
//!   LATEST switch, state file and install location, and does what `logic::plan`
//!   says. A request that arrives meanwhile sets `dirty` again, so the worker
//!   loops once more before it exits: no request is dropped, and the last switch
//!   position always wins (off during the first registration ends removed; on
//!   during a delete ends registered).
//! * **A commit gate.** An operation does all its preparation (`BeginList`,
//!   building the links, `AddUserTasks`) and then asks `gate()` right before
//!   its single persistent step (`CommitList` / `DeleteList`). The gate says yes
//!   only while the operation is still the current, uncancelled one, and then
//!   marks it `Committing`. All of that happens under the coordinator's lock, so
//!   "cancel" and "commit" can never both win.
//! * **Timeout invariant.** The worker waits for an operation for a bounded
//!   time. On timeout it cancels the operation. If the operation has not passed
//!   the gate, it is now unable to change the shell: when the detached thread
//!   ever wakes up, the gate refuses and it aborts. A later operation therefore
//!   cannot be overwritten by a stale one. If the operation is already past the
//!   gate (its commit is inside the shell), it cannot be taken back; the worker
//!   then starts nothing else until that commit returns (its completion
//!   re-wakes the worker), so changes never reorder.
//! * **Final removal (uninstall).** `remove_for_good` sets `closed` under the
//!   same lock. From then on no write can pass the gate, whatever thread it is
//!   on; the worker deletes the list once, after any commit already inside the
//!   shell has returned. Callers wait at most a bounded time (`wait_final`); an
//!   uninstall never hangs the UI, and no thread is ever terminated.
//!
//! The shell side is the `Backend` trait, so the ordering rules are tested with
//! a fake backend whose steps block on barriers.

use super::logic::{eligible, fingerprint, plan, Plan, State, TASKS};
use crate::setup::logic::Where;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Everything a decision reads, read fresh each time.
#[derive(Clone, Debug)]
pub struct Inputs {
    pub at: Where,
    pub exe: String,
    pub aumid: String,
    pub enabled: bool,
    pub multi: bool,
    /// the debug-build disposable test AUMID
    pub test: bool,
}

/// How an operation ended at the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// the change was made
    Persisted,
    /// the gate said no: the shell was left unchanged
    Refused,
}

pub trait Backend: Send + Sync + 'static {
    fn inputs(&self) -> Inputs;
    fn read_state(&self) -> State;
    fn write_state(&self, s: &State);
    /// uninstall: the state file goes with the list
    fn clear_state(&self);
    /// Blocking. Replace the user tasks. MUST call `gate()` right before the one
    /// step that persists them and do nothing persistent when it returns false.
    fn write_list(&self, exe: &str, aumid: &str, gate: &dyn Fn() -> bool) -> Result<Step, String>;
    /// Blocking. Delete the list; same `gate()` rule.
    fn delete_list(&self, aumid: &str, gate: &dyn Fn() -> bool) -> Result<Step, String>;
    fn now(&self) -> u64;
    fn log(&self, line: &str);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// preparing; may still be cancelled
    Running,
    /// past the gate: inside the shell, cannot be taken back
    Committing,
    /// cancelled before the gate: can never change the shell
    Cancelled,
    /// returned (state file already written)
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Write,
    Delete,
}

struct Op {
    id: u64,
    kind: Kind,
    phase: Phase,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Final {
    /// not asked for, or still going
    Pending,
    Removed,
    /// the shell refused or did not answer twice: left as it is
    GaveUp,
}

struct Inner {
    dirty: bool,
    worker: bool,
    /// final removal asked for: no write may pass the gate again
    closed: bool,
    fin: Final,
    fin_tries: u32,
    op: Option<Op>,
    next_id: u64,
    why: &'static str,
}

enum Ran {
    Done,
    Cancelled,
    /// timed out while committing inside the shell
    Stuck,
}

pub struct Coordinator<B: Backend> {
    b: B,
    m: Mutex<Inner>,
    cv: Condvar,
    write_wait: Duration,
    delete_wait: Duration,
}

impl<B: Backend> Coordinator<B> {
    pub fn new(b: B, write_wait: Duration, delete_wait: Duration) -> Arc<Self> {
        Arc::new(Coordinator {
            b,
            m: Mutex::new(Inner { dirty: false, worker: false, closed: false, fin: Final::Pending, fin_tries: 0, op: None, next_id: 0, why: "" }),
            cv: Condvar::new(),
            write_wait,
            delete_wait,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask for a reconcile against the latest settings. Returns at once.
    pub fn request(self: &Arc<Self>, why: &'static str) {
        let spawn = {
            let mut g = self.lock();
            if g.closed && g.fin != Final::Pending {
                return;
            }
            g.dirty = true;
            g.why = why;
            take_worker(&mut g)
        };
        if spawn {
            self.spawn_worker();
        }
    }

    /// Uninstall / `--remove-jumplist`: from now on no write can reach the shell,
    /// and the list is deleted once. False (nothing done) when this copy never
    /// owned the global list (portable copy, `HAYATE_MULTI`).
    pub fn remove_for_good(self: &Arc<Self>, why: &'static str) -> bool {
        let i = self.b.inputs();
        if let Err(r) = eligible(&i.at, i.multi, i.test) {
            self.b.log(&format!("jumplist: {why}: nothing removed ({r})"));
            return false;
        }
        let spawn = {
            let mut g = self.lock();
            g.closed = true;
            g.dirty = true;
            g.why = why;
            let s = take_worker(&mut g);
            self.cv.notify_all();
            s
        };
        if spawn {
            self.spawn_worker();
        }
        true
    }

    /// Wait up to `wait` for the final removal. `Pending` also when none was asked for.
    pub fn wait_final(&self, wait: Duration) -> Final {
        let end = Instant::now() + wait;
        let mut g = self.lock();
        while g.closed && g.fin == Final::Pending {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            g = self.cv.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        g.fin
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn spawn_worker(self: &Arc<Self>) {
        let me = self.clone();
        if std::thread::Builder::new().name("jumplist-worker".into()).spawn(move || me.worker()).is_err() {
            self.lock().worker = false;
        }
    }

    fn worker(self: &Arc<Self>) {
        enum Job {
            Sync(&'static str),
            Final,
        }
        loop {
            let job = {
                let mut g = self.lock();
                // an earlier operation is inside the shell and cannot be taken back:
                // nothing else starts until it returns (its completion wakes us)
                if matches!(&g.op, Some(op) if op.phase == Phase::Committing) {
                    g.worker = false;
                    return;
                }
                if g.closed {
                    if g.fin != Final::Pending {
                        g.worker = false;
                        return;
                    }
                    Job::Final
                } else if g.dirty {
                    g.dirty = false;
                    Job::Sync(g.why)
                } else {
                    g.worker = false;
                    return;
                }
            };
            match job {
                Job::Sync(why) => self.sync(why),
                Job::Final => {
                    let i = self.b.inputs();
                    let r = self.run_op(Kind::Delete, true, &i, String::new(), "uninstall");
                    let gave_up = {
                        let mut g = self.lock();
                        if matches!(r, Ran::Stuck) {
                            // the commit is still inside the shell: the final delete waits for it
                            None
                        } else if g.fin == Final::Pending {
                            g.fin_tries += 1;
                            if g.fin_tries >= 2 {
                                g.fin = Final::GaveUp;
                                self.cv.notify_all();
                                Some(true)
                            } else {
                                Some(false)
                            }
                        } else {
                            Some(false)
                        }
                    };
                    if gave_up == Some(true) {
                        self.b.log("jumplist: uninstall: could not remove the list; given up");
                    }
                }
            }
        }
    }

    fn sync(self: &Arc<Self>, why: &'static str) {
        let i = self.b.inputs();
        let st = self.b.read_state();
        let fp = fingerprint(&i.aumid, &i.exe);
        match plan(&i.at, i.enabled, i.multi, i.test, &st, &fp, self.b.now()) {
            Plan::Skip(r) => self.b.log(&format!("jumplist: {why}: not written ({r})")),
            Plan::Write => {
                let _ = self.run_op(Kind::Write, false, &i, fp, why);
            }
            Plan::Delete => {
                let _ = self.run_op(Kind::Delete, false, &i, fp, why);
            }
        }
    }

    /// Start one operation on its own thread and wait for it, at most the kind's
    /// timeout. The thread is never killed; a timeout cancels it at the gate.
    fn run_op(self: &Arc<Self>, kind: Kind, fin: bool, i: &Inputs, fp: String, why: &'static str) -> Ran {
        let id = {
            let mut g = self.lock();
            g.next_id += 1;
            let id = g.next_id;
            g.op = Some(Op { id, kind, phase: Phase::Running });
            id
        };
        let wait = if kind == Kind::Write { self.write_wait } else { self.delete_wait };
        let me = self.clone();
        let (exe, aumid) = (i.exe.clone(), i.aumid.clone());
        let spawned = std::thread::Builder::new().name("jumplist".into()).spawn(move || me.op_thread(id, kind, fin, exe, aumid, fp, why));
        if spawned.is_err() {
            self.b.log(&format!("jumplist: {why}: cannot start a thread"));
            if let Some(op) = self.lock().op.as_mut() {
                op.phase = Phase::Cancelled;
            }
            return Ran::Cancelled;
        }
        let end = Instant::now() + wait;
        let mut g = self.lock();
        loop {
            let closed = g.closed;
            let Some(op) = g.op.as_mut().filter(|o| o.id == id) else { return Ran::Cancelled };
            match op.phase {
                Phase::Done => return Ran::Done,
                Phase::Cancelled => return Ran::Cancelled,
                Phase::Running | Phase::Committing => {}
            }
            // a write is pointless once the final removal was asked for
            if closed && kind == Kind::Write && op.phase == Phase::Running {
                op.phase = Phase::Cancelled;
                drop(g);
                self.b.log(&format!("jumplist: {why}: write cancelled (uninstall)"));
                return Ran::Cancelled;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let secs = wait.as_secs_f32();
                if op.phase == Phase::Running {
                    op.phase = Phase::Cancelled;
                    drop(g);
                    self.b.log(&format!("jumplist: {why}: the shell did not answer in {secs} s; cancelled, it can no longer change the list"));
                    return Ran::Cancelled;
                }
                // inside the shell: keep the work to redo, and stop until it returns
                g.dirty = true;
                drop(g);
                self.b.log(&format!("jumplist: {why}: the shell is still committing after {secs} s; waiting for it before anything else"));
                return Ran::Stuck;
            }
            g = self.cv.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// The commit gate of operation `id` (see the module docs).
    fn gate(&self, id: u64) -> bool {
        let mut g = self.lock();
        let closed = g.closed;
        let ok = match g.op.as_mut() {
            Some(op) if op.id == id && op.phase == Phase::Running => {
                if closed && op.kind == Kind::Write {
                    op.phase = Phase::Cancelled;
                    false
                } else {
                    op.phase = Phase::Committing;
                    true
                }
            }
            _ => false,
        };
        if !ok {
            self.cv.notify_all();
        }
        ok
    }

    fn op_thread(self: Arc<Self>, id: u64, kind: Kind, fin: bool, exe: String, aumid: String, fp: String, why: &'static str) {
        let t0 = Instant::now();
        let gate = || self.gate(id);
        let r = match kind {
            Kind::Write => self.b.write_list(&exe, &aumid, &gate),
            Kind::Delete => self.b.delete_list(&aumid, &gate),
        };
        let persisted = r == Ok(Step::Persisted);
        // the state file is written BEFORE the operation counts as done, so the
        // next operation always reads what this one left. A failed or refused
        // operation leaves it alone: a failed delete keeps the old fingerprint,
        // so the next reconcile deletes again.
        if persisted {
            match (kind, fin) {
                (Kind::Write, _) => self.b.write_state(&State { fingerprint: Some(fp), at: self.b.now(), removed: false }),
                (Kind::Delete, false) => self.b.write_state(&State::after_delete()),
                (Kind::Delete, true) => self.b.clear_state(),
            }
        }
        let ms = t0.elapsed().as_millis();
        self.b.log(&match (&r, kind) {
            (Ok(Step::Persisted), Kind::Write) => format!("jumplist: {why}: {} tasks written for {aumid} -> {exe} ({ms} ms)", TASKS.len()),
            (Ok(Step::Persisted), Kind::Delete) => format!("jumplist: {why}: removed the list of {aumid} ({ms} ms)"),
            (Ok(Step::Refused), _) => format!("jumplist: {why}: stale or cancelled {}, the list was not changed ({ms} ms)", if kind == Kind::Write { "write" } else { "delete" }),
            (Err(e), Kind::Write) => format!("jumplist: {why}: the shell refused the list ({e}); not retried until the next start or switch"),
            (Err(e), Kind::Delete) => format!("jumplist: {why}: could not remove the list ({e})"),
        });
        let kick = {
            let mut g = self.lock();
            let mine = matches!(&g.op, Some(op) if op.id == id);
            if mine {
                if let Some(op) = g.op.as_mut() {
                    if matches!(op.phase, Phase::Running | Phase::Committing) {
                        op.phase = Phase::Done;
                    }
                }
                if fin && persisted {
                    g.fin = Final::Removed;
                }
            }
            // the worker gave up waiting for this one (timeout while committing): wake a new one
            let more = g.dirty || (g.closed && g.fin == Final::Pending);
            let k = mine && more && take_worker(&mut g);
            self.cv.notify_all();
            k
        };
        if kick {
            self.spawn_worker();
        }
    }
}

fn take_worker(g: &mut Inner) -> bool {
    if g.worker {
        false
    } else {
        g.worker = true;
        true
    }
}

#[cfg(test)]
mod tests;
