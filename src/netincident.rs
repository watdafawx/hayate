//! Network incident history: when the Network verdict stays Degraded or Offline for a while, ONE
//! Watch event records it ("OpenJDK Platform binary could not reach 10.0.0.2:25565 ·
//! 61 failed/min, no reply · 14:02–14:09"), and the same event is closed with its end time when
//! the verdict clears. Pure state machine: no clock, no I/O, no Windows. The caller feeds one
//! observation per Network paint and acts on the returned `Action`.
//!
//! Events can only be detected while the Network detail is shown and Blink is running: the
//! verdict is computed from counters that are only read then, and nothing polls on its own. A
//! gap in the observations (detail hidden, window minimised) never counts as time in an incident:
//! an open incident is closed at the last time it was seen failing.
//!
//! Rules:
//! - the verdict must be bad for `window` seconds (default 120) without a clear stretch of
//!   `CLEAR_SECS` (30): short dips inside an incident do not split it, short blips never start one;
//! - one event per incident; the offender named in it is the latest noticeable one (app + remote);
//! - per (app, remote) there is a `COOLDOWN_SECS` (600) cooldown from the start or end of its last
//!   incident: a pair that fails again inside it opens no new event;
//! - no toast (the Watch feed is the record).

use std::collections::HashMap;
#[cfg(debug_assertions)]
use std::sync::OnceLock;

/// Seconds the verdict must stay bad before an event is written.
pub const INCIDENT_SECS: u64 = 120;
/// Seconds of "not bad" that end an incident (or cancel a pending one).
pub const CLEAR_SECS: u64 = 30;
/// Per (app, remote) pair: no second event within this many seconds of the last one.
pub const COOLDOWN_SECS: u64 = 600;
/// Observations further apart than this are a gap (the detail was not shown).
pub const GAP_SECS: u64 = 10;
/// Test-only override of `INCIDENT_SECS` (smoke tests), 5..=3600. Compiled in debug builds
/// only: release always uses `INCIDENT_SECS`.
#[cfg(debug_assertions)]
pub const ENV_INCIDENT_SECS: &str = "BLINK_NET_INCIDENT_SECS";

/// `INCIDENT_SECS`, or the test override.
#[cfg(debug_assertions)]
pub fn window_secs() -> u64 {
    static S: OnceLock<u64> = OnceLock::new();
    *S.get_or_init(|| std::env::var(ENV_INCIDENT_SECS).ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|v| (5..=3600).contains(v)).unwrap_or(INCIDENT_SECS))
}

/// Release: no environment hook.
#[cfg(not(debug_assertions))]
pub fn window_secs() -> u64 {
    INCIDENT_SECS
}

/// Who an incident is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub app: String,
    pub remote: String,
}

/// What the event says. `app` / `remote` are empty when no single offender is known.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub app: String,
    /// "10.0.0.2:25565" / "[2001:db8::1]:443"
    pub remote: String,
    /// worst failed + dropped per minute seen during the incident
    pub per_min: u32,
    /// the failure is dropped connections, not unreachable ones
    pub midstream: bool,
    /// lower-case kind, "no reply"
    pub kind: String,
    /// the verdict label, used when there is no offender
    pub label: String,
    /// exe path of the app (its icon in the feed)
    pub path: String,
}

impl Info {
    pub fn key(&self) -> Key {
        Key { app: self.app.clone(), remote: self.remote.clone() }
    }
    pub fn has_offender(&self) -> bool {
        !self.app.is_empty() && !self.remote.is_empty()
    }
    /// The event title: the app, or "Network".
    pub fn name(&self) -> String {
        if self.has_offender() { self.app.clone() } else { "Network".into() }
    }
    /// The event's second line. `end` None: still going ("since 14:02").
    pub fn detail(&self, start: u64, end: Option<u64>, clock: &dyn Fn(u64) -> String) -> String {
        let when = match end {
            Some(e) => format!("{}–{}", clock(start), clock(e)),
            None => format!("since {}", clock(start)),
        };
        if self.has_offender() {
            let (verb, what) = if self.midstream { ("dropped connections to", "dropped") } else { ("could not reach", "failed") };
            let kind = if self.kind.is_empty() { String::new() } else { format!(", {}", self.kind) };
            format!("{verb} {} · {} {what}/min{kind} · {when}", self.remote, self.per_min)
        } else {
            format!("{} · {when}", if self.label.is_empty() { "Network degraded" } else { &self.label })
        }
    }
}

/// One look at the verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct Obs {
    /// the verdict is Degraded or Offline
    pub bad: bool,
    pub info: Info,
}

/// What the caller must do.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Write the event (start time = when the verdict first went bad), then call `Tracker::opened(id)`.
    Open { start: u64, info: Info },
    /// Close the event `id` with the end time.
    Close { id: u64, start: u64, end: u64, info: Info },
}

#[derive(Clone, Debug, PartialEq)]
enum State {
    Idle,
    /// bad since `since`, not yet long enough
    Pending { since: u64, last_bad: u64, info: Info },
    /// the event is written (`id` set by `opened`)
    Open { id: Option<u64>, since: u64, last_bad: u64, info: Info },
}

pub struct Tracker {
    window: u64,
    state: State,
    last_seen: u64,
    /// pair -> unix time of its last event start / end
    cool: HashMap<Key, u64>,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker::new(window_secs())
    }
}

impl Tracker {
    pub fn new(window: u64) -> Tracker {
        Tracker { window, state: State::Idle, last_seen: 0, cool: HashMap::new() }
    }

    /// The event for the last `Action::Open` got this id.
    pub fn opened(&mut self, new_id: u64) {
        if let State::Open { id, .. } = &mut self.state {
            *id = Some(new_id);
        }
    }

    fn cooling(&self, k: &Key, now: u64) -> bool {
        self.cool.get(k).is_some_and(|t| now.saturating_sub(*t) < COOLDOWN_SECS)
    }

    /// Fold a newer offender into the incident's info: the latest named pair wins and its rate
    /// restarts; the same pair keeps the worst rate seen.
    fn merge(info: &mut Info, o: &Info) {
        if !o.has_offender() {
            if !info.has_offender() {
                info.label = o.label.clone();
            }
            return;
        }
        if info.key() == o.key() {
            let peak = info.per_min.max(o.per_min);
            *info = Info { per_min: peak, ..o.clone() };
        } else {
            *info = o.clone();
        }
    }

    /// One observation at unix time `now`. At most one action per call.
    pub fn observe(&mut self, now: u64, o: &Obs) -> Option<Action> {
        let gap = self.last_seen != 0 && now.saturating_sub(self.last_seen) > GAP_SECS;
        let seen = self.last_seen;
        self.last_seen = now;
        if gap {
            // nobody looked: a pending incident is forgotten, an open one ends where it was last seen failing
            match std::mem::replace(&mut self.state, State::Idle) {
                State::Open { id, since, last_bad, info } => {
                    let end = last_bad.min(seen).max(since);
                    self.cool.insert(info.key(), end);
                    return id.map(|id| Action::Close { id, start: since, end, info });
                }
                _ => {}
            }
        }
        if self.cool.len() > 64 {
            self.cool.retain(|_, t| now.saturating_sub(*t) < COOLDOWN_SECS);
        }
        match &mut self.state {
            State::Idle => {
                if o.bad {
                    self.state = State::Pending { since: now, last_bad: now, info: o.info.clone() };
                }
                None
            }
            State::Pending { since, last_bad, info } => {
                if o.bad {
                    *last_bad = now;
                    Self::merge(info, &o.info);
                } else if now.saturating_sub(*last_bad) >= CLEAR_SECS {
                    self.state = State::Idle;
                    return None;
                }
                // opens only on a bad look, never during a clear stretch
                if !o.bad || now.saturating_sub(*since) < self.window {
                    return None;
                }
                let (since, last_bad, info) = (*since, *last_bad, info.clone());
                if self.cooling(&info.key(), now) {
                    // this pair already has an event: no second one for the same trouble
                    self.state = State::Idle;
                    return None;
                }
                self.cool.insert(info.key(), now);
                self.state = State::Open { id: None, since, last_bad, info: info.clone() };
                Some(Action::Open { start: since, info })
            }
            State::Open { id, since, last_bad, info } => {
                if o.bad {
                    *last_bad = now;
                    Self::merge(info, &o.info);
                    return None;
                }
                if now.saturating_sub(*last_bad) < CLEAR_SECS {
                    return None;
                }
                let (id, since, end, info) = (*id, *since, *last_bad, info.clone());
                self.cool.insert(info.key(), now);
                self.state = State::Idle;
                id.map(|id| Action::Close { id, start: since, end, info })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_800_000_000;

    fn off(app: &str, remote: &str, per_min: u32) -> Info {
        Info { app: app.into(), remote: remote.into(), per_min, kind: "no reply".into(), label: "Degraded: connections failing".into(), ..Default::default() }
    }
    fn bad(i: Info) -> Obs {
        Obs { bad: true, info: i }
    }
    fn ok() -> Obs {
        Obs { bad: false, info: Info::default() }
    }
    fn clock(t: u64) -> String {
        let s = t % 86400;
        format!("{:02}:{:02}", s / 3600, s / 60 % 60)
    }
    /// Feed `obs` once a second from `from` to `to` inclusive; returns the actions and when they came.
    fn run(t: &mut Tracker, from: u64, to: u64, obs: &Obs) -> Vec<(u64, Action)> {
        let mut out = Vec::new();
        for s in from..=to {
            if let Some(a) = t.observe(T0 + s, obs) {
                if matches!(a, Action::Open { .. }) {
                    t.opened(7);
                }
                out.push((s, a));
            }
        }
        out
    }

    #[test]
    fn a_bad_verdict_opens_one_event_after_the_window() {
        let mut t = Tracker::new(120);
        let j = off("OpenJDK Platform binary", "10.0.0.2:25565", 61);
        let a = run(&mut t, 0, 119, &bad(j.clone()));
        assert!(a.is_empty(), "119 s is not enough");
        let a = run(&mut t, 120, 400, &bad(j.clone()));
        assert_eq!(a.len(), 1, "one event for 280 more seconds of the same trouble");
        assert_eq!(a[0].0, 120);
        assert_eq!(a[0].1, Action::Open { start: T0, info: j });
    }

    #[test]
    fn a_short_blip_never_opens() {
        let mut t = Tracker::new(120);
        let j = off("x.exe", "10.0.0.2:80", 20);
        assert!(run(&mut t, 0, 90, &bad(j.clone())).is_empty());
        // clear for the full CLEAR_SECS: the pending incident is forgotten
        assert!(run(&mut t, 91, 130, &ok()).is_empty());
        // bad again: the clock restarts at 131, so 120 s later (not at 120) it opens
        assert!(run(&mut t, 131, 250, &bad(j.clone())).is_empty());
        let a = run(&mut t, 251, 251, &bad(j));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].1, Action::Open { start: T0 + 131, info: off("x.exe", "10.0.0.2:80", 20) });
    }

    #[test]
    fn a_dip_shorter_than_the_clear_time_does_not_split_an_incident() {
        let mut t = Tracker::new(60);
        let j = off("x.exe", "10.0.0.2:80", 20);
        run(&mut t, 0, 40, &bad(j.clone()));
        run(&mut t, 41, 60, &ok()); // 20 s clear
        let a = run(&mut t, 61, 200, &bad(j.clone()));
        assert_eq!(a.len(), 1, "opened once, at the first check past the window");
        assert!(matches!(a[0].1, Action::Open { start, .. } if start == T0));
        // and a dip inside an open incident does not close it either
        assert!(run(&mut t, 201, 220, &ok()).is_empty());
        assert!(run(&mut t, 221, 300, &bad(j)).is_empty());
    }

    #[test]
    fn clearing_closes_the_event_with_the_last_bad_time_and_the_worst_rate() {
        let mut t = Tracker::new(60);
        run(&mut t, 0, 100, &bad(off("x.exe", "10.0.0.2:80", 20)));
        run(&mut t, 101, 110, &bad(off("x.exe", "10.0.0.2:80", 95)));
        run(&mut t, 111, 140, &bad(off("x.exe", "10.0.0.2:80", 12)));
        assert!(run(&mut t, 141, 169, &ok()).is_empty(), "29 s clear is still inside the incident");
        let a = run(&mut t, 170, 200, &ok());
        assert_eq!(a.len(), 1);
        match &a[0].1 {
            Action::Close { id, start, end, info } => {
                assert_eq!((*id, *start, *end), (7, T0, T0 + 140));
                assert_eq!(info.per_min, 95, "the peak, not the last value");
            }
            x => panic!("{x:?}"),
        }
        // nothing more once idle
        assert!(run(&mut t, 201, 300, &ok()).is_empty());
    }

    #[test]
    fn the_same_pair_is_not_reported_again_inside_the_cooldown_but_another_pair_is() {
        let mut t = Tracker::new(60);
        let a = off("x.exe", "10.0.0.2:80", 30);
        run(&mut t, 0, 100, &bad(a.clone()));
        run(&mut t, 101, 140, &ok()); // closes at 131
        // fails again 60 s later, for far longer than the window: still no second event (cooldown from the close at 131)
        let r = run(&mut t, 200, 500, &bad(a.clone()));
        assert!(r.is_empty(), "{r:?}");
        run(&mut t, 501, 540, &ok());
        // a different remote of the same app opens its own event
        let b = off("x.exe", "10.0.0.9:80", 30);
        let r = run(&mut t, 600, 700, &bad(b));
        assert_eq!(r.len(), 1, "{r:?}");
        // after the cooldown the first pair can open again (last event of the pair ended at its close, T0+500+..)
        run(&mut t, 701, 760, &ok());
        let r = run(&mut t, 2000, 2100, &bad(a));
        assert_eq!(r.len(), 1, "cooldown over");
    }

    #[test]
    fn a_gap_in_the_observations_closes_at_the_last_failing_time() {
        let mut t = Tracker::new(60);
        let j = off("x.exe", "10.0.0.2:80", 30);
        run(&mut t, 0, 100, &bad(j.clone()));
        // the detail is hidden for 10 minutes: the next look closes the incident where it was last seen
        let a = run(&mut t, 700, 700, &bad(j.clone()));
        assert_eq!(a.len(), 1);
        assert!(matches!(&a[0].1, Action::Close { end, .. } if *end == T0 + 100), "{a:?}");
        // a pending incident is forgotten by a gap
        let mut t = Tracker::new(120);
        run(&mut t, 0, 100, &bad(j.clone()));
        assert!(run(&mut t, 400, 450, &bad(j)).is_empty(), "the 100 s before the gap do not count toward the window");
    }

    #[test]
    fn no_offender_still_records_the_verdict_and_a_late_offender_is_adopted() {
        let mut t = Tracker::new(60);
        let anon = Info { label: "Degraded: adapter errors".into(), ..Default::default() };
        run(&mut t, 0, 30, &bad(anon.clone()));
        // the offender shows up half way: same incident, same start, named
        let j = off("x.exe", "10.0.0.2:80", 40);
        let a = run(&mut t, 31, 80, &bad(j.clone()));
        assert_eq!(a.len(), 1);
        assert!(matches!(&a[0].1, Action::Open { start, info } if *start == T0 && info.key() == j.key()), "{a:?}");
        // with no offender at all the event is about the network
        let mut t = Tracker::new(60);
        let a = run(&mut t, 0, 70, &bad(anon.clone()));
        match &a[0].1 {
            Action::Open { info, .. } => {
                assert_eq!(info.name(), "Network");
                assert_eq!(info.detail(T0, None, &clock), format!("Degraded: adapter errors · since {}", clock(T0)));
            }
            x => panic!("{x:?}"),
        }
    }

    #[test]
    fn detail_text_matches_the_mock() {
        let i = off("OpenJDK Platform binary", "10.0.0.2:25565", 61);
        let (s, e) = (T0, T0 + 7 * 60);
        assert_eq!(i.detail(s, Some(e), &clock), "could not reach 10.0.0.2:25565 · 61 failed/min, no reply · 08:00–08:07");
        assert_eq!(i.detail(s, None, &clock), "could not reach 10.0.0.2:25565 · 61 failed/min, no reply · since 08:00");
        let m = Info { midstream: true, kind: "dropped midstream".into(), ..i };
        assert_eq!(m.detail(s, Some(e), &clock), "dropped connections to 10.0.0.2:25565 · 61 dropped/min, dropped midstream · 08:00–08:07");
    }

    #[test]
    fn an_event_that_was_never_confirmed_written_is_not_closed() {
        // Open with no `opened(id)` call (the Watch was not available): nothing to close, no panic
        let mut t = Tracker::new(10);
        let j = off("x.exe", "10.0.0.2:80", 30);
        for s in 0..=20 {
            let _ = t.observe(T0 + s, &bad(j.clone()));
        }
        let mut closes = 0;
        for s in 21..=80 {
            if t.observe(T0 + s, &ok()).is_some() {
                closes += 1;
            }
        }
        assert_eq!(closes, 0);
    }
}
