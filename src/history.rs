//! One hour of per-second history for the History tab, kept in RAM. The
//! sampler records every tick (shown or hidden); the UI takes a snapshot when
//! the tab is painted. ~3600 f32 per series, about 14 KB each.

use std::collections::VecDeque;
use std::sync::Mutex;

pub const CAP: usize = 3600;

#[derive(Clone)]
pub struct Series {
    pub key: String,
    pub title: String,
    pub unit: &'static str,
    pub color: u32,
    /// fixed axis maximum (percentages, RAM size); None = scale to the data
    pub max: Option<f32>,
    pub data: VecDeque<f32>,
    /// Temperature ticks, including missing readings; History keeps its existing data.
    ticks: Option<VecDeque<f32>>,
}

impl Series {
    #[cfg(test)]
    pub fn for_test(key: &str, unit: &'static str, d: &[f32]) -> Series {
        Series { key: key.into(), title: key.into(), unit, color: 0, max: None, data: d.iter().copied().collect(), ticks: None }
    }
}

static STORE: Mutex<Vec<Series>> = Mutex::new(Vec::new());

/// Append one sample to a series, creating it on first use.
pub fn record(key: &str, title: &str, unit: &'static str, color: u32, max: Option<f32>, v: f32) {
    let Ok(mut all) = STORE.lock() else { return };
    let i = match all.iter().position(|s| s.key == key) {
        Some(i) => i,
        None => {
            all.push(Series { key: key.into(), title: title.into(), unit, color, max, data: VecDeque::new(), ticks: (unit == "°C").then(VecDeque::new) });
            all.len() - 1
        }
    };
    let s = &mut all[i];
    if s.title != title {
        s.title = title.into(); // e.g. a drive letter changed
    }
    s.max = max;
    if s.data.len() >= CAP {
        s.data.pop_front();
    }
    s.data.push_back(if v.is_finite() { v } else { 0.0 });
    if let Some(ticks) = &mut s.ticks { push_tick(ticks, v); }
}

fn push_tick(ticks: &mut VecDeque<f32>, v: f32) {
    if ticks.len() >= CAP { ticks.pop_front(); }
    ticks.push_back(v);
}

/// Preserve a missing temperature tick without changing the History tab's samples.
pub fn temperature_gap(key: &str) {
    let Ok(mut all) = STORE.lock() else { return };
    if let Some(ticks) = all.iter_mut().find(|s| s.key == key).and_then(|s| s.ticks.as_mut()) {
        push_tick(ticks, f32::NAN);
    }
}

/// Copy for painting. Series keep their creation order.
pub fn snapshot() -> Vec<Series> {
    STORE.lock().map(|s| s.clone()).unwrap_or_default()
}

/// The newest `n` samples (temperature: one-second slots, NaN when missing),
/// oldest first, or `None` when the
/// series does not exist. Copies only that series, unlike `snapshot`.
pub fn tail(key: &str, n: usize) -> Option<Vec<f32>> {
    let all = STORE.lock().ok()?;
    let s = all.iter().find(|s| s.key == key)?;
    let data = s.ticks.as_ref().unwrap_or(&s.data);
    let skip = data.len().saturating_sub(n);
    Some(data.iter().skip(skip).copied().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temperature_recovery_preserves_missing_seconds() {
        let key = "test-temperature-recovery";
        record(key, "Test", "°C", 0, None, 40.0);
        temperature_gap(key);
        temperature_gap(key);
        record(key, "Test", "°C", 0, None, 45.0);
        let ticks = tail(key, 4).unwrap();
        assert_eq!(ticks.len(), 4);
        assert_eq!(ticks[0], 40.0);
        assert!(ticks[1].is_nan() && ticks[2].is_nan());
        assert_eq!(ticks[3], 45.0);
        assert_eq!(snapshot().iter().find(|s| s.key == key).unwrap().data.len(), 2);
    }
}
