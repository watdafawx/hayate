//! Ordering tests for the coordinator against a fake shell whose steps block on
//! barriers (`Hook`), so each race is forced, not hoped for.

use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const WAIT: Duration = Duration::from_secs(10);
const LONG: Duration = Duration::from_secs(30);

/// A one-shot latch.
#[derive(Default)]
struct Latch {
    m: Mutex<bool>,
    cv: Condvar,
}

impl Latch {
    fn set(&self) {
        *self.m.lock().unwrap() = true;
        self.cv.notify_all();
    }
    fn wait(&self) {
        let g = self.m.lock().unwrap();
        let (g, t) = self.cv.wait_timeout_while(g, WAIT, |v| !*v).unwrap();
        assert!(*g && !t.timed_out(), "latch never opened");
    }
}

/// A barrier a fake step stops at: `arrived` is set when it gets there, it
/// continues after `release`.
#[derive(Default)]
struct Hook {
    arrived: Latch,
    release: Latch,
}

impl Hook {
    fn new() -> Arc<Hook> {
        Arc::new(Hook::default())
    }
    fn stop(&self) {
        self.arrived.set();
        self.release.wait();
    }
}

struct Fake {
    inp: Mutex<Inputs>,
    /// jumplist.ini
    state: Mutex<Option<State>>,
    /// what the shell holds
    list: Mutex<Option<String>>,
    events: Mutex<Vec<String>>,
    logs: Mutex<Vec<String>>,
    /// stop here (before the gate) in the next operations, in start order
    pre: Mutex<VecDeque<Arc<Hook>>>,
    /// stop here (after the gate, inside the shell's commit)
    inside: Mutex<VecDeque<Arc<Hook>>>,
    fail_delete: AtomicBool,
    writes: AtomicUsize,
}

impl Fake {
    fn new() -> Arc<Fake> {
        Arc::new(Fake {
            inp: Mutex::new(Inputs { at: Where::User, exe: r"C:\Blink\blink.exe".into(), aumid: "Blink.Test".into(), enabled: true, multi: false, test: false }),
            state: Mutex::new(None),
            list: Mutex::new(None),
            events: Mutex::new(Vec::new()),
            logs: Mutex::new(Vec::new()),
            pre: Mutex::new(VecDeque::new()),
            inside: Mutex::new(VecDeque::new()),
            fail_delete: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
        })
    }
    fn set_enabled(&self, on: bool) {
        self.inp.lock().unwrap().enabled = on;
    }
    fn set_where(&self, at: Where, multi: bool, test: bool) {
        let mut i = self.inp.lock().unwrap();
        i.at = at;
        i.multi = multi;
        i.test = test;
    }
    fn hold_pre(&self) -> Arc<Hook> {
        let h = Hook::new();
        self.pre.lock().unwrap().push_back(h.clone());
        h
    }
    fn hold_inside(&self) -> Arc<Hook> {
        let h = Hook::new();
        self.inside.lock().unwrap().push_back(h.clone());
        h
    }
    fn ev(&self, e: &str) {
        self.events.lock().unwrap().push(e.to_string());
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
    fn count(&self, e: &str) -> usize {
        self.events().iter().filter(|x| *x == e).count()
    }
    fn listed(&self) -> bool {
        self.list.lock().unwrap().is_some()
    }
    fn wait_event(&self, e: &str, n: usize) {
        wait_for(|| self.count(e) >= n, e);
    }
}

fn wait_for(mut f: impl FnMut() -> bool, what: &str) {
    let end = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

impl Backend for Arc<Fake> {
    fn inputs(&self) -> Inputs {
        self.inp.lock().unwrap().clone()
    }
    fn read_state(&self) -> State {
        self.state.lock().unwrap().clone().unwrap_or_default()
    }
    fn write_state(&self, s: &State) {
        *self.state.lock().unwrap() = Some(s.clone());
    }
    fn clear_state(&self) {
        *self.state.lock().unwrap() = None;
        self.ev("state:cleared");
    }
    fn write_list(&self, exe: &str, aumid: &str, gate: &dyn Fn() -> bool) -> Result<Step, String> {
        self.ev("write:begin");
        let h = self.pre.lock().unwrap().pop_front();
        if let Some(h) = h {
            h.stop();
        }
        if !gate() {
            self.ev("write:refused");
            return Ok(Step::Refused);
        }
        let h = self.inside.lock().unwrap().pop_front();
        if let Some(h) = h {
            h.stop();
        }
        *self.list.lock().unwrap() = Some(format!("{aumid}|{exe}"));
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.ev("write:commit");
        Ok(Step::Persisted)
    }
    fn delete_list(&self, _aumid: &str, gate: &dyn Fn() -> bool) -> Result<Step, String> {
        self.ev("delete:begin");
        let h = self.pre.lock().unwrap().pop_front();
        if let Some(h) = h {
            h.stop();
        }
        if !gate() {
            self.ev("delete:refused");
            return Ok(Step::Refused);
        }
        let h = self.inside.lock().unwrap().pop_front();
        if let Some(h) = h {
            h.stop();
        }
        if self.fail_delete.load(Ordering::SeqCst) {
            self.ev("delete:failed");
            return Err("E_FAIL".into());
        }
        *self.list.lock().unwrap() = None;
        self.ev("delete:commit");
        Ok(Step::Persisted)
    }
    fn now(&self) -> u64 {
        1000
    }
    fn log(&self, line: &str) {
        self.logs.lock().unwrap().push(line.to_string());
    }
}

fn coord(f: &Arc<Fake>) -> Arc<Coordinator<Arc<Fake>>> {
    Coordinator::new(f.clone(), LONG, LONG)
}

fn coord_t(f: &Arc<Fake>, ms: u64) -> Arc<Coordinator<Arc<Fake>>> {
    Coordinator::new(f.clone(), Duration::from_millis(ms), Duration::from_millis(ms))
}

/// no worker and nothing pending
fn idle<B: Backend>(c: &Arc<Coordinator<B>>) {
    wait_for(
        || {
            let g = c.lock();
            !g.worker && !g.dirty
        },
        "the worker to go idle",
    );
}

fn state_of(f: &Arc<Fake>) -> Option<State> {
    f.state.lock().unwrap().clone()
}

// ---- (1) toggles ----------------------------------------------------------------

#[test]
fn off_during_the_first_registration_ends_removed() {
    let f = Fake::new();
    let c = coord(&f);
    let h = f.hold_pre();
    c.request("start");
    h.arrived.wait();
    // the user switches it off while the very first write is still in the shell
    f.set_enabled(false);
    c.request("setting changed");
    h.release.set();
    idle(&c);
    assert!(!f.listed(), "the list must end removed: {:?}", f.events());
    assert_eq!(f.events(), ["write:begin", "write:commit", "delete:begin", "delete:commit"]);
    assert_eq!(state_of(&f), Some(State::after_delete()));
}

#[test]
fn on_during_a_delete_ends_registered() {
    let f = Fake::new();
    *f.list.lock().unwrap() = Some("old".into());
    *f.state.lock().unwrap() = Some(State { fingerprint: Some("0123456789abcdef".into()), at: 1, removed: false });
    f.set_enabled(false);
    let c = coord(&f);
    let h = f.hold_pre();
    c.request("setting changed");
    h.arrived.wait();
    f.set_enabled(true);
    c.request("setting changed");
    h.release.set();
    idle(&c);
    assert!(f.listed(), "the list must end registered: {:?}", f.events());
    assert_eq!(f.events(), ["delete:begin", "delete:commit", "write:begin", "write:commit"]);
    assert!(state_of(&f).unwrap().fingerprint.is_some());
}

#[test]
fn a_burst_of_toggles_ends_at_the_last_position() {
    for last in [true, false] {
        let f = Fake::new();
        let c = coord(&f);
        let h = f.hold_pre();
        c.request("start");
        h.arrived.wait();
        for n in 0..7 {
            f.set_enabled(n % 2 == 0);
            c.request("setting changed");
        }
        f.set_enabled(last);
        c.request("setting changed");
        h.release.set();
        idle(&c);
        assert_eq!(f.listed(), last, "{:?}", f.events());
        // never two operations at once: begin/commit pairs never interleave
        let ev = f.events();
        for w in ev.chunks(2) {
            assert!(w[0].ends_with(":begin") && w[1].ends_with(":commit"), "{ev:?}");
        }
    }
}

// ---- (2) uninstall --------------------------------------------------------------

#[test]
fn uninstall_while_a_write_prepares_means_the_write_never_commits() {
    let f = Fake::new();
    let c = coord(&f);
    let h = f.hold_pre();
    c.request("start");
    h.arrived.wait();
    assert!(c.remove_for_good("uninstall"));
    // the stuck write is released only now, AFTER the removal was asked for
    h.release.set();
    assert_eq!(c.wait_final(WAIT), Final::Removed);
    f.wait_event("write:refused", 1);
    assert_eq!(f.count("write:commit"), 0, "{:?}", f.events());
    assert!(!f.listed());
    assert_eq!(state_of(&f), None, "the state file goes too");
}

#[test]
fn uninstall_never_returns_while_a_write_is_hung_and_still_ends_removed() {
    // the write thread stays hung (never released) for the whole test: the UI wait is bounded and
    // the removal still happens, because that write is cancelled at the gate, not waited for
    let f = Fake::new();
    let c = coord(&f);
    let h = f.hold_pre();
    c.request("start");
    h.arrived.wait();
    let t0 = Instant::now();
    assert!(c.remove_for_good("uninstall"));
    assert_eq!(c.wait_final(Duration::from_secs(5)), Final::Removed);
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert!(!f.listed());
    // nothing the zombie does later can recreate it
    h.release.set();
    f.wait_event("write:refused", 1);
    assert!(!f.listed());
}

#[test]
fn uninstall_orders_the_delete_after_a_commit_already_inside_the_shell() {
    let f = Fake::new();
    let c = coord(&f);
    let h = f.hold_inside();
    c.request("start");
    h.arrived.wait(); // past the gate: this commit can no longer be taken back
    assert!(c.remove_for_good("uninstall"));
    // while the commit is inside the shell the delete must wait for it, not race it
    assert_eq!(c.wait_final(Duration::from_millis(150)), Final::Pending);
    assert_eq!(f.count("delete:begin"), 0, "{:?}", f.events());
    h.release.set();
    assert_eq!(c.wait_final(WAIT), Final::Removed);
    assert_eq!(f.events(), ["write:begin", "write:commit", "delete:begin", "delete:commit", "state:cleared"]);
    assert!(!f.listed());
}

#[test]
fn nothing_writes_after_the_final_removal() {
    let f = Fake::new();
    let c = coord(&f);
    assert!(c.remove_for_good("uninstall"));
    assert_eq!(c.wait_final(WAIT), Final::Removed);
    let before = f.events();
    // the start sync, a later switch, anything
    c.request("start");
    f.set_enabled(false);
    f.set_enabled(true);
    c.request("setting changed");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(f.events(), before);
    assert!(!f.listed());
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn a_failing_final_delete_is_retried_once_then_given_up_without_hanging() {
    let f = Fake::new();
    f.fail_delete.store(true, Ordering::SeqCst);
    *f.list.lock().unwrap() = Some("old".into());
    *f.state.lock().unwrap() = Some(State { fingerprint: Some("0123456789abcdef".into()), at: 1, removed: false });
    let c = coord(&f);
    assert!(c.remove_for_good("uninstall"));
    assert_eq!(c.wait_final(WAIT), Final::GaveUp);
    assert_eq!(f.count("delete:failed"), 2);
    // a failed delete keeps the retry state (the state file is only cleared on a confirmed delete)
    assert!(state_of(&f).is_some());
    // and it is still impossible to write
    c.request("start");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(f.count("write:begin"), 0);
}

// ---- (3) eligibility ------------------------------------------------------------

#[test]
fn portable_and_multi_copies_never_remove_the_global_list() {
    for (at, multi) in [(Where::Portable, false), (Where::User, true), (Where::Machine, true), (Where::Portable, true)] {
        let f = Fake::new();
        // an installed copy's list exists and must survive this copy's "uninstall"
        *f.list.lock().unwrap() = Some("installed copy's list".into());
        f.set_where(at.clone(), multi, false);
        let c = coord(&f);
        assert!(!c.remove_for_good("uninstall"), "{at:?} multi {multi}");
        assert!(!c.is_closed());
        assert_eq!(c.wait_final(Duration::from_millis(10)), Final::Pending);
        // not even a switch-off sync touches it
        f.set_enabled(false);
        *f.state.lock().unwrap() = Some(State { fingerprint: Some("0123456789abcdef".into()), at: 1, removed: false });
        c.request("setting changed");
        idle(&c);
        assert_eq!(f.count("delete:begin"), 0, "{at:?} multi {multi}: {:?}", f.events());
        assert!(f.listed());
    }
}

#[test]
fn installed_copies_and_the_debug_identity_do_remove() {
    for (at, multi, test) in [(Where::User, false, false), (Where::Machine, false, false), (Where::Portable, true, true)] {
        let f = Fake::new();
        *f.list.lock().unwrap() = Some("list".into());
        f.set_where(at.clone(), multi, test);
        let c = coord(&f);
        assert!(c.remove_for_good("uninstall"), "{at:?}");
        assert_eq!(c.wait_final(WAIT), Final::Removed);
        assert!(!f.listed());
    }
}

// ---- (4) timeouts ---------------------------------------------------------------

#[test]
fn a_timed_out_write_can_never_commit_later() {
    let f = Fake::new();
    let c = coord_t(&f, 120);
    let h = f.hold_pre();
    c.request("start");
    h.arrived.wait();
    idle(&c); // the worker timed out, cancelled it, and is done: nothing was requested meanwhile
    assert!(!f.listed());
    assert_eq!(state_of(&f), None, "a cancelled write leaves no cache");
    // the detached shell call wakes up much later
    h.release.set();
    f.wait_event("write:refused", 1);
    std::thread::sleep(Duration::from_millis(30));
    assert!(!f.listed(), "{:?}", f.events());
    assert_eq!(state_of(&f), None);
    assert_eq!(f.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn a_stale_write_cannot_overwrite_a_newer_one() {
    let f = Fake::new();
    let c = coord_t(&f, 120);
    let h = f.hold_pre();
    c.request("start");
    h.arrived.wait();
    idle(&c); // op 1 timed out and is cancelled
    c.request("setting changed"); // op 2: unhindered
    idle(&c);
    assert!(f.listed());
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    // now the old one wakes up: refused, the newer list stands, exactly one commit ever
    h.release.set();
    f.wait_event("write:refused", 1);
    assert!(f.listed());
    assert_eq!(f.writes.load(Ordering::SeqCst), 1);
    assert_eq!(f.count("write:commit"), 1);
}

#[test]
fn a_timed_out_delete_cannot_remove_a_newer_registration() {
    let f = Fake::new();
    *f.list.lock().unwrap() = Some("old".into());
    *f.state.lock().unwrap() = Some(State { fingerprint: Some("0123456789abcdef".into()), at: 1, removed: false });
    f.set_enabled(false);
    let c = coord_t(&f, 120);
    let h = f.hold_pre();
    c.request("setting changed");
    h.arrived.wait();
    idle(&c); // the delete timed out and was cancelled
    f.set_enabled(true);
    c.request("setting changed");
    idle(&c);
    assert!(f.listed());
    h.release.set(); // the old delete finally runs: refused
    f.wait_event("delete:refused", 1);
    assert!(f.listed(), "{:?}", f.events());
    assert_eq!(f.count("delete:commit"), 0);
}

#[test]
fn a_request_that_arrives_during_an_op_that_times_out_is_not_lost() {
    let f = Fake::new();
    let c = coord_t(&f, 400);
    let h = f.hold_pre();
    c.request("start");
    h.arrived.wait();
    f.set_enabled(false);
    c.request("setting changed"); // arrives while op 1 is hung
    // op 1 times out, is cancelled, and the loop reconciles the LATEST preference
    idle(&c);
    assert!(!f.listed());
    assert_eq!(f.count("delete:commit"), 1, "{:?}", f.events());
    assert_eq!(state_of(&f), Some(State::after_delete()));
    h.release.set();
    f.wait_event("write:refused", 1);
    assert!(!f.listed());
}

#[test]
fn a_commit_stuck_inside_the_shell_holds_everything_else_until_it_returns() {
    let f = Fake::new();
    let c = coord_t(&f, 100);
    let h = f.hold_inside();
    c.request("start");
    h.arrived.wait(); // inside the commit
    wait_for(|| !c.lock().worker, "the worker to stop waiting"); // timed out while committing
    // the user switches it off: nothing may touch the shell until that commit returns
    f.set_enabled(false);
    c.request("setting changed");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(f.count("delete:begin"), 0, "{:?}", f.events());
    h.release.set();
    // the commit returning wakes the worker, which then applies the latest position
    wait_for(|| !f.listed() && f.count("delete:commit") == 1 && !c.lock().worker, "the delete after the commit");
    assert_eq!(f.events(), ["write:begin", "write:commit", "delete:begin", "delete:commit"]);
    assert_eq!(state_of(&f), Some(State::after_delete()));
}

// ---- (5) the cache never decides removal ------------------------------------------

#[test]
fn switching_off_deletes_even_without_a_cache() {
    let f = Fake::new();
    *f.list.lock().unwrap() = Some("a list jumplist.ini never heard of".into());
    f.set_enabled(false);
    let c = coord(&f);
    c.request("start");
    idle(&c);
    assert!(!f.listed(), "{:?}", f.events());
    assert_eq!(state_of(&f), Some(State::after_delete()));
    // confirmed once: quiet afterwards
    c.request("start");
    idle(&c);
    assert_eq!(f.count("delete:begin"), 1);
}

#[test]
fn a_failed_delete_keeps_its_retry_state() {
    let f = Fake::new();
    *f.list.lock().unwrap() = Some("old".into());
    *f.state.lock().unwrap() = Some(State { fingerprint: Some("0123456789abcdef".into()), at: 1, removed: false });
    f.set_enabled(false);
    f.fail_delete.store(true, Ordering::SeqCst);
    let c = coord(&f);
    c.request("setting changed");
    idle(&c);
    assert!(f.listed());
    let st = state_of(&f).unwrap();
    assert!(st.fingerprint.is_some() && !st.removed, "state must still say a list may exist: {st:?}");
    // next start retries and succeeds
    f.fail_delete.store(false, Ordering::SeqCst);
    c.request("start");
    idle(&c);
    assert!(!f.listed());
    assert!(state_of(&f).unwrap().removed);
}
