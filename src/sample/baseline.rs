//! Baseline ("usual level") of the system-wide TCP counters: failed connects per minute, TCP
//! resets per minute and retransmit %. Pure and unit-tested; the Network health verdict and
//! tiles judge a counter against this machine's own usual level instead of a fixed number.
//!
//! Nothing new is read. The counters are the `GetTcpStatisticsEx` values `sample::nethealth`
//! already reads once a second while the Network detail is visible. The sampler diffs them over
//! one minute and keeps the last 60 one-minute samples (3 counters x 60 x 4 bytes = 720 bytes).
//! History therefore only accrues while that view is shown, with the same gap-not-invented-data
//! rule as the Sensors ring: a hidden detail, a read gap or a counter reset drops the minute
//! being measured, never fills it. Samples already in the ring stay (they are real) but a gap
//! is never bridged. Nothing is written to disk: a restart starts a new ring.
//!
//! `usual` is the median of the ring and needs `MIN_SAMPLES` samples; before that the caller
//! shows a muted "learning" note and falls back to the fixed thresholds.

use std::collections::VecDeque;
#[cfg(debug_assertions)]
use std::sync::OnceLock;
use std::time::Instant;

use super::nethealth::TcpCtr;

/// Samples kept (one per minute): the last hour.
pub const RING: usize = 60;
/// Samples needed before "usual" means anything.
pub const MIN_SAMPLES: usize = 15;
/// Seconds one sample covers.
pub const SAMPLE_SECS: f64 = 60.0;
/// Test-only override of `SAMPLE_SECS` (smoke tests): shortens the minute so the 15 sample
/// minimum is reached in seconds. Compiled in debug builds only: release always uses `SAMPLE_SECS`.
#[cfg(debug_assertions)]
pub const ENV_SAMPLE_SECS: &str = "BLINK_NET_BASELINE_SECS";

/// Seconds per sample: `SAMPLE_SECS`, or the test override (1..=3600).
#[cfg(debug_assertions)]
pub fn sample_secs() -> f64 {
    static S: OnceLock<f64> = OnceLock::new();
    *S.get_or_init(|| std::env::var(ENV_SAMPLE_SECS).ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| (1.0..=3600.0).contains(v)).unwrap_or(SAMPLE_SECS))
}

/// Release: no environment hook.
#[cfg(not(debug_assertions))]
pub fn sample_secs() -> f64 {
    SAMPLE_SECS
}

/// One minute: failed connects per minute, resets per minute, % of sent segments retransmitted.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    pub fail: f32,
    pub rst: f32,
    pub retr: f32,
}

/// The usual level of each counter (median of the ring).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usual {
    pub fail: f64,
    pub rst: f64,
    pub retr: f64,
}

/// The ring as three series, oldest first, for the tile sparklines.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Series {
    pub fail: Vec<f32>,
    pub rst: Vec<f32>,
    pub retr: Vec<f32>,
}

impl Series {
    pub fn len(&self) -> usize {
        self.fail.len()
    }
}

/// Median of `v` (mean of the middle two for an even count); 0 for none.
pub fn median(v: &[f32]) -> f64 {
    let mut s: Vec<f32> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return 0.0;
    }
    s.sort_by(|a, b| a.total_cmp(b));
    let n = s.len();
    if n % 2 == 1 { s[n / 2] as f64 } else { (s[n / 2 - 1] as f64 + s[n / 2] as f64) / 2.0 }
}

/// [WARN, HOT] for a counter whose usual level is `usual`: WARN at max(3 x usual, usual + 10/min)
/// and HOT at max(8 x usual, usual + 40/min); for the retransmit % the margins are 1.5 and 6 points.
pub fn warn_hot(usual: f64, pct: bool) -> [f64; 2] {
    let u = if usual.is_finite() { usual.max(0.0) } else { 0.0 };
    if pct {
        [(3.0 * u).max(u + 1.5), (8.0 * u).max(u + 6.0)]
    } else {
        [(3.0 * u).max(u + 10.0), (8.0 * u).max(u + 40.0)]
    }
}

#[derive(Default)]
pub struct Ring {
    s: VecDeque<Sample>,
    /// where the minute being measured started
    anchor: Option<(Instant, TcpCtr)>,
}

impl Ring {
    pub fn len(&self) -> usize {
        self.s.len()
    }

    /// Add one finished minute.
    pub fn push(&mut self, s: Sample) {
        self.s.push_back(s);
        while self.s.len() > RING {
            self.s.pop_front();
        }
    }

    /// The detail was hidden, or a read was missed: the minute being measured is dropped.
    pub fn gap(&mut self) {
        self.anchor = None;
    }

    /// One read of the counters. When `interval_s` has passed since the anchor, the growth over
    /// that span becomes a sample and the read is the new anchor. Counters that went back (an
    /// adapter or stack reset) drop the minute instead of inventing a rate.
    pub fn observe(&mut self, at: Instant, c: TcpCtr, interval_s: f64) {
        let Some((t0, c0)) = self.anchor else {
            self.anchor = Some((at, c));
            return;
        };
        let span = at.saturating_duration_since(t0).as_secs_f64();
        if span < interval_s {
            return;
        }
        self.anchor = Some((at, c));
        if c.fails < c0.fails || c.resets < c0.resets || c.retrans < c0.retrans || c.out_segs < c0.out_segs {
            return;
        }
        let per_min = |a: u64, b: u64| ((a - b) as f64 / span * 60.0) as f32;
        let out = (c.out_segs - c0.out_segs) as f64;
        self.push(Sample { fail: per_min(c.fails, c0.fails), rst: per_min(c.resets, c0.resets), retr: if out > 0.0 { ((c.retrans - c0.retrans) as f64 / out * 100.0) as f32 } else { 0.0 } });
    }

    /// Median of each counter, once there are `MIN_SAMPLES`.
    pub fn usual(&self) -> Option<Usual> {
        if self.s.len() < MIN_SAMPLES {
            return None;
        }
        let col = |f: fn(&Sample) -> f32| -> f64 { median(&self.s.iter().map(f).collect::<Vec<_>>()) };
        Some(Usual { fail: col(|s| s.fail), rst: col(|s| s.rst), retr: col(|s| s.retr) })
    }

    pub fn series(&self) -> Series {
        Series { fail: self.s.iter().map(|s| s.fail).collect(), rst: self.s.iter().map(|s| s.rst).collect(), retr: self.s.iter().map(|s| s.retr).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn s(f: f32) -> Sample {
        Sample { fail: f, rst: f / 2.0, retr: 0.5 }
    }

    #[test]
    fn median_odd_even_and_empty() {
        assert_eq!(median(&[]), 0.0);
        assert_eq!(median(&[5.0]), 5.0);
        assert_eq!(median(&[9.0, 1.0, 4.0]), 4.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 10.0]), 2.5);
        assert_eq!(median(&[f32::NAN, 2.0, 4.0]), 3.0, "NaN is ignored");
    }

    #[test]
    fn warn_and_hot_follow_the_mock_formula() {
        // quiet machine: the absolute margin wins
        assert_eq!(warn_hot(0.0, false), [10.0, 40.0]);
        assert_eq!(warn_hot(2.0, false), [12.0, 42.0]);
        // busy machine: the multiple wins
        assert_eq!(warn_hot(84.0, false), [252.0, 672.0]);
        assert_eq!(warn_hot(12.0, false), [36.0, 96.0]);
        // retransmit percent: points, not per-minute counts
        assert_eq!(warn_hot(0.2, true), [1.7, 6.2]);
        assert_eq!(warn_hot(2.0, true), [6.0, 16.0]);
        // junk in, sane out
        assert_eq!(warn_hot(f64::NAN, false), [10.0, 40.0]);
        assert_eq!(warn_hot(-5.0, false), [10.0, 40.0]);
    }

    #[test]
    fn usual_needs_fifteen_samples() {
        let mut r = Ring::default();
        for i in 0..MIN_SAMPLES - 1 {
            r.push(s(i as f32));
            assert_eq!(r.usual(), None, "{} samples is still learning", r.len());
        }
        r.push(s(100.0));
        let u = r.usual().unwrap();
        // 0..=13 and 100: the median is the 8th value, 7
        assert_eq!(u.fail, 7.0);
        assert_eq!(u.rst, 3.5);
        assert_eq!(u.retr, 0.5);
    }

    #[test]
    fn ring_keeps_the_last_hour_only() {
        let mut r = Ring::default();
        for i in 0..100 {
            r.push(s(i as f32));
        }
        assert_eq!(r.len(), RING);
        let sr = r.series();
        assert_eq!((sr.len(), sr.fail[0], sr.fail[RING - 1]), (RING, 40.0, 99.0));
    }

    fn ctr(f: u64, r: u64, re: u64, out: u64) -> TcpCtr {
        TcpCtr { fails: f, resets: r, retrans: re, out_segs: out }
    }

    #[test]
    fn a_sample_is_the_growth_over_one_interval() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = Ring::default();
        r.observe(at(0), ctr(100, 50, 10, 1000), 60.0);
        r.observe(at(30), ctr(110, 55, 20, 2000), 60.0);
        assert_eq!(r.len(), 0, "not a minute yet");
        r.observe(at(60), ctr(160, 80, 30, 3000), 60.0);
        assert_eq!(r.len(), 1);
        assert_eq!(r.s[0], Sample { fail: 60.0, rst: 30.0, retr: 1.0 });
        // the next minute starts from the read that closed this one
        r.observe(at(120), ctr(160, 80, 30, 3000), 60.0);
        assert_eq!(r.s[1], Sample { fail: 0.0, rst: 0.0, retr: 0.0 }, "no segments sent: 0 %, not NaN");
        // a span longer than the interval is scaled to a minute
        r.observe(at(210), ctr(250, 80, 30, 3000), 60.0);
        assert_eq!(r.s[2].fail, 60.0);
    }

    #[test]
    fn a_gap_or_a_counter_reset_drops_the_minute_instead_of_inventing_it() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = Ring::default();
        r.observe(at(0), ctr(0, 0, 0, 0), 60.0);
        r.gap(); // the detail was hidden
        r.observe(at(300), ctr(900, 0, 0, 0), 60.0);
        assert_eq!(r.len(), 0, "the 5 minute gap is not one sample of 180/min");
        r.observe(at(360), ctr(930, 0, 0, 0), 60.0);
        assert_eq!(r.len(), 1);
        // counters went back (stack reset): nothing is recorded, the read re-anchors
        r.observe(at(420), ctr(5, 0, 0, 0), 60.0);
        assert_eq!(r.len(), 1);
        r.observe(at(480), ctr(35, 0, 0, 0), 60.0);
        assert_eq!(r.len(), 2);
        assert_eq!(r.s[1].fail, 30.0);
    }
}
