//! Catch-up gate: decides which System-log events may toast.
//!
//! When the subscription resumes after a saved bookmark, Windows first
//! delivers everything that happened while Blink was closed. Those events are
//! always logged, but only toast when they fall inside a short, recent gap
//! (the previous session ended < 24 h ago), and only when there are few of
//! them. Anything older (or a whole replayed log) is logged as read, silently.

use super::AlertType;

/// Event this close before Watch started still counts as live.
pub const GRACE_MS: u64 = 60_000;
/// Longest "Blink was closed" gap whose events may still toast.
pub const MAX_GAP_MS: u64 = 24 * 3600 * 1000;
/// Bookmark save time is only throttled-accurate; accept events a bit older.
pub const SLACK_MS: u64 = 5 * 60 * 1000;
/// More caught-up toasts than this in one batch: one summary instead.
pub const BATCH_CAP: usize = 20;
/// Batch ends after this long without another caught-up event.
pub const IDLE_FLUSH_MS: u64 = 1_500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// happened while Watch runs (or just before): toast now
    Live,
    /// happened in the recent gap since the last session: toast, batched
    CatchUp,
    /// older than the last session / gap too long / no bookmark: log only, read
    Old,
}

/// `event_ms` 0 = unknown time (treated as live). `saved_ms` = when the
/// bookmark we resumed from was last saved (None = no valid bookmark).
pub fn classify(event_ms: u64, start_ms: u64, saved_ms: Option<u64>) -> Verdict {
    if event_ms == 0 || event_ms + GRACE_MS >= start_ms {
        return Verdict::Live;
    }
    match saved_ms {
        Some(s) if start_ms.saturating_sub(s) <= MAX_GAP_MS && event_ms + SLACK_MS >= s => Verdict::CatchUp,
        _ => Verdict::Old,
    }
}

/// A toast held back until its catch-up batch is complete.
#[derive(Clone, Debug, PartialEq)]
pub struct Held {
    pub alert: AlertType,
    pub title: String,
    pub text: String,
    pub subject: String,
    pub actor: String,
    pub id: Option<u64>,
}

/// What to show once a batch is complete.
#[derive(Debug, Default, PartialEq)]
pub struct Flush {
    pub toasts: Vec<Held>,
    /// batch too big: (count, dominant alert type) for one summary toast
    pub summary: Option<(usize, AlertType)>,
    /// old events logged without a toast since the last flush
    pub old: usize,
}

pub struct Gate {
    pub start_ms: u64,
    pub saved_ms: Option<u64>,
    held: Vec<Held>,
    old: usize,
    last_ms: u64,
}

impl Gate {
    pub fn new(start_ms: u64, saved_ms: Option<u64>) -> Gate {
        Gate { start_ms, saved_ms, held: Vec::new(), old: 0, last_ms: 0 }
    }

    pub fn classify(&self, event_ms: u64) -> Verdict {
        classify(event_ms, self.start_ms, self.saved_ms)
    }

    pub fn hold(&mut self, now_ms: u64, h: Held) {
        self.held.push(h);
        self.last_ms = now_ms;
    }

    pub fn skip_old(&mut self, now_ms: u64) {
        self.old += 1;
        self.last_ms = now_ms;
    }

    pub fn pending(&self) -> bool {
        !self.held.is_empty() || self.old > 0
    }

    /// Batch is over (idle long enough)?
    pub fn due(&self, now_ms: u64) -> bool {
        self.pending() && now_ms.saturating_sub(self.last_ms) >= IDLE_FLUSH_MS
    }

    /// End the batch: up to BATCH_CAP toasts pass, more become one summary.
    pub fn flush(&mut self) -> Flush {
        let held = std::mem::take(&mut self.held);
        let old = std::mem::take(&mut self.old);
        if held.len() <= BATCH_CAP {
            return Flush { toasts: held, summary: None, old };
        }
        let mut counts: Vec<(AlertType, usize)> = Vec::new();
        for h in &held {
            match counts.iter_mut().find(|(a, _)| *a == h.alert) {
                Some(c) => c.1 += 1,
                None => counts.push((h.alert, 1)),
            }
        }
        let dominant = counts.iter().max_by_key(|(_, n)| *n).map(|(a, _)| *a).unwrap_or(AlertType::ServiceInstalled);
        Flush { toasts: Vec::new(), summary: Some((held.len(), dominant)), old }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 3600 * 1000;
    const START: u64 = 1_794_791_167_000;

    fn held(a: AlertType, n: &str) -> Held {
        Held { alert: a, title: n.into(), text: String::new(), subject: n.into(), actor: String::new(), id: None }
    }

    #[test]
    fn live_events_toast() {
        assert_eq!(classify(START + 5_000, START, None), Verdict::Live);
        assert_eq!(classify(START - 30_000, START, None), Verdict::Live); // within grace
        assert_eq!(classify(0, START, None), Verdict::Live); // unknown time
    }

    #[test]
    fn no_bookmark_means_old() {
        // dev PC 2026-11-16: whole System log (months) replayed, no valid bookmark
        let months_ago = 1_792_222_069_000;
        assert_eq!(classify(months_ago, START, None), Verdict::Old);
        assert_eq!(classify(START - 2 * GRACE_MS, START, None), Verdict::Old);
    }

    #[test]
    fn recent_gap_catches_up() {
        let saved = START - 2 * H; // previous session ended 2 h ago
        assert_eq!(classify(START - H, START, Some(saved)), Verdict::CatchUp);
        assert_eq!(classify(saved - SLACK_MS + 1, START, Some(saved)), Verdict::CatchUp);
        // older than the previous session's end: already seen or replayed
        assert_eq!(classify(saved - SLACK_MS - 1, START, Some(saved)), Verdict::Old);
        assert_eq!(classify(saved - 30 * 24 * H, START, Some(saved)), Verdict::Old);
    }

    #[test]
    fn long_gap_is_old() {
        let saved = START - 25 * H;
        assert_eq!(classify(START - H, START, Some(saved)), Verdict::Old);
        // but live still toasts
        assert_eq!(classify(START + 1, START, Some(saved)), Verdict::Live);
    }

    #[test]
    fn small_batch_passes() {
        let mut g = Gate::new(START, Some(START - H));
        for i in 0..BATCH_CAP {
            g.hold(START + i as u64, held(AlertType::StartTypeChanged, "BITS"));
        }
        g.skip_old(START + 100);
        assert!(!g.due(START + 100 + IDLE_FLUSH_MS - 1));
        assert!(g.due(START + 100 + IDLE_FLUSH_MS));
        let f = g.flush();
        assert_eq!(f.toasts.len(), BATCH_CAP);
        assert_eq!(f.summary, None);
        assert_eq!(f.old, 1);
        assert!(!g.pending());
    }

    #[test]
    fn big_batch_one_summary() {
        let mut g = Gate::new(START, Some(START - H));
        for i in 0..3230 {
            g.hold(START + i, held(AlertType::StartTypeChanged, "IsolationSession"));
        }
        for i in 0..284 {
            g.hold(START + i, held(AlertType::ServiceInstalled, "svc"));
        }
        let f = g.flush();
        assert!(f.toasts.is_empty());
        assert_eq!(f.summary, Some((3514, AlertType::StartTypeChanged)));
        assert_eq!(g.flush(), Flush::default());
    }
}
