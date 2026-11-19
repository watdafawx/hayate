//! Remembered main-window placement (`window.ini` in the data dir): pure logic, no Win32.
//! Saved: the NORMAL (restored) rect in pixels, the DPI it was saved at, and the
//! maximized flag. At start the rect is checked against the current monitors:
//! mostly off-screen or monitor gone -> the centred default; otherwise rescaled for
//! a DPI change, clamped to the work area and the minimum size, and pulled on-screen.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn w(&self) -> i32 {
        self.right - self.left
    }
    pub fn h(&self) -> i32 {
        self.bottom - self.top
    }
    fn overlap_area(&self, o: &Rect) -> i64 {
        let w = (self.right.min(o.right) - self.left.max(o.left)).max(0) as i64;
        let h = (self.bottom.min(o.bottom) - self.top.max(o.top)).max(0) as i64;
        w * h
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Saved {
    pub rect: Rect,
    pub dpi: u32,
    pub maximized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mon {
    pub work: Rect,
    pub dpi: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub rect: Rect,
    pub maximized: bool,
    /// true: the saved rect was used (possibly adjusted); false: the centred default
    pub from_saved: bool,
}

const MAX_COORD: i32 = 32000;
const MAX_SIZE: i32 = 20000;

pub fn parse_ini(text: &str) -> Option<Saved> {
    let (mut x, mut y, mut w, mut h, mut dpi, mut max) = (None, None, None, None, None, false);
    for line in text.lines() {
        let line = line.split(';').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        let n = v.parse::<i64>().ok();
        match k.trim().to_ascii_lowercase().as_str() {
            "x" => x = n,
            "y" => y = n,
            "width" => w = n,
            "height" => h = n,
            "dpi" => dpi = n,
            "maximized" => max = matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            _ => {}
        }
    }
    let (x, y, w, h, dpi) = (x?, y?, w?, h?, dpi?);
    let c = MAX_COORD as i64;
    if x.abs() > c || y.abs() > c || !(1..=MAX_SIZE as i64).contains(&w) || !(1..=MAX_SIZE as i64).contains(&h) || !(48..=960).contains(&dpi) {
        return None;
    }
    let (x, y, w, h) = (x as i32, y as i32, w as i32, h as i32);
    Some(Saved { rect: Rect { left: x, top: y, right: x + w, bottom: y + h }, dpi: dpi as u32, maximized: max })
}

pub fn render_ini(s: &Saved) -> String {
    format!(
        "; Hayate main-window placement (written on move/resize/close). Delete this file to reset.\n[window]\nx = {}\ny = {}\nwidth = {}\nheight = {}\ndpi = {}\nmaximized = {}\n",
        s.rect.left,
        s.rect.top,
        s.rect.w(),
        s.rect.h(),
        s.dpi,
        s.maximized
    )
}

fn scale(v: i32, num: u32, den: u32) -> i32 {
    ((v as i64 * num as i64 + den as i64 / 2) / den.max(1) as i64) as i32
}

/// The centred default (`def` DIPs at `mon`'s DPI, clamped to its work area).
pub fn default_rect(mon: &Mon, def: (i32, i32), min: (i32, i32)) -> Rect {
    let (w, h) = (scale(def.0, mon.dpi, 96), scale(def.1, mon.dpi, 96));
    fit(mon, w, h, scale(min.0, mon.dpi, 96), scale(min.1, mon.dpi, 96), None)
}

/// Size clamped to min and work area; position `at` (or centred) pulled fully inside.
fn fit(mon: &Mon, w: i32, h: i32, min_w: i32, min_h: i32, at: Option<(i32, i32)>) -> Rect {
    let wa = mon.work;
    let w = w.max(min_w).min(wa.w().max(1));
    let h = h.max(min_h).min(wa.h().max(1));
    let (mut x, mut y) = at.unwrap_or((wa.left + (wa.w() - w) / 2, wa.top + (wa.h() - h) / 2));
    x = x.min(wa.right - w).max(wa.left);
    y = y.min(wa.bottom - h).max(wa.top);
    Rect { left: x, top: y, right: x + w, bottom: y + h }
}

/// `mons[0]` is the primary monitor. `def` and `min` are in DIPs.
pub fn resolve(saved: Option<&Saved>, mons: &[Mon], def: (i32, i32), min: (i32, i32)) -> Option<Resolved> {
    let primary = mons.first()?;
    let fallback = |max: bool| Resolved { rect: default_rect(primary, def, min), maximized: max, from_saved: false };
    let Some(s) = saved else { return Some(fallback(false)) };
    // monitor holding most of the saved rect
    let (best, area) = mons.iter().map(|m| (m, s.rect.overlap_area(&m.work))).max_by_key(|(_, a)| *a)?;
    let total = (s.rect.w() as i64) * (s.rect.h() as i64);
    // at least a quarter of the window, and a usable strip, must be on a monitor
    if total <= 0 || area * 4 < total || area == 0 {
        return Some(fallback(s.maximized));
    }
    let (mut w, mut h, mut x, mut y) = (s.rect.w(), s.rect.h(), s.rect.left, s.rect.top);
    if s.dpi != best.dpi {
        w = scale(w, best.dpi, s.dpi);
        h = scale(h, best.dpi, s.dpi);
        x = best.work.left + scale(x - best.work.left, best.dpi, s.dpi);
        y = best.work.top + scale(y - best.work.top, best.dpi, s.dpi);
    }
    let rect = fit(best, w, h, scale(min.0, best.dpi, 96), scale(min.1, best.dpi, 96), Some((x, y)));
    Some(Resolved { rect, maximized: s.maximized, from_saved: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, rr: i32, b: i32) -> Rect {
        Rect { left: l, top: t, right: rr, bottom: b }
    }
    fn mon(l: i32, t: i32, rr: i32, b: i32, dpi: u32) -> Mon {
        Mon { work: r(l, t, rr, b), dpi }
    }
    const DEF: (i32, i32) = (1120, 720);
    const MIN: (i32, i32) = (640, 420);

    fn saved(rect: Rect, dpi: u32, max: bool) -> Saved {
        Saved { rect, dpi, maximized: max }
    }

    #[test]
    fn first_run_centred_default() {
        let m = [mon(0, 0, 1920, 1040, 96)];
        let x = resolve(None, &m, DEF, MIN).unwrap();
        assert!(!x.from_saved && !x.maximized);
        assert_eq!(x.rect, r(400, 160, 1520, 880));
    }

    #[test]
    fn saved_rect_kept_when_on_screen() {
        let m = [mon(0, 0, 1920, 1040, 96)];
        let s = saved(r(100, 50, 900, 650), 96, false);
        let x = resolve(Some(&s), &m, DEF, MIN).unwrap();
        assert!(x.from_saved);
        assert_eq!(x.rect, s.rect);
    }

    #[test]
    fn off_screen_falls_back() {
        let m = [mon(0, 0, 1920, 1040, 96)];
        let s = saved(r(5000, 5000, 5800, 5600), 96, false);
        let x = resolve(Some(&s), &m, DEF, MIN).unwrap();
        assert!(!x.from_saved);
        assert_eq!(x.rect, r(400, 160, 1520, 880));
    }

    #[test]
    fn mostly_off_screen_falls_back_slightly_off_clamps() {
        let m = [mon(0, 0, 1920, 1040, 96)];
        // 10 % visible: fallback
        let s = saved(r(1820, 100, 2820, 700), 96, false);
        assert!(!resolve(Some(&s), &m, DEF, MIN).unwrap().from_saved);
        // 84 % visible: pulled inside (min width 640 applies)
        let s = saved(r(1500, 100, 2000, 600), 96, false);
        let x = resolve(Some(&s), &m, DEF, MIN).unwrap();
        assert!(x.from_saved);
        assert_eq!(x.rect, r(1280, 100, 1920, 600));
    }

    #[test]
    fn monitor_gone_second_monitor_removed() {
        // saved on a monitor at x 1920.., now only the primary exists
        let m = [mon(0, 0, 1920, 1040, 96)];
        let s = saved(r(2200, 100, 3000, 700), 96, true);
        let x = resolve(Some(&s), &m, DEF, MIN).unwrap();
        assert!(!x.from_saved);
        assert!(x.maximized); // the flag survives
    }

    #[test]
    fn multi_monitor_picks_largest_overlap() {
        let m = [mon(0, 0, 1920, 1040, 96), mon(1920, 0, 3840, 1040, 96)];
        let s = saved(r(2000, 100, 2800, 700), 96, false);
        let x = resolve(Some(&s), &m, DEF, MIN).unwrap();
        assert!(x.from_saved);
        assert_eq!(x.rect, s.rect);
        // negative-origin monitor (left of primary)
        let m = [mon(0, 0, 1920, 1040, 96), mon(-1280, 0, 0, 1000, 96)];
        let s = saved(r(-1200, 50, -400, 650), 96, false);
        assert_eq!(resolve(Some(&s), &m, DEF, MIN).unwrap().rect, s.rect);
    }

    #[test]
    fn spans_two_monitors_ok() {
        let m = [mon(0, 0, 1920, 1040, 96), mon(1920, 0, 3840, 1040, 96)];
        let s = saved(r(1500, 100, 2300, 700), 96, false);
        assert!(resolve(Some(&s), &m, DEF, MIN).unwrap().from_saved);
    }

    #[test]
    fn dpi_rescale() {
        // saved at 100 %, monitor now 200 %
        let m = [mon(0, 0, 3840, 2080, 192)];
        let s = saved(r(100, 50, 900, 650), 96, false);
        let x = resolve(Some(&s), &m, DEF, MIN).unwrap();
        assert_eq!(x.rect, r(200, 100, 1800, 1300));
        // and back down
        let m = [mon(0, 0, 1920, 1040, 96)];
        let s = saved(r(200, 100, 1800, 1300), 192, false);
        assert_eq!(resolve(Some(&s), &m, DEF, MIN).unwrap().rect, r(100, 50, 900, 650));
    }

    #[test]
    fn clamps_size_to_work_area_and_min() {
        let m = [mon(0, 0, 1280, 720, 96)];
        let s = saved(r(0, 0, 1500, 900), 96, false);
        let x = resolve(Some(&s), &m, DEF, MIN).unwrap();
        assert_eq!(x.rect, r(0, 0, 1280, 720));
        let s = saved(r(100, 100, 150, 130), 96, false);
        let x = resolve(Some(&s), &m, DEF, MIN).unwrap();
        assert_eq!((x.rect.w(), x.rect.h()), (640, 420));
        // default on a small screen is clamped too
        let m = [mon(0, 0, 800, 500, 96)];
        assert_eq!(resolve(None, &m, DEF, MIN).unwrap().rect, r(0, 0, 800, 500));
    }

    #[test]
    fn no_monitors_none() {
        assert!(resolve(None, &[], DEF, MIN).is_none());
    }

    #[test]
    fn ini_round_trip() {
        let s = saved(r(-300, 40, 700, 640), 144, true);
        assert_eq!(parse_ini(&render_ini(&s)), Some(s));
    }

    #[test]
    fn ini_garbage_rejected() {
        assert_eq!(parse_ini(""), None);
        assert_eq!(parse_ini("x = abc\ny = 1\nwidth = 5\nheight = 5\ndpi = 96"), None);
        assert_eq!(parse_ini("x = 99999\ny = 0\nwidth = 800\nheight = 600\ndpi = 96"), None);
        assert_eq!(parse_ini("x = 0\ny = 0\nwidth = 0\nheight = 600\ndpi = 96"), None);
        assert_eq!(parse_ini("x = 0\ny = 0\nwidth = -5\nheight = 600\ndpi = 96"), None);
        assert_eq!(parse_ini("x = 0\ny = 0\nwidth = 800\nheight = 600\ndpi = 0"), None);
        assert_eq!(parse_ini("x = 0\ny = 0\nwidth = 800\nheight = 600"), None); // no dpi
        assert_eq!(parse_ini("x = 0\ny = 0\nwidth = 9999999999999999999\nheight = 600\ndpi = 96"), None);
        assert_eq!(parse_ini("\u{0}\u{1}garbage\n=\n===\n"), None);
        // maximized garbage -> false, other keys fine, comments ignored
        let s = parse_ini("; c\n[window]\nx=1 ; c\ny=2\nwidth=700\nheight=500\ndpi=96\nmaximized=banana\nfoo=bar").unwrap();
        assert!(!s.maximized);
        assert_eq!(s.rect, r(1, 2, 701, 502));
    }
}
