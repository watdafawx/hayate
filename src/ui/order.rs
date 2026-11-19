//! Stable row order for live lists (Vitals "TOP BY ..." panels, Processes).
//!
//! Values stay live every sampler tick. Only the ORDER is gated:
//!
//! * it is recomputed at most every [`REORDER_MS`] (5 s);
//! * not while the pointer is in the list, while the user is scrolling,
//!   dragging, typing a row move or has a menu open, and not until
//!   [`THAW_MS`] (2 s) after the last of those ended;
//! * an explicit change (sort column/direction, filter, Apps/Processes
//!   toggle, ...) is a different `spec`: it applies at once and starts a new
//!   stable order;
//! * a row that exits while the order is held keeps its slot as
//!   [`Slot::Gone`] (the caller draws it dim and inert); new rows wait for the
//!   next apply instead of pushing the rows under the pointer around.
//!
//! One `Stable` per list. Time is passed in (monotonic ms), so the whole
//! policy is unit-testable.

use std::hash::Hash;

pub const REORDER_MS: u64 = 5_000;
pub const THAW_MS: u64 = 2_000;
/// Not arranged for this long (tab switched, window hidden): the old order is
/// stale, start over.
const STALE_MS: u64 = 3_000;

/// Monotonic milliseconds for the UI thread.
pub fn now_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Slot<K, T> {
    Live(T),
    /// the row's process left while the order was held: keep the slot, show it as gone
    Gone(K),
}

pub struct Arranged<K, T> {
    pub slots: Vec<Slot<K, T>>,
    /// the order was recomputed from the live sort in this call
    pub applied: bool,
    /// `applied` because of a new `spec` (explicit change), not the timer
    pub explicit: bool,
}

pub struct Stable<K> {
    order: Vec<K>,
    spec: u64,
    applied_at: u64,
    last_call: u64,
    has: bool,
    inside: bool,
    /// drag / menu in progress
    active: bool,
    /// frozen until (ms) after the last interaction ended
    quiet_until: u64,
}

impl<K> Default for Stable<K> {
    fn default() -> Self {
        Stable { order: Vec::new(), spec: 0, applied_at: 0, last_call: 0, has: false, inside: false, active: false, quiet_until: 0 }
    }
}

impl<K: Clone + Eq + Hash> Stable<K> {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pointer is (not) inside the list. Leaving starts the thaw delay.
    pub fn pointer(&mut self, now: u64, inside: bool) {
        if self.inside && !inside {
            self.quiet_until = self.quiet_until.max(now + THAW_MS);
        }
        self.inside = inside;
    }

    /// A drag or a menu is (not) in progress. Ending starts the thaw delay.
    pub fn active(&mut self, now: u64, on: bool) {
        if self.active && !on {
            self.quiet_until = self.quiet_until.max(now + THAW_MS);
        }
        self.active = on;
    }

    /// A point interaction (wheel notch, key, click): freezes until THAW_MS after it.
    pub fn touch(&mut self, now: u64) {
        self.quiet_until = self.quiet_until.max(now + THAW_MS);
    }

    pub fn frozen(&self, now: u64) -> bool {
        self.inside || self.active || now < self.quiet_until
    }

    /// True when the next `arrange` would recompute the order (the UI uses it
    /// to schedule a repaint even when no value changed).
    pub fn due(&self, now: u64) -> bool {
        self.has && !self.frozen(now) && now.saturating_sub(self.applied_at) >= REORDER_MS
    }

    pub fn keys(&self) -> &[K] {
        &self.order
    }

    /// Nothing is on screen from this list (no rows, no ghosts), so there is no
    /// position to protect: the next `arrange` adopts the live order at once, even
    /// under the pointer or a menu. Without it a list first arranged while empty
    /// (a hidden window's snapshot has no rows) would hold that empty order.
    /// A list that still shows ghosts is never "nothing": callers check first.
    pub fn restart_if_empty(&mut self) {
        if self.order.is_empty() {
            self.has = false;
        }
    }

    /// Produce the rows to show.
    ///
    /// * `spec`: signature of the explicit sort/filter/group state. A change applies at once.
    /// * `live`: keys in the order the current sort wants (best first), already limited as the list wants.
    /// * `get`: current payload of a key; `None` = that row no longer exists.
    ///
    /// While held, rows keep the held order with live payloads; missing ones become `Gone`.
    pub fn arrange<T>(&mut self, now: u64, spec: u64, live: Vec<K>, mut get: impl FnMut(&K) -> Option<T>) -> Arranged<K, T> {
        // A skipped/slow paint must never override an active interaction hold.
        let stale = now.saturating_sub(self.last_call) > STALE_MS && !self.frozen(now);
        self.last_call = now;
        let explicit = self.has && spec != self.spec;
        let timer = self.has && !self.frozen(now) && now.saturating_sub(self.applied_at) >= REORDER_MS;
        let apply = !self.has || stale || explicit || timer;
        if apply {
            self.order = live;
            self.spec = spec;
            self.applied_at = now;
            self.has = true;
            if stale || explicit {
                // a fresh list owes the pointer nothing: no leftover thaw delay
                self.quiet_until = 0;
            }
        }
        let slots = self
            .order
            .iter()
            .map(|k| match get(k) {
                Some(t) => Slot::Live(t),
                None => Slot::Gone(k.clone()),
            })
            .collect();
        Arranged { slots, applied: apply, explicit: apply && (explicit || stale) }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_paint_does_not_reorder_under_pointer_or_menu() {
        for menu in [false, true] {
            let mut s = Stable::<u32>::new();
            arr(&mut s, 0, 1, &[1, 2], &[1, 2]);
            s.pointer(100, !menu);
            s.active(100, menu);
            assert_eq!(arr(&mut s, 10_000, 1, &[2, 1], &[1, 2]), vec![Slot::Live(10), Slot::Live(20)]);
            s.pointer(10_001, false);
            s.active(10_001, false);
            assert_eq!(arr(&mut s, 11_000, 1, &[2, 1], &[1, 2]), vec![Slot::Live(10), Slot::Live(20)]);
            assert_eq!(arr(&mut s, 12_001, 1, &[2, 1], &[1, 2]), vec![Slot::Live(20), Slot::Live(10)]);
        }
    }

    fn arr(s: &mut Stable<u32>, now: u64, spec: u64, live: &[u32], alive: &[u32]) -> Vec<Slot<u32, u32>> {
        let alive = alive.to_vec();
        s.arrange(now, spec, live.to_vec(), move |k| alive.contains(k).then_some(*k * 10)).slots
    }
    /// call `arrange` once a second like the sampler tick does, up to and including `to`
    fn run(s: &mut Stable<u32>, from: u64, to: u64, spec: u64, live: &[u32], alive: &[u32]) -> Vec<Slot<u32, u32>> {
        let mut t = from;
        loop {
            let r = arr(s, t, spec, live, alive);
            if t >= to {
                return r;
            }
            t = (t + 1000).min(to);
        }
    }
    fn live(v: &[u32]) -> Vec<Slot<u32, u32>> {
        v.iter().map(|k| Slot::Live(*k * 10)).collect()
    }

    #[test]
    fn first_call_adopts_live_order() {
        let mut s = Stable::new();
        assert_eq!(arr(&mut s, 0, 1, &[3, 1, 2], &[1, 2, 3]), live(&[3, 1, 2]));
    }

    #[test]
    fn order_updates_at_most_every_five_seconds() {
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2, 3], &[1, 2, 3]);
        // ranking flips every second: the shown order does not
        for t in 1..5 {
            assert_eq!(arr(&mut s, t * 1000, 1, &[3, 2, 1], &[1, 2, 3]), live(&[1, 2, 3]), "t={t}s");
        }
        assert_eq!(arr(&mut s, 5000, 1, &[3, 2, 1], &[1, 2, 3]), live(&[3, 2, 1]));
        assert_eq!(arr(&mut s, 6000, 1, &[1, 2, 3], &[1, 2, 3]), live(&[3, 2, 1]), "next apply is 5 s after the last");
        assert_eq!(arr(&mut s, 10_000, 1, &[1, 2, 3], &[1, 2, 3]), live(&[1, 2, 3]));
    }

    #[test]
    fn values_stay_live_while_order_is_held() {
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2], &[1, 2]);
        let a = s.arrange(1000, 1, vec![2, 1], |k| Some(*k * 100));
        assert!(!a.applied);
        assert_eq!(a.slots, vec![Slot::Live(100), Slot::Live(200)], "held order, fresh payloads");
    }

    #[test]
    fn pointer_inside_freezes_and_exit_grace_is_two_seconds() {
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2], &[1, 2]);
        s.pointer(4000, true);
        // way past 5 s, pointer still in: held
        assert_eq!(run(&mut s, 1000, 20_000, 1, &[2, 1], &[1, 2]), live(&[1, 2]));
        assert!(s.frozen(20_000));
        s.pointer(21_000, false);
        assert!(s.frozen(22_999), "no earlier than 2 s after leaving");
        assert_eq!(arr(&mut s, 22_999, 1, &[2, 1], &[1, 2]), live(&[1, 2]));
        assert!(!s.frozen(23_000));
        assert!(s.due(23_000));
        assert_eq!(arr(&mut s, 23_000, 1, &[2, 1], &[1, 2]), live(&[2, 1]));
        assert!(!s.due(23_001));
    }

    #[test]
    fn re_entering_cancels_the_thaw() {
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2], &[1, 2]);
        s.pointer(1000, true);
        s.pointer(2000, false);
        s.pointer(3000, true);
        s.pointer(9000, false);
        assert!(s.frozen(10_999));
        assert!(!s.frozen(11_000));
    }

    #[test]
    fn scroll_drag_and_menu_freeze_too() {
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2], &[1, 2]);
        // wheel notch at 9 s: frozen until 11 s even though 5 s passed
        run(&mut s, 1000, 4000, 1, &[1, 2], &[1, 2]);
        run(&mut s, 5000, 8000, 1, &[1, 2], &[1, 2]);
        s.touch(9000);
        assert_eq!(arr(&mut s, 10_000, 1, &[2, 1], &[1, 2]), live(&[1, 2]));
        assert_eq!(arr(&mut s, 11_000, 1, &[2, 1], &[1, 2]), live(&[2, 1]));
        // menu open for 30 s: nothing moves under it, 2 s after it closes order may update
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2], &[1, 2]);
        s.active(1000, true);
        assert_eq!(run(&mut s, 2000, 31_000, 1, &[2, 1], &[1, 2]), live(&[1, 2]));
        s.active(32_000, false);
        assert_eq!(arr(&mut s, 33_000, 1, &[2, 1], &[1, 2]), live(&[1, 2]));
        assert_eq!(arr(&mut s, 34_000, 1, &[2, 1], &[1, 2]), live(&[2, 1]));
    }

    #[test]
    fn explicit_change_applies_immediately_and_even_while_frozen() {
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2, 3], &[1, 2, 3]);
        s.pointer(500, true);
        let a = s.arrange(600, 2, vec![3, 2, 1], |k| Some(*k));
        assert!(a.applied && a.explicit);
        assert_eq!(a.slots, vec![Slot::Live(3), Slot::Live(2), Slot::Live(1)]);
        // and that order is now the stable one: held again
        let a = s.arrange(1600, 2, vec![1, 2, 3], |k| Some(*k));
        assert!(!a.applied);
        assert_eq!(a.slots[0], Slot::Live(3));
    }

    #[test]
    fn exited_rows_keep_their_slot_while_held_and_new_rows_wait() {
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2, 3], &[1, 2, 3]);
        s.pointer(100, true);
        // 2 exited, 9 is new and now the busiest
        let got = arr(&mut s, 2000, 1, &[9, 1, 3], &[1, 3, 9]);
        assert_eq!(got, vec![Slot::Live(10), Slot::Gone(2), Slot::Live(30)], "slot of 2 reserved, 9 not inserted");
        // keeps holding
        let got = run(&mut s, 3000, 30_000, 1, &[9, 1, 3], &[1, 3, 9]);
        assert_eq!(got, vec![Slot::Live(10), Slot::Gone(2), Slot::Live(30)]);
        // thaw: ghost goes, new row integrates in sort order
        s.pointer(31_000, false);
        let got = arr(&mut s, 33_000, 1, &[9, 1, 3], &[1, 3, 9]);
        assert_eq!(got, vec![Slot::Live(90), Slot::Live(10), Slot::Live(30)]);
    }

    #[test]
    fn exits_without_interaction_also_wait_for_the_timer() {
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2, 3], &[1, 2, 3]);
        assert_eq!(arr(&mut s, 1000, 1, &[1, 3], &[1, 3]), vec![Slot::Live(10), Slot::Gone(2), Slot::Live(30)]);
        assert_eq!(arr(&mut s, 5000, 1, &[1, 3], &[1, 3]), live(&[1, 3]));
    }

    #[test]
    fn stale_list_starts_over() {
        let mut s = Stable::new();
        arr(&mut s, 0, 1, &[1, 2], &[1, 2]);
        s.pointer(100, true);
        // list was not painted for a while (other tab): old pointer state and order are void
        s.pointer(200, false);
        let a = s.arrange(60_000, 1, vec![2, 1], |k| Some(*k));
        assert!(a.applied && a.explicit);
        assert_eq!(a.slots[0], Slot::Live(2));
        assert!(!s.frozen(60_000), "fresh list: no thaw delay owed");
    }

    #[test]
    fn empty_order_adopts_the_first_rows_even_when_held_but_never_displaces_shown_ones() {
        for hold in ["pointer", "menu"] {
            let mut s = Stable::new();
            assert!(arr(&mut s, 0, 1, &[], &[]).is_empty());
            s.pointer(0, hold == "pointer");
            s.active(0, hold == "menu");
            // without the restart the empty order would be held until the thaw
            assert!(arr(&mut s, 1000, 1, &[1], &[1]).is_empty(), "plain arrange keeps an empty order held");
            s.restart_if_empty();
            assert_eq!(arr(&mut s, 2000, 1, &[2, 1], &[1, 2]), live(&[2, 1]), "{hold}: nothing shown, adopt");
            // a list with rows (or ghosts) is not restarted: the held slots stay
            s.restart_if_empty();
            assert_eq!(arr(&mut s, 3000, 1, &[1, 2, 3], &[1, 2, 3]), live(&[2, 1]), "{hold}: new row waits");
            let mut s = Stable::new();
            arr(&mut s, 0, 1, &[1], &[1]);
            s.pointer(1, true);
            s.restart_if_empty();
            assert_eq!(arr(&mut s, 2000, 1, &[2], &[2]), vec![Slot::Gone(1)], "exited row keeps its slot, new one waits");
        }
    }

    #[test]
    fn due_tells_the_ui_to_repaint_for_a_quiet_list() {
        let mut s = Stable::new();
        assert!(!s.due(10_000), "nothing arranged yet");
        arr(&mut s, 0, 1, &[1], &[1]);
        assert!(!s.due(4999));
        assert!(s.due(5000));
        s.pointer(5000, true);
        assert!(!s.due(9000));
    }
}
