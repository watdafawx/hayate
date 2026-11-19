//! History tab (v2, the design mock): range chips, a summary strip
//! (now / avg / peak), memory-growth suspects, then one graph panel per series
//! recorded by `crate::history` (per-second samples kept in RAM, one hour).
//! The crosshair is synced across all panels by sample age. Nothing here
//! samples or holds handles: it only paints a snapshot while the tab is shown.

use super::*;
use crate::history::{self, Series};
use crate::leak;

/// (label, seconds shown)
pub const RANGES: [(&str, usize); 3] = [("5 min", 300), ("15 min", 900), ("1 h", 3600)];

const GAP: f32 = 8.0;
const PH: f32 = 32.0; // panel header
const PLOT_H: f32 = 84.0;
const CHART_H: f32 = PH + 8.0 + PLOT_H + 4.0 + 13.0 + 7.0;
const STRIP_H: f32 = PH + 62.0;
const LEAK_ROW: f32 = 26.0;
const CHIP_ON_LN: u32 = 0x343c49;

pub struct HistoryView {
    pub range: usize,
    pub scroll: f32,
    /// last mouse position that triggered a hover repaint
    pub hover_x: f32,
    pub hover_y: f32,
}

impl Default for HistoryView {
    fn default() -> Self {
        HistoryView { range: 2, scroll: 0.0, hover_x: -100.0, hover_y: -100.0 }
    }
}

fn fmt_val(unit: &str, v: f32) -> String {
    match unit {
        "%" if v < 10.0 => format!("{v:.1}%"),
        "%" => format!("{v:.0}%"),
        "°C" => format!("{v:.0}°C"),
        "GB" => format!("{v:.1} GB"),
        u if v >= 100.0 => format!("{v:.0} {u}"),
        u => format!("{v:.1} {u}"),
    }
}

fn fmt_mb(mb: f32) -> String {
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

fn ago(secs: usize) -> String {
    if secs < 90 {
        format!("{secs} s ago")
    } else {
        format!("{} min ago", secs / 60)
    }
}

/// Largest value per bucket so spikes survive thinning.
fn thin(v: &[f32], target: usize) -> Vec<f32> {
    if v.len() <= target || target == 0 {
        return v.to_vec();
    }
    let step = v.len().div_ceil(target);
    v.chunks(step).map(|c| c.iter().copied().fold(0.0, f32::max)).collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Stats {
    now: f32,
    avg: f32,
    peak: f32,
    peak_i: usize,
}

/// avg / peak / now of the visible samples; None when there are none.
fn stats(vals: &[f32]) -> Option<Stats> {
    let now = *vals.last()?;
    let mut sum = 0.0;
    let (mut peak, mut peak_i) = (f32::MIN, 0);
    for (i, v) in vals.iter().enumerate() {
        sum += v;
        if *v > peak {
            peak = *v;
            peak_i = i;
        }
    }
    Some(Stats { now, avg: sum / vals.len() as f32, peak, peak_i })
}

/// The newest `range_pts` samples of a series.
fn visible(s: &Series, range_pts: usize) -> Vec<f32> {
    let n = s.data.len();
    s.data.iter().skip(n - n.min(range_pts)).copied().collect()
}

/// Fixed axis if the series has one, else the visible peak with headroom.
fn axis_max(max: Option<f32>, unit: &str, peak: f32) -> f32 {
    max.filter(|m| *m > 0.0).unwrap_or_else(|| (peak * 1.2).max(if unit == "°C" { 50.0 } else { 1.0 }))
}

/// Sample index (into `take` visible samples) that is `age` seconds old, if any.
fn index_at_age(take: usize, age: usize) -> Option<usize> {
    (age < take).then(|| take - 1 - age)
}

/// Age in seconds under the mouse for a plot spanning `range_pts` seconds,
/// newest at the right edge.
fn age_at(mx: f32, plot_x: f32, plot_w: f32, range_pts: usize) -> usize {
    let t = ((plot_x + plot_w - mx) / plot_w.max(1.0)).clamp(0.0, 1.0);
    (t * (range_pts.saturating_sub(1)) as f32).round() as usize
}

/// Series with the highest average over the visible range among those whose
/// key starts with `prefix` followed by digits (`disk`, `nic`).
fn busiest<'a>(series: &'a [Series], prefix: &str, range_pts: usize) -> Option<&'a Series> {
    series
        .iter()
        .filter(|s| s.key.strip_prefix(prefix).is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())))
        .filter_map(|s| stats(&visible(s, range_pts)).map(|st| (s, st.avg)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(s, _)| s)
}

fn by_key<'a>(series: &'a [Series], key: &str) -> Option<&'a Series> {
    series.iter().find(|s| s.key == key)
}

/// Columns of graph panels for a content width.
fn columns(w: f32) -> usize {
    if w >= 820.0 { 2 } else { 1 }
}

fn leak_h(n: usize) -> f32 {
    if n == 0 { 34.0 } else { PH + LEAK_ROW * n.min(5) as f32 }
}

fn content_height(n_leaks: usize, n_series: usize, ncols: usize) -> f32 {
    let rows = n_series.div_ceil(ncols) as f32;
    STRIP_H + GAP + leak_h(n_leaks) + GAP + rows * (CHART_H + GAP)
}

pub fn paint(app: &mut App, g: &mut Gfx, body: Rect) {
    let area = body.inset(10.0, 10.0);
    let range = app.history.range.min(RANGES.len() - 1);
    let (range_label, range_pts) = RANGES[range];

    // header + range chips
    g.text("HISTORY", Rect::new(area.x, area.y, 70.0, 30.0), f(11.0).b(), TX2);
    let mut x = area.r();
    for (i, (label, _)) in RANGES.iter().enumerate().rev() {
        let w = g.measure(label, f(12.0)) + 24.0;
        x -= w;
        let r = Rect::new(x, area.y + 3.0, w, 24.0);
        let on = i == range;
        g.round(r, 12.0, if on { PAN3 } else { BG });
        g.round_stroke(r, 12.0, if on { CHIP_ON_LN } else { LN }, 1.0);
        g.text(label, r, f(12.0).c(), if on { TX1 } else { TX2 });
        app.hits.push((r, Hit::Range(i)));
        x -= 6.0;
    }
    let meta = "per-second samples, last hour kept in RAM";
    let mw = (x - (area.x + 78.0)).max(0.0);
    g.text(meta, Rect::new(area.x + 78.0, area.y, mw, 30.0), f(11.5), TX3);

    let series = history::snapshot();
    let sus = leak::suspects();
    let ncols = columns(area.w);
    let top = area.y + 30.0;
    let view = Rect::new(area.x, top, area.w, (area.b() - top).max(0.0));
    let content_h = content_height(sus.len(), series.len(), ncols);
    app.history.scroll = app.history.scroll.clamp(0.0, (content_h - view.h).max(0.0));
    let pw = area.w - super::nav::gutter(content_h, view.h); // room for the scrollbar
    let oy = top - app.history.scroll;
    let sus_y = oy + STRIP_H + GAP;
    let gy = sus_y + leak_h(sus.len()) + GAP;

    // synced crosshair: the age under the mouse in whichever plot it is over
    let plot_rect = |i: usize| {
        let col_w = if ncols == 2 { (pw - GAP) / 2.0 } else { pw };
        let r = Rect::new(area.x + (i % ncols) as f32 * (col_w + GAP), gy + (i / ncols) as f32 * (CHART_H + GAP), col_w, CHART_H);
        (r, Rect::new(r.x + 12.0, r.y + PH + 8.0, r.w - 24.0, PLOT_H))
    };
    let mut hover_age = None;
    if view.contains(app.mx, app.my) {
        for i in 0..series.len() {
            let (_, p) = plot_rect(i);
            if p.contains(app.mx, app.my) {
                hover_age = Some(age_at(app.mx, p.x, p.w, range_pts));
                break;
            }
        }
    }

    g.clip(view);
    summary(g, Rect::new(area.x, oy, pw, STRIP_H), &series, range_pts, range_label);
    leaks_panel(app, g, Rect::new(area.x, sus_y, pw, leak_h(sus.len())), &sus);
    for (i, s) in series.iter().enumerate() {
        let (r, _) = plot_rect(i);
        if r.b() < view.y || r.y > view.b() {
            continue;
        }
        chart(g, r, s, range_pts, range_label, hover_age);
    }
    if series.is_empty() {
        g.text("Collecting samples…", Rect::new(area.x, gy, area.w, 30.0), f(12.0), TX3);
    }
    g.unclip();

    // scrollbar
    let sc = app.history.scroll;
    super::nav::paint_bar(app, g, super::nav::BAR_HISTORY, view, content_h, sc);
}

fn panel(g: &mut Gfx, r: Rect) {
    g.round(r, 7.0, PAN);
    g.round_stroke(r, 7.0, LN, 1.0);
}

fn panel_head(g: &mut Gfx, r: Rect, title: &str, meta: &str) {
    g.text(&title.to_uppercase(), Rect::new(r.x + 12.0, r.y, r.w - 24.0, PH), f(11.0).b(), TX2);
    if !meta.is_empty() {
        let tw = g.measure(&title.to_uppercase(), f(11.0).b()) + 20.0;
        g.text(meta, Rect::new(r.x + 12.0 + tw, r.y, (r.w - 24.0 - tw).max(0.0), PH), f(11.5), TX3);
    }
    g.line(r.x + 1.0, r.y + PH, r.r() - 1.0, r.y + PH, SEP, 1.0);
}

fn summary(g: &mut Gfx, r: Rect, series: &[Series], range_pts: usize, range_label: &str) {
    panel(g, r);
    panel_head(g, r, "Summary", &format!("last {range_label} · per-second samples"));
    let mut cells: Vec<(String, &Series, u32)> = Vec::new();
    if let Some(s) = by_key(series, "cpu") {
        cells.push(("CPU".into(), s, C_CPU));
    }
    if let Some(s) = by_key(series, "mem") {
        cells.push(("Memory in use".into(), s, C_MEM));
    }
    if let Some(s) = by_key(series, "gpu0") {
        cells.push(("GPU".into(), s, C_GPU));
    }
    if let Some(s) = busiest(series, "disk", range_pts) {
        cells.push((format!("Busiest drive · {}", s.title.split(" · ").next().unwrap_or("")), s, C_DSK));
    }
    if let Some(s) = busiest(series, "nic", range_pts) {
        cells.push((format!("Network · {}", s.title), s, C_NET));
    }
    let body = Rect::new(r.x, r.y + PH, r.w, r.h - PH);
    if cells.is_empty() {
        g.text("Collecting samples…", body.inset(12.0, 0.0), f(12.0), TX3);
        return;
    }
    let cw = body.w / cells.len() as f32;
    for (k, (label, s, col)) in cells.iter().enumerate() {
        let c = Rect::new(body.x + k as f32 * cw, body.y, cw, body.h);
        if k > 0 {
            g.line(c.x, c.y, c.x, c.b(), SEP, 1.0);
        }
        g.round(Rect::new(c.x + 12.0, c.y + 10.0, 3.0, c.h - 20.0), 1.5, *col);
        let tx = c.x + 25.0;
        let tw = (c.r() - tx - 8.0).max(0.0);
        g.text(label, Rect::new(tx, c.y + 8.0, tw, 16.0), f(11.0), TX3);
        match stats(&visible(s, range_pts)) {
            Some(st) => {
                let v = fmt_val(s.unit, st.now);
                let (num, unit) = v.split_once(' ').unwrap_or((&v, ""));
                let nw = g.measure(num, f(21.0).m().b());
                g.text(num, Rect::new(tx, c.y + 22.0, tw, 26.0), f(21.0).m().b(), TX1);
                if !unit.is_empty() {
                    g.text(unit, Rect::new(tx + nw + 3.0, c.y + 22.0, (tw - nw - 3.0).max(0.0), 26.0), f(11.5), TX3);
                }
                let sub = format!("avg {} · peak {}", fmt_val(s.unit, st.avg), fmt_val(s.unit, st.peak));
                g.text(&sub, Rect::new(tx, c.y + 46.0, tw, 14.0), f(11.0).m(), TX3);
            }
            None => g.text("—", Rect::new(tx, c.y + 22.0, tw, 26.0), f(21.0).m().b(), TX3),
        }
    }
}

fn leaks_panel(app: &mut App, g: &mut Gfx, r: Rect, sus: &[leak::Suspect]) {
    panel(g, r);
    if sus.is_empty() {
        g.circle(r.x + 16.0, r.y + r.h / 2.0, 3.5, OK);
        g.text(
            "No memory leaks suspected. Hayate checks every 30 s, even when hidden, and needs about 20 min of history per process.",
            Rect::new(r.x + 30.0, r.y, r.w - 42.0, r.h),
            f(11.5),
            TX3,
        );
        return;
    }
    panel_head(g, r, "Memory growth suspects", "private memory keeps climbing, even after dips");
    for (k, s) in sus.iter().take(5).enumerate() {
        let row = Rect::new(r.x + 1.0, r.y + PH + k as f32 * LEAK_ROW, r.w - 2.0, LEAK_ROW);
        if k % 2 == 0 {
            g.fill(row, ZEBRA);
        }
        g.circle(row.x + 15.0, row.y + LEAK_ROW / 2.0, 3.5, WARN);
        let ir = Rect::new(row.r() - 12.0 - 56.0, row.y + 3.0, 56.0, 20.0);
        let hov = app.hover == Some(Hit::Range(SUS_IGNORE + k));
        g.round(ir, 4.0, if hov { PAN3 } else { PAN2 });
        g.round_stroke(ir, 4.0, LN, 1.0);
        g.text("Ignore", ir, f(11.0).c(), if hov { TX1 } else { TX3 });
        app.hits.push((ir, Hit::Range(SUS_IGNORE + k)));
        let d = format!("{} → {} over {} min · +{}/h", fmt_mb(s.start_mb as f32), fmt_mb(s.now_mb as f32), s.minutes, fmt_mb(s.rate_mb_h));
        let dw = g.measure(&d, f(11.5).m()) + 4.0;
        g.text(&d, Rect::new(ir.x - 12.0 - dw, row.y, dw, LEAK_ROW), f(11.5).m(), TX2);
        let nw = (ir.x - 12.0 - dw - 12.0 - (row.x + 30.0)).max(0.0);
        let name = format!("{}  ·  pid {}", s.name, s.pid);
        g.text(&name, Rect::new(row.x + 30.0, row.y, nw, LEAK_ROW), f(12.0), TX1);
    }
}

fn chart(g: &mut Gfx, r: Rect, s: &Series, range_pts: usize, range_label: &str, hover_age: Option<usize>) {
    panel(g, r);
    let vals = visible(s, range_pts);
    let take = vals.len();
    let st = stats(&vals);
    let peak = st.map_or(0.0, |x| x.peak.max(0.0));
    let max = axis_max(s.max, s.unit, peak);
    let hov_i = hover_age.and_then(|a| index_at_age(take, a)).filter(|_| take >= 2);

    // header: title left, legend (now / avg / peak) right
    let shown_now = match (hov_i, hover_age) {
        (Some(i), Some(a)) => Some((ago(a), vals[i])),
        _ => st.map(|x| ("now".to_string(), x.now)),
    };
    let mut legend: Vec<(String, String, bool)> = Vec::new();
    if let Some((l, v)) = shown_now {
        legend.push((l, fmt_val(s.unit, v), true));
    }
    if let Some(x) = st {
        legend.push(("avg".into(), fmt_val(s.unit, x.avg), false));
        legend.push(("peak".into(), fmt_val(s.unit, x.peak), false));
    }
    let mut lx = r.r() - 12.0;
    for (label, val, strong) in legend.iter().rev() {
        let vf = if *strong { f(11.0).m().b() } else { f(11.0).m() };
        let vw = g.measure(val, vf) + 2.0;
        lx -= vw;
        g.text(val, Rect::new(lx, r.y, vw, PH), vf, if *strong { TX1 } else { TX2 });
        let lw = g.measure(label, f(11.0)) + 4.0;
        lx -= lw;
        g.text(label, Rect::new(lx, r.y, lw, PH), f(11.0), TX3);
        if *strong {
            // colour key square before the "now" label only
            g.round(Rect::new(lx - 13.0, r.y + PH / 2.0 - 4.0, 8.0, 8.0), 2.0, s.color);
            lx -= 13.0;
        }
        lx -= 14.0;
    }
    let tw = (lx - (r.x + 12.0) - 4.0).max(0.0);
    g.text(&s.title.to_uppercase(), Rect::new(r.x + 12.0, r.y, tw, PH), f(11.0).b(), TX2);
    g.line(r.x + 1.0, r.y + PH, r.r() - 1.0, r.y + PH, SEP, 1.0);

    let plot = Rect::new(r.x + 12.0, r.y + PH + 8.0, r.w - 24.0, PLOT_H);
    for k in 1..4 {
        let y = plot.y + plot.h * k as f32 / 4.0;
        g.line(plot.x, y, plot.r(), y, SEP, 1.0);
    }
    // the part of the window that has data is right-aligned
    let frac = take as f32 / range_pts as f32;
    let sub = Rect::new(plot.r() - plot.w * frac, plot.y, plot.w * frac, plot.h);
    if take >= 2 {
        g.spark(sub, thin(&vals, (sub.w / 2.0).max(2.0) as usize).into_iter(), max, s.color);
        // peak marker
        if let Some(x) = st.filter(|x| x.peak > 0.0) {
            let px = sub.x + sub.w * x.peak_i as f32 / (take - 1) as f32;
            let py = sub.b() - (x.peak / max).clamp(0.0, 1.0) * (sub.h - 1.5) - 0.75;
            g.circle(px, py, 4.5, PAN);
            g.circle(px, py, 2.5, s.color);
        }
    } else {
        g.text("Collecting samples…", plot, f(11.5).c(), TX4);
    }
    g.text(&fmt_val(s.unit, max), Rect::new(plot.x + 4.0, plot.y, 90.0, 14.0), f(10.0).m(), TX4);
    g.text(&format!("−{range_label}"), Rect::new(plot.x, plot.b() + 4.0, 80.0, 13.0), f(10.0).m(), TX4);
    g.text("now", Rect::new(plot.r() - 40.0, plot.b() + 4.0, 40.0, 13.0), f(10.0).m().r(), TX4);

    // crosshair + readout
    if let (Some(i), Some(a)) = (hov_i, hover_age) {
        let x = sub.x + sub.w * i as f32 / (take - 1) as f32;
        g.line(x, plot.y, x, plot.b(), TX3, 1.0);
        let t = format!("{} · {}", fmt_val(s.unit, vals[i]), ago(a));
        let w = g.measure(&t, f(11.0).m()) + 12.0;
        let flip = (x - plot.x) / plot.w.max(1.0) > 0.6;
        let rx = if flip { x - 6.0 - w } else { x + 6.0 };
        let rr = Rect::new(rx.max(plot.x), plot.y, w, 18.0);
        g.round(rr, 4.0, PAN3);
        g.round_stroke(rr, 4.0, LN, 1.0);
        g.text(&t, rr, f(11.0).m().c(), TX1);
    }
}

/// Hit::Range ids from here on are the suspects' "Ignore" buttons.
const SUS_IGNORE: usize = 100;

/// No rows to select here: Up / Down / Page / Home / End scroll the panels.
pub fn key_scroll(app: &mut App, m: super::nav::Move) {
    use super::nav::Move;
    // the panel area: the body minus its 10 px insets and the 30 px header
    let view_h = (app.h - TITLE_H - STATUS_H - 50.0).max(80.0);
    let s = &mut app.history.scroll;
    match m {
        Move::Up => *s -= 40.0,
        Move::Down => *s += 40.0,
        Move::PageUp => *s -= view_h - 40.0,
        Move::PageDown => *s += view_h - 40.0,
        Move::Home => *s = 0.0,
        // paint clamps to the real end
        Move::End => *s = f32::MAX / 4.0,
    }
    *s = s.max(0.0);
}

pub fn click(app: &mut App, h: Hit) -> Action {
    match h {
        Hit::Range(i) if i >= SUS_IGNORE => {
            if let Some(s) = leak::suspects().get(i - SUS_IGNORE) {
                leak::set_user_ignored(&s.name, true);
            }
            Action::Repaint
        }
        Hit::Range(i) => {
            app.history.range = i;
            Action::Repaint
        }
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn series(key: &str, unit: &'static str, d: &[f32]) -> Series {
        Series::for_test(key, unit, d)
    }

    #[test]
    fn stats_avg_peak_now() {
        let st = stats(&[1.0, 5.0, 3.0]).unwrap();
        assert_eq!((st.now, st.peak, st.peak_i), (3.0, 5.0, 1));
        assert!((st.avg - 3.0).abs() < 1e-6);
        assert!(stats(&[]).is_none());
    }

    #[test]
    fn visible_takes_newest() {
        let s = series("cpu", "%", &[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(visible(&s, 2), vec![3.0, 4.0]);
        assert_eq!(visible(&s, 10).len(), 4);
    }

    #[test]
    fn axis_fixed_or_headroom() {
        assert_eq!(axis_max(Some(100.0), "%", 3.0), 100.0);
        assert!((axis_max(None, "MB/s", 100.0) - 120.0).abs() < 1e-4);
        assert_eq!(axis_max(None, "°C", 40.0), 50.0);
        assert_eq!(axis_max(None, "MB/s", 0.0), 1.0);
    }

    #[test]
    fn crosshair_age_maps_to_sample() {
        // right edge = newest
        assert_eq!(age_at(110.0, 10.0, 100.0, 301), 0);
        assert_eq!(age_at(10.0, 10.0, 100.0, 301), 300);
        assert_eq!(age_at(60.0, 10.0, 100.0, 301), 150);
        // a series with 60 samples has nothing 100 s ago: no crosshair, no fake value
        assert_eq!(index_at_age(60, 0), Some(59));
        assert_eq!(index_at_age(60, 59), Some(0));
        assert_eq!(index_at_age(60, 100), None);
    }

    #[test]
    fn busiest_picks_highest_average_of_numbered_keys() {
        let all = vec![
            series("disk0", "MB/s", &[1.0, 1.0]),
            series("disk3", "MB/s", &[9.0, 9.0]),
            series("disks", "MB/s", &[99.0, 99.0]),
            series("nic0", "Mb/s", &[2.0, 2.0]),
        ];
        assert_eq!(busiest(&all, "disk", 60).unwrap().key, "disk3");
        assert_eq!(busiest(&all, "nic", 60).unwrap().key, "nic0");
        assert!(busiest(&all[3..], "disk", 60).is_none());
        // visible range changes the verdict: only the newest sample counts
        let b = vec![series("disk0", "MB/s", &[50.0, 0.0, 5.0]), series("disk1", "MB/s", &[0.0, 0.0, 6.0])];
        assert_eq!(busiest(&b, "disk", 1).unwrap().key, "disk1");
    }

    #[test]
    fn layout_heights_and_columns() {
        assert_eq!(columns(819.0), 1);
        assert_eq!(columns(820.0), 2);
        assert_eq!(leak_h(0), 34.0);
        assert_eq!(leak_h(2), PH + 2.0 * LEAK_ROW);
        assert_eq!(leak_h(9), PH + 5.0 * LEAK_ROW);
        let one = content_height(0, 9, 1);
        let two = content_height(0, 9, 2);
        assert!(two < one);
        assert_eq!(content_height(0, 4, 2) - content_height(0, 2, 2), CHART_H + GAP);
    }
}
