//! 60 s rolling throughput history per remote scope (All / LAN / WAN), all adapters.
//!
//! Source: the ETW feed (`netetw::Feed`), the same attributed-process totals the scope
//! strip under TOP BY NETWORK shows. Not the selected adapter's counters, and not link
//! utilisation: bytes of processes whose PID could be attributed for the whole interval.
//!
//! The sampler thread owns one `ScopeHist` and calls `observe` once per tick with its
//! monotonic clock, so the history follows wall time, not paint calls. One slot is one
//! wall-clock second (`now_ms / 1000`), stored at `second % HIST`.
//!
//! Honesty rules:
//! - a slot holds a value only where an ETW interval really measured it; every other slot
//!   is NaN (drawn as a gap, never joined, never 0): feed off, needs admin, unavailable,
//!   starting / baseline interval, unreadable events, a skipped second
//! - an interval that spans several seconds (a stalled sampler) fills them with its
//!   average, nothing beyond the seconds it covered
//! - a restart of the ETW session always passes through a non-`Live` feed, so the restart
//!   shows as a gap; the history is dropped entirely once the feed is `Off` (Network
//!   detail closed for 20 s) and what comes back after a long absence is blank
//! - fixed memory: 6 x 60 f32, no allocation per tick

use crate::netetw::{Feed, ScopeTotals, Status, View};
use crate::sample::HIST;

/// What the Throughput graph plots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Graph {
    /// The selected adapter's own counters (the default).
    #[default]
    Adapter,
    /// ETW: every adapter, every attributed process.
    All,
    /// ETW: every adapter, remote end private.
    Lan,
    /// ETW: every adapter, remote end public.
    Wan,
}

impl Graph {
    /// The ETW view this graph reads (`None`: not an ETW graph).
    pub fn view(self) -> Option<View> {
        match self {
            Graph::Adapter => None,
            Graph::All => Some(View::All),
            Graph::Lan => Some(View::Lan),
            Graph::Wan => Some(View::Wan),
        }
    }
}

/// Mb/s of a byte rate (ETW counts bytes, the Vitals graphs bits).
pub fn mbps(bytes_per_s: u64) -> f32 {
    (bytes_per_s as f64 * 8.0 / 1e6) as f32
}

/// The numbers a graph shows for one scope.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    pub down: f32,
    pub up: f32,
}

fn pick(t: &ScopeTotals, v: View) -> (u64, u64) {
    match v {
        View::All => t.all,
        View::Lan => t.lan,
        View::Wan => t.wan,
    }
}

/// Totals of a feed that really measured traffic: `Live`, a completed interval, and not
/// "events arrive but cannot be read" (an empty list with unreadable events is not idle).
pub fn readable(feed: &Feed) -> Option<ScopeTotals> {
    let ok = feed.status == Status::Live && feed.iv.is_some() && !(feed.rows.is_empty() && feed.bad > 0);
    ok.then(|| feed.totals())
}

/// Current rates of a scope from a feed (`None` while there is no valid measurement).
pub fn current(feed: &Feed, v: View) -> Option<Rates> {
    let (d, u) = pick(&readable(feed)?, v);
    Some(Rates { down: mbps(d), up: mbps(u) })
}

/// The last 60 s of one scope: `HIST` values per direction, oldest first, NaN = no data.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Series {
    pub all: (Vec<f32>, Vec<f32>),
    pub lan: (Vec<f32>, Vec<f32>),
    pub wan: (Vec<f32>, Vec<f32>),
}

impl Series {
    /// (down, up) of a view; empty slices when the history has not been read.
    pub fn of(&self, v: View) -> (&[f32], &[f32]) {
        let s = match v {
            View::All => &self.all,
            View::Lan => &self.lan,
            View::Wan => &self.wan,
        };
        (&s.0, &s.1)
    }
}

/// Slot values, in the order all down, all up, lan down, lan up, wan down, wan up.
const N: usize = 6;

pub struct ScopeHist {
    /// wall second (collector clock / 1000) of the newest slot; `None` = nothing recorded
    sec: Option<u64>,
    /// `Interval::id` of the last interval written
    last_iv: u64,
    buf: [[f32; HIST]; N],
}

impl Default for ScopeHist {
    fn default() -> Self {
        ScopeHist { sec: None, last_iv: 0, buf: [[f32::NAN; HIST]; N] }
    }
}

impl ScopeHist {
    pub fn reset(&mut self) {
        self.sec = None;
        self.buf = [[f32::NAN; HIST]; N];
    }

    /// Open the slots up to second `now`: new seconds start as gaps.
    fn advance(&mut self, now: u64) {
        match self.sec {
            Some(s) if now <= s => {} // same second (or the clock stepped back: never rewrite the past)
            Some(s) if now - s >= HIST as u64 => {
                self.buf = [[f32::NAN; HIST]; N];
                self.sec = Some(now);
            }
            Some(s) => {
                for t in s + 1..=now {
                    for row in &mut self.buf {
                        row[t as usize % HIST] = f32::NAN;
                    }
                }
                self.sec = Some(now);
            }
            None => {
                self.buf = [[f32::NAN; HIST]; N];
                self.sec = Some(now);
            }
        }
    }

    /// One sampler tick. `now_ms`: the clock the collector ticks with. Call it every tick
    /// the sampler runs; it does nothing but bookkeeping.
    pub fn observe(&mut self, now_ms: u64, feed: &Feed) {
        if self.sec.is_some_and(|sec| now_ms / 1000 < sec) {
            return; // reject the whole observation, including its interval id
        }
        if feed.status == Status::Off {
            self.reset();
            return;
        }
        let now = now_ms / 1000;
        self.advance(now);
        let (Some(iv), Some(t)) = (feed.iv, readable(feed)) else { return };
        if iv.id == self.last_iv {
            return; // the cached copy of an interval already written
        }
        self.last_iv = iv.id;
        let Some(newest) = self.sec else { return };
        // seconds the interval covers: (start second, end second], clipped to the 60 s window
        let end = (iv.end_ms / 1000).min(newest);
        let start = iv.end_ms.saturating_sub(iv.span_ms) / 1000;
        let first = (start + 1).max(newest.saturating_sub(HIST as u64 - 1));
        let vals = [t.all.0, t.all.1, t.lan.0, t.lan.1, t.wan.0, t.wan.1].map(mbps);
        for sec in first..=end {
            let i = sec as usize % HIST;
            // a second that already holds a measurement is never overwritten
            if self.buf[0][i].is_finite() {
                continue;
            }
            for (row, v) in self.buf.iter_mut().zip(vals) {
                row[i] = v;
            }
        }
    }

    /// The 60 s window ending at the newest second, oldest first. Seconds before the
    /// first observation (and everything after a reset) are NaN.
    pub fn series(&self) -> Series {
        let out = |row: &[f32; HIST]| -> Vec<f32> {
            (0..HIST)
                .map(|k| {
                    let Some(newest) = self.sec else { return f32::NAN };
                    let back = (HIST - 1 - k) as u64;
                    match newest.checked_sub(back) {
                        Some(t) => row[t as usize % HIST],
                        None => f32::NAN,
                    }
                })
                .collect()
        };
        Series { all: (out(&self.buf[0]), out(&self.buf[1])), lan: (out(&self.buf[2]), out(&self.buf[3])), wan: (out(&self.buf[4]), out(&self.buf[5])) }
    }
}

/// Debug builds only, `HAYATE_TEST_NETSCOPE=1`: replaces a collecting feed with a synthetic
/// one (a 24 s cycle of changing LAN / WAN rates and a 3 s baseline gap) so the scope graphs
/// can be inspected without an elevated Hayate. Besides the pid 0 row it adds rows of the first
/// few live processes `ids`, so the Top by Network list has rows to read. Never compiled into
/// release builds.
#[cfg(debug_assertions)]
pub fn synthetic(now_ms: u64, real: Feed, ids: &std::collections::HashMap<u32, i64>) -> Feed {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if real.status == Status::Off || !*ON.get_or_init(|| crate::osutil::env_var_os("TEST_NETSCOPE").is_some()) {
        return real;
    }
    let s = now_ms / 1000;
    if (16..19).contains(&(s % 24)) {
        return Feed { status: Status::Collecting, ..Feed::OFF };
    }
    let k = s % 24;
    let lan_d = 250_000 * (1 + k % 5);
    let wan_d = 500_000 * (1 + (k / 3) % 4);
    let other = 20_000 * (k % 3);
    let row = crate::netetw::NetRow { pid: 0, create: 0, down: lan_d + wan_d + other, up: 60_000 * (1 + k % 4), lan_down: lan_d, lan_up: 20_000, wan_down: wan_d, wan_up: 40_000 * (1 + k % 4) };
    let mut rows = vec![row];
    let mut live: Vec<(u32, i64)> = ids.iter().filter(|x| *x.0 > 4).map(|(&p, &c)| (p, c)).collect();
    live.sort_unstable();
    for (i, &(pid, create)) in live.iter().take(4).enumerate() {
        let n = 1 + i as u64;
        rows.push(crate::netetw::NetRow { pid, create, down: 90_000 * n + 10_000, up: 20_000 * n, lan_down: 40_000 * n, lan_up: 10_000 * n, wan_down: 50_000 * n, wan_up: 10_000 * n });
    }
    Feed { status: Status::Live, rows, lost: 0, unattributed: 0, bad: 0, iv: Some(crate::netetw::Interval { id: s + 1, end_ms: now_ms, span_ms: 1000 }), conn: Default::default() }
}

/// Text of one graph column for the hover readout: "—" for a gap, never 0.
pub fn rate_text(v: f32) -> String {
    if !v.is_finite() {
        "—".into()
    } else if v >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netetw::{Interval, NetRow};

    fn row(down: u64, up: u64, lan: (u64, u64), wan: (u64, u64)) -> NetRow {
        NetRow { pid: 10, create: 5, down, up, lan_down: lan.0, lan_up: lan.1, wan_down: wan.0, wan_up: wan.1 }
    }

    fn live(id: u64, end_ms: u64, span_ms: u64, rows: Vec<NetRow>) -> Feed {
        Feed { status: Status::Live, rows, lost: 0, unattributed: 0, bad: 0, iv: Some(Interval { id, end_ms, span_ms }), conn: Default::default() }
    }

    fn status(s: Status) -> Feed {
        Feed { status: s, ..Feed::OFF }
    }

    fn finite(v: &[f32]) -> usize {
        v.iter().filter(|x| x.is_finite()).count()
    }

    #[test]
    fn bytes_become_megabits() {
        assert_eq!(mbps(0), 0.0);
        assert_eq!(mbps(125_000), 1.0);
        assert_eq!(mbps(12_500_000), 100.0);
        let t = current(&live(1, 1000, 1000, vec![row(250_000, 125_000, (250_000, 0), (0, 125_000))]), View::Wan).unwrap();
        assert_eq!(t, Rates { down: 0.0, up: 1.0 });
    }

    #[test]
    fn empty_history_is_all_gaps() {
        let h = ScopeHist::default();
        let s = h.series();
        for v in [View::All, View::Lan, View::Wan] {
            let (d, u) = s.of(v);
            assert_eq!((d.len(), u.len()), (HIST, HIST));
            assert_eq!(finite(d) + finite(u), 0);
        }
    }

    #[test]
    fn one_interval_per_second_lands_in_its_own_slot() {
        let mut h = ScopeHist::default();
        for k in 1..=5u64 {
            let f = live(k, k * 1000 + 40, 1000, vec![row(125_000 * k, 0, (125_000 * k, 0), (0, 0))]);
            h.observe(k * 1000 + 40, &f);
        }
        let s = h.series();
        let (d, u) = s.of(View::All);
        assert_eq!(&d[HIST - 5..], &[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(&u[HIST - 5..], &[0.0; 5], "a measured zero is a zero");
        assert_eq!(finite(d), 5, "nothing before the first sample");
        assert_eq!(s.of(View::Lan).0[HIST - 1], 5.0);
        assert_eq!(s.of(View::Wan).0[HIST - 1], 0.0, "a scope with no traffic is 0 while measured");
    }

    #[test]
    fn cached_feed_and_fast_ticks_never_duplicate_a_second() {
        let mut h = ScopeHist::default();
        let f = live(1, 2050, 1000, vec![row(125_000, 125_000, (125_000, 0), (0, 125_000))]);
        for t in [2050, 2250, 2450, 2650, 2850] {
            h.observe(t, &f);
        }
        assert_eq!(finite(h.series().of(View::All).0), 1);
    }

    #[test]
    fn a_skipped_second_is_a_gap_not_a_zero_and_not_joined() {
        let mut h = ScopeHist::default();
        h.observe(1000, &live(1, 1000, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        h.observe(2000, &live(2, 2000, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        // 3 s feed unreadable (needs a baseline after a stall), then a normal interval
        h.observe(3000, &status(Status::Collecting));
        h.observe(4000, &status(Status::Collecting));
        h.observe(5000, &live(3, 5000, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        let d = h.series().all.0;
        let t = &d[HIST - 5..];
        assert_eq!((t[0], t[1], t[4]), (1.0, 1.0, 1.0));
        assert!(t[2].is_nan() && t[3].is_nan(), "{t:?}");
    }

    #[test]
    fn a_long_interval_fills_only_the_seconds_it_measured() {
        let mut h = ScopeHist::default();
        h.observe(10_000, &live(1, 10_000, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        // sampler stalled 3 s: one interval averages 3 s ending at 13 s
        h.observe(13_000, &live(2, 13_000, 3000, vec![row(250_000, 0, (0, 0), (0, 0))]));
        let d = h.series().all.0;
        assert_eq!(&d[HIST - 4..], &[1.0, 2.0, 2.0, 2.0]);
        assert_eq!(finite(&d), 4);
    }

    #[test]
    fn a_second_already_measured_is_not_overwritten() {
        let mut h = ScopeHist::default();
        h.observe(10_050, &live(1, 10_050, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        h.observe(10_990, &live(2, 10_990, 940, vec![row(1_250_000, 0, (0, 0), (0, 0))]));
        let d = h.series().all.0;
        assert_eq!(d[HIST - 1], 1.0);
        assert_eq!(finite(&d), 1);
    }

    #[test]
    fn unreadable_missing_and_error_states_are_gaps() {
        let mut bad = live(2, 2000, 1000, Vec::new());
        bad.bad = 3;
        for (k, f) in [status(Status::NeedsAdmin), status(Status::Unavailable("x".into())), status(Status::Collecting), bad, live(3, 5000, 1000, Vec::new())].into_iter().enumerate() {
            let mut h = ScopeHist::default();
            h.observe((k as u64 + 1) * 1000, &f);
            let s = h.series();
            let n = finite(s.of(View::All).0);
            // the last feed is a real idle second: a measured zero
            assert_eq!(n, usize::from(k == 4), "feed {k}: {f:?}");
        }
    }

    #[test]
    fn nonfinite_never_enters_and_totals_saturate() {
        let mut h = ScopeHist::default();
        let f = live(1, 1000, 1000, vec![row(u64::MAX, u64::MAX, (u64::MAX, 0), (0, u64::MAX)), row(u64::MAX, 5, (1, 1), (1, 1))]);
        h.observe(1000, &f);
        let s = h.series();
        for v in [View::All, View::Lan, View::Wan] {
            let (d, u) = s.of(v);
            assert!(d.iter().chain(u).all(|x| x.is_nan() || x.is_finite()));
            assert!(d[HIST - 1].is_finite() && u[HIST - 1].is_finite());
        }
        assert_eq!(rate_text(f32::NAN), "—");
        assert_eq!(rate_text(f32::INFINITY), "—");
        assert_eq!(rate_text(0.0), "0.0");
        assert_eq!(rate_text(123.4), "123");
    }

    #[test]
    fn session_restart_shows_a_gap_and_off_forgets_everything() {
        let mut h = ScopeHist::default();
        h.observe(1000, &live(1, 1000, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        h.observe(2000, &live(2, 2000, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        // session died: Unavailable, then a fresh session: Collecting baseline, then Live with a new id
        h.observe(3000, &status(Status::Unavailable("ended".into())));
        h.observe(4000, &status(Status::Collecting));
        h.observe(5000, &live(3, 5000, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        let d = h.series().all.0;
        assert_eq!(&d[HIST - 5..HIST - 3], &[1.0, 1.0]);
        assert!(d[HIST - 3].is_nan() && d[HIST - 2].is_nan());
        assert_eq!(d[HIST - 1], 1.0);
        // Network detail closed for the grace period: feed Off drops the history
        h.observe(6000, &status(Status::Off));
        assert_eq!(finite(&h.series().all.0), 0);
        // reopened a long time later: no old samples come back
        h.observe(900_000, &status(Status::Collecting));
        assert_eq!(finite(&h.series().all.0), 0);
    }

    #[test]
    fn a_long_absence_blanks_the_window_and_a_clock_step_back_is_ignored() {
        let mut h = ScopeHist::default();
        h.observe(1000, &live(1, 1000, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        h.observe(100_000, &status(Status::Collecting));
        assert_eq!(finite(&h.series().all.0), 0, "everything is older than 60 s");
        h.observe(100_000, &live(2, 100_000, 1000, vec![row(125_000, 0, (0, 0), (0, 0))]));
        h.observe(50_000, &status(Status::Collecting));
        assert_eq!(h.series().all.0[HIST - 1], 1.0, "an earlier clock never rewrites the newest slot");
        h.observe(99_000, &live(3, 99_000, 1000, vec![row(250_000, 0, (0, 0), (0, 0))]));
        assert!(h.series().all.0[HIST - 2].is_nan(), "backward live observation must not fill older gaps");
        assert_eq!(h.last_iv, 2, "rejected observation must not consume its interval id");
        h.observe(101_000, &live(3, 101_000, 1000, vec![row(250_000, 0, (0, 0), (0, 0))]));
        assert_eq!(h.series().all.0[HIST - 1], 2.0);
    }

    #[test]
    fn window_slides_one_slot_per_second() {
        let mut h = ScopeHist::default();
        for k in 1..=70u64 {
            h.observe(k * 1000, &live(k, k * 1000, 1000, vec![row(125_000 * k, 0, (0, 0), (0, 0))]));
        }
        let d = h.series().all.0;
        assert_eq!(d.len(), HIST);
        assert_eq!(d[0], 11.0);
        assert_eq!(d[HIST - 1], 70.0);
        assert!(d.windows(2).all(|w| w[1] == w[0] + 1.0));
    }

    #[test]
    fn scopes_are_independent_series_and_graph_views_map_to_them() {
        let mut h = ScopeHist::default();
        h.observe(1000, &live(1, 1000, 1000, vec![row(375_000, 125_000, (125_000, 0), (250_000, 125_000))]));
        let s = h.series();
        assert_eq!((s.of(View::All).0[HIST - 1], s.of(View::All).1[HIST - 1]), (3.0, 1.0));
        assert_eq!((s.of(View::Lan).0[HIST - 1], s.of(View::Lan).1[HIST - 1]), (1.0, 0.0));
        assert_eq!((s.of(View::Wan).0[HIST - 1], s.of(View::Wan).1[HIST - 1]), (2.0, 1.0));
        assert_eq!(Graph::default(), Graph::Adapter);
        assert_eq!(Graph::Adapter.view(), None, "the adapter graph reads no ETW scope");
        assert_eq!([Graph::Adapter, Graph::All, Graph::Lan, Graph::Wan].map(Graph::view), [None, Some(View::All), Some(View::Lan), Some(View::Wan)]);
    }

    #[test]
    fn current_needs_a_valid_measurement() {
        assert_eq!(current(&status(Status::Collecting), View::All), None);
        assert_eq!(current(&Feed::OFF, View::Lan), None);
        let mut f = live(1, 1000, 1000, Vec::new());
        assert_eq!(current(&f, View::Wan), Some(Rates { down: 0.0, up: 0.0 }), "idle is a real zero");
        f.bad = 1;
        assert_eq!(current(&f, View::Wan), None, "unreadable events with no rows is not idle");
        f.iv = None;
        f.rows.push(row(125_000, 0, (0, 0), (125_000, 0)));
        assert_eq!(current(&f, View::Wan), None, "no completed interval, no number");
    }
}
