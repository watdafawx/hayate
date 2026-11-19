//! Fixed-size 60 s rings for the Vitals detail views.
//!
//! Every ring holds exactly `HIST` f32 slots, allocated once; pushing never
//! allocates and never grows. NaN marks "no sample" (the view leaves a gap).
//! Memory: 240 bytes per series. A `Rings` of n series is one n x 60 block
//! (32 threads: 7.5 KB).

use super::HIST;

/// One series, oldest to newest.
#[derive(Clone)]
pub struct Ring {
    buf: [f32; HIST],
    /// next slot to write
    head: usize,
}

impl Default for Ring {
    fn default() -> Self {
        Ring { buf: [f32::NAN; HIST], head: 0 }
    }
}

impl Ring {
    pub fn push(&mut self, v: f32) {
        self.buf[self.head] = v;
        self.head = (self.head + 1) % HIST;
    }
    /// Newest sample (NaN before the first push).
    #[cfg(test)]
    pub fn last(&self) -> f32 {
        self.buf[(self.head + HIST - 1) % HIST]
    }
    /// Oldest to newest, always `HIST` values.
    pub fn iter(&self) -> impl Iterator<Item = f32> + '_ {
        (0..HIST).map(move |i| self.buf[(self.head + i) % HIST])
    }
    pub fn to_vec(&self) -> Vec<f32> {
        self.iter().collect()
    }
}

/// `n` series that advance together (one column per push), e.g. one per
/// logical CPU. Stored row-major so a row is contiguous in `to_vec`.
#[derive(Clone, Default)]
pub struct Rings {
    n: usize,
    buf: Vec<f32>,
    head: usize,
}

impl Rings {
    pub fn new(n: usize) -> Self {
        Rings { n, buf: vec![f32::NAN; n * HIST], head: 0 }
    }
    /// Push one column; missing values (a short `col`) are NaN.
    pub fn push(&mut self, col: &[f32]) {
        for r in 0..self.n {
            self.buf[r * HIST + self.head] = col.get(r).copied().unwrap_or(f32::NAN);
        }
        self.head = (self.head + 1) % HIST;
    }
    /// Row-major copy: row r is `out[r * HIST..(r + 1) * HIST]`, oldest first.
    pub fn to_vec(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.buf.len());
        for r in 0..self.n {
            let row = &self.buf[r * HIST..(r + 1) * HIST];
            out.extend_from_slice(&row[self.head..]);
            out.extend_from_slice(&row[..self.head]);
        }
        out
    }
}

/// Max and mean over the finite samples (None when there are none).
pub fn stats(v: impl Iterator<Item = f32>) -> Option<(f32, f32)> {
    let (mut mx, mut sum, mut n) = (f32::MIN, 0.0f32, 0u32);
    for x in v.filter(|x| x.is_finite()) {
        mx = mx.max(x);
        sum += x;
        n += 1;
    }
    (n > 0).then(|| (mx, sum / n as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_last_hist_in_order() {
        let mut r = Ring::default();
        assert!(r.last().is_nan());
        assert!(r.iter().all(|v| v.is_nan()), "empty ring is all gaps");
        for i in 0..(HIST + 5) {
            r.push(i as f32);
        }
        let v = r.to_vec();
        assert_eq!(v.len(), HIST, "never grows");
        assert_eq!(v[0], 5.0, "oldest kept");
        assert_eq!(v[HIST - 1], (HIST + 4) as f32, "newest last");
        assert!(v.windows(2).all(|w| w[1] == w[0] + 1.0));
        assert_eq!(r.last(), (HIST + 4) as f32);
    }

    #[test]
    fn ring_partial_fill_pads_front_with_gaps() {
        let mut r = Ring::default();
        r.push(1.0);
        r.push(2.0);
        let v = r.to_vec();
        assert!(v[..HIST - 2].iter().all(|x| x.is_nan()));
        assert_eq!(&v[HIST - 2..], &[1.0, 2.0]);
    }

    #[test]
    fn rings_rows_advance_together() {
        let mut r = Rings::new(3);
        for t in 0..(HIST + 2) {
            let t = t as f32;
            r.push(&[t, t * 10.0, t * 100.0]);
        }
        let v = r.to_vec();
        assert_eq!(v.len(), 3 * HIST);
        for row in 0..3 {
            let s = &v[row * HIST..(row + 1) * HIST];
            let k = [1.0, 10.0, 100.0][row];
            assert_eq!(s[0], 2.0 * k, "row {row} oldest");
            assert_eq!(s[HIST - 1], (HIST + 1) as f32 * k, "row {row} newest");
        }
    }

    #[test]
    fn rings_short_column_is_gap() {
        let mut r = Rings::new(2);
        r.push(&[7.0]);
        let v = r.to_vec();
        assert_eq!(v[HIST - 1], 7.0);
        assert!(v[2 * HIST - 1].is_nan());
    }

    #[test]
    fn stats_skip_gaps() {
        assert_eq!(stats([f32::NAN, 2.0, 4.0, f32::NAN].into_iter()), Some((4.0, 3.0)));
        assert_eq!(stats([f32::NAN].into_iter()), None);
    }
}
