//! Vitals, direction D1 "Stacked": a Task Manager style device list on the
//! left (thumbnail graph, value line, model), and for the selected device a
//! detail pane in one order for every device: header stats → 60 s timeline →
//! device section → top consumers.
//!
//! Paint cost: every series is one path geometry; the CPU heatmap is one
//! nearest-neighbour bitmap per cluster plus a cell-gap overlay (~100 thin
//! fills), so the CPU view stays a few hundred draw calls.

use super::order::{self, Slot, Stable};
use super::procmenu::{Origin, Rep, ShowIn, Target};
use super::*;
use crate::sample::{ring, Snapshot, HIST};
use std::collections::HashMap;
use std::sync::Arc;

mod nethealth;
mod offenders;

/// A device in the list. Disks and adapters are keyed by number / LUID so
/// the selection survives reordering.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Dev {
    #[default]
    Cpu,
    Mem,
    Gpu(usize),
    Disk(u32),
    Nic(u64),
}

pub struct VitState {
    pub sel: Dev,
    /// CPU: full-height thread bars instead of the heatmap
    bars: bool,
    /// consumers: one row per process instead of per app
    per_proc: bool,
    /// Top by Network: which remote scope the rows show (All / LAN / WAN)
    net_view: crate::netetw::View,
    /// Network throughput graph: Total / LAN / WAN of all adapters on one plot (else the selected
    /// adapter's own down and up). Independent of `net_view`: the two controls label their own source
    net_combined: bool,
    /// Combined graph direction: up instead of down
    net_up: bool,
    pub(super) list_scroll: f32,
    list_h: f32,
    pub(super) scroll: f32,
    content_h: f32,
    /// timeline column under the mouse
    hover_col: Option<usize>,
    /// hover area of the current timeline (plot x range, whole timeline height)
    plot: Rect,
    /// a second timeline on the same time axis (Network: the LAN graph); the columns line up,
    /// so one hovered column reads both, each from its own source
    plot2: Rect,
    /// a third (Network: the internet RTT graph); the gateway one is `plot2`
    plot3: Rect,
    /// which hover area the pointer is in (0 main, 1 second, 2 third): only that graph shows the readout
    hover_slot: u8,
    devs: Vec<Dev>,
    /// keyboard: each device entry's (top, height) in list content coordinates, and the list's view height
    dev_spans: Vec<(f32, f32)>,
    list_view_h: f32,
    groups_seq: u64,
    groups: Vec<procs::Group>,
    wd: Widths,
    keys: Vec<Arc<str>>,
    /// per heat bitmap: (sample seq, device w, h) it was built for
    stamps: Vec<(u64, u32, u32)>,
    /// held row order per Top list (`list_id`): values live, order every 5 s / when idle
    tops: HashMap<u8, Stable<TopKey>>,
    /// last known row names, for rows that exited while the order was held
    top_names: HashMap<TopKey, String>,
    /// last wheel / scroll interaction (order::now_ms), 0 = none
    touch_at: u64,
    /// what each Top row of the last paint stood for, by the `Hit::TopRow(i)` it registered
    top_hits: Vec<TopHit>,
    /// the snapshot the last paint drew from (a right-click resolves its rows against it)
    shown: Option<Arc<Snapshot>>,
    /// area the detail column is drawn in: hit regions are cut to it
    clip: Rect,
    /// the row a context menu is open on
    pub pick: Option<TopKey>,
    /// Who is failing: the selected app (lower-cased name), "Show apps" pressed, and the key of
    /// each table row of the last paint (`Hit::VitSeg(offenders::ROW_HIT + i)`)
    off_sel: Option<String>,
    off_show: bool,
    off_keys: Vec<String>,
    /// (ip, port) of the rows drawn last: the only destinations "Test host" may use
    off_targets: Vec<(std::net::IpAddr, u16)>,
    /// network incident history (feeds one Watch event per long incident)
    incident: crate::netincident::Tracker,
}

/// One Top row as drawn: enough to rebuild what it stood for from the snapshot it was drawn from.
#[derive(Clone, Debug)]
struct TopHit {
    id: TopKey,
    /// the process whose icon stands for the row: the representative of an Apps row
    rep: (u32, i64),
    /// the name the row showed
    name: String,
    by: By,
    view: crate::netetw::View,
}

/// What identifies a Top row across refreshes: an app (lower-cased display
/// name, the Apps list merges by it) or one process (pid + create time).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TopKey {
    App(String),
    Proc(u32, i64),
}

impl Default for VitState {
    fn default() -> Self {
        VitState {
            sel: Dev::Cpu,
            bars: false,
            per_proc: false,
            net_view: crate::netetw::View::All,
            net_combined: true,
            net_up: false,
            list_scroll: 0.0,
            list_h: 0.0,
            scroll: 0.0,
            content_h: 0.0,
            hover_col: None,
            plot: Rect::default(),
            plot2: Rect::default(),
            plot3: Rect::default(),
            hover_slot: 0,
            devs: Vec::new(),
            dev_spans: Vec::new(),
            list_view_h: 0.0,
            groups_seq: u64::MAX,
            groups: Vec::new(),
            wd: Widths::default(),
            keys: (0..8).map(|i| Arc::from(format!("\u{0}vit-heat{i}"))).collect(),
            stamps: vec![(0, 0, 0); 8],
            tops: HashMap::new(),
            top_names: HashMap::new(),
            touch_at: 0,
            top_hits: Vec::new(),
            shown: None,
            clip: Rect::default(),
            pick: None,
            off_sel: None,
            off_show: false,
            off_keys: Vec::new(),
            off_targets: Vec::new(),
            incident: crate::netincident::Tracker::default(),
        }
    }
}

impl VitState {
    /// A wheel notch, drag or menu close: Top orders hold for 2 s.
    pub fn touch(&mut self) {
        self.touch_at = order::now_ms();
    }
}

/// For the sampler: which detail reads to run (`sample::VIT_*`).
pub fn sel_code(app: &App) -> u8 {
    use crate::sample::*;
    if app.tab != Tab::Vitals {
        return 0;
    }
    match app.vit.sel {
        Dev::Cpu => VIT_CPU,
        Dev::Mem => VIT_MEM,
        Dev::Gpu(_) => VIT_GPU,
        Dev::Disk(_) => VIT_DISK,
        Dev::Nic(_) => VIT_NET,
    }
}

const MEM_COMMIT: u32 = 0x8a7fd0;
const MEM_MOD: u32 = 0x6f5fae;
const MEM_STBY: u32 = 0x4a4470;
const DSK_W: u32 = 0x9a7440;
const NET_UP: u32 = 0x4f6fa0;
const GPU_VRAM: u32 = 0x4f8f70;
const INSTALLED: u32 = 0x3a4250;
const HEAT_MID: u32 = 0x2a8f9e;
const HEAT_HI: u32 = 0x9be3ec;
const HEAT_NONE: u32 = 0x1a1f26;
const CLUSTER_C: [u32; 4] = [C_CPU0, HEAT_MID, 0x7fb8c0, 0x3f8790];

pub(crate) fn short_cpu(name: &str) -> String {
    let n = name.replace("AMD ", "").replace("Intel(R) Core(TM) ", "").replace("(R)", "").replace("(TM)", "");
    match n.find(" Processor").or_else(|| n.find(" CPU")) {
        Some(i) => {
            // "Ryzen 9 5900X 12-Core" → "Ryzen 9 5900X"
            let s = n[..i].trim();
            match s.rfind(' ') {
                Some(sp) if s.ends_with("-Core") => s[..sp].trim().to_string(),
                _ => s.to_string(),
            }
        }
        None => n.trim().to_string(),
    }
}

fn short_gpu(name: &str) -> String {
    name.replace("NVIDIA GeForce ", "").replace("NVIDIA ", "").replace("(TM)", "").replace("(R)", "").replace("AMD Radeon", "Radeon").trim().to_string()
}

/// 87.4 MB / 504 MB / 1.19 GB / 16.6 GB (three digits from 1 GB up)
pub(super) fn mem_s(b: u64) -> String {
    let mb = b as f64 / 1048576.0;
    let g = mb / 1024.0;
    if g >= 100.0 {
        format!("{g:.0} GB")
    } else if g >= 10.0 {
        format!("{g:.1} GB")
    } else if g >= 1.0 {
        format!("{g:.2} GB")
    } else if mb >= 100.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{mb:.1} MB")
    }
}
pub(super) fn gb(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}
/// 0.4 / 41.6 / 312
pub(super) fn num(v: f64) -> String {
    if v >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}
fn link(bps: u64) -> String {
    if bps >= 1_000_000_000 {
        format!("{} Gb/s", num(bps as f64 / 1e9).trim_end_matches(".0"))
    } else {
        format!("{} Mb/s", bps / 1_000_000)
    }
}
pub(super) fn size(b: u64) -> String {
    let g = b as f64 / 1e9;
    if g >= 1000.0 {
        format!("{:.1} TB", g / 1000.0)
    } else {
        format!("{g:.0} GB")
    }
}
/// Smallest 1 / 2 / 2.5 / 5 x 10^k at or above `v`.
fn nice(v: f32) -> f32 {
    let v = v.max(1e-3);
    let p = 10f32.powf(v.log10().floor());
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if m * p >= v * 0.999 {
            return m * p;
        }
    }
    10.0 * p
}
fn last(v: &[f32]) -> f32 {
    v.iter().rev().copied().find(|x| x.is_finite()).unwrap_or(0.0)
}
fn vmax(v: &[f32]) -> f32 {
    ring::stats(v.iter().copied()).map(|s| s.0).unwrap_or(0.0)
}
fn ago(c: usize) -> String {
    match HIST - 1 - c.min(HIST - 1) {
        0 => "now".into(),
        n => format!("−{n} s"),
    }
}
fn at(v: &[f32], c: usize) -> f32 {
    v.get(c).copied().unwrap_or(f32::NAN)
}
fn pct(v: f32) -> String {
    if v.is_finite() {
        format!("{v:.0}%")
    } else {
        "—".into()
    }
}
/// `lo..hi` blend of two 0xRRGGBB colours.
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |s: u32| {
        let x = ((a >> s) & 0xff) as f32;
        let y = ((b >> s) & 0xff) as f32;
        ((x + (y - x) * t).round() as u32) << s
    };
    ch(16) | ch(8) | ch(0)
}
fn heat(v: f32) -> u32 {
    if !v.is_finite() {
        HEAT_NONE
    } else if v < 55.0 {
        mix(TRACK, HEAT_MID, v / 55.0)
    } else {
        mix(HEAT_MID, HEAT_HI, (v - 55.0) / 45.0)
    }
}

// ---------- input ----------

/// Mouse moved: tracks the timeline column under it. True when it changed.
pub fn hover(app: &mut App, x: f32, y: f32) -> bool {
    let hit = [app.vit.plot, app.vit.plot2, app.vit.plot3]
        .into_iter()
        .enumerate()
        .find(|(_, p)| app.tab == Tab::Vitals && p.w > 0.0 && p.contains(x, y))
        .map(|(k, p)| ((((x - p.x) / p.w * HIST as f32) as usize).min(HIST - 1), k as u8));
    let c = hit.map(|h| h.0);
    let changed = c != app.vit.hover_col || hit.is_some_and(|h| h.1 != app.vit.hover_slot);
    app.vit.hover_col = c;
    if let Some((_, k)) = hit {
        app.vit.hover_slot = k;
    }
    changed
}

pub fn leave(app: &mut App) {
    app.vit.hover_col = None;
}

pub fn wheel(app: &mut App, delta: f32) -> bool {
    app.vit.touch();
    let over_list = app.hits.iter().any(|(r, h)| matches!(h, Hit::Device(_)) && r.x <= app.mx && app.mx <= r.r());
    if over_list {
        app.vit.list_scroll -= delta * 0.6;
    } else {
        app.vit.scroll -= delta * 0.6;
    }
    true
}

pub fn click(app: &mut App, h: Hit) -> Action {
    match h {
        Hit::Device(i) => {
            if let Some(d) = app.vit.devs.get(i).copied() {
                if d != app.vit.sel {
                    app.vit.sel = d;
                    app.vit.scroll = 0.0;
                    app.vit.hover_col = None;
                }
            }
        }
        Hit::VitSeg(0) => app.vit.bars = false,
        Hit::VitSeg(1) => app.vit.bars = true,
        // Apps / Processes is an explicit change: new rows, new stable order at once
        Hit::VitSeg(2) => {
            app.vit.per_proc = false;
            app.vit.tops.clear();
        }
        Hit::VitSeg(3) => {
            app.vit.per_proc = true;
            app.vit.tops.clear();
        }
        // All / LAN / WAN: also an explicit change of what the list is
        Hit::VitSeg(i @ 4..=6) => {
            let v = [crate::netetw::View::All, crate::netetw::View::Lan, crate::netetw::View::Wan][i - 4];
            if app.vit.net_view != v {
                app.vit.net_view = v;
                app.vit.tops.clear();
            }
        }
        // throughput graph: Combined / Adapter, and the Combined graph's Down / Up
        Hit::VitSeg(i @ 7..=10) => {
            match i {
                7 => app.vit.net_combined = true,
                8 => app.vit.net_combined = false,
                9 => app.vit.net_up = false,
                _ => app.vit.net_up = true,
            }
            app.vit.hover_col = None;
        }
        // network probes: the switch in the Network health panel, and the "Turn probes on" button
        Hit::VitSeg(11) => {
            crate::sample::probe::toggle();
        }
        Hit::VitSeg(12) => {
            if !crate::sample::probe::enabled() {
                crate::sample::probe::toggle();
            }
        }
        Hit::VitSeg(offenders::SHOW_HIT) => app.vit.off_show = !app.vit.off_show,
        // a Who-is-failing row: select the app (again: clear), the graph marks follow
        Hit::VitSeg(i @ 100..=199) => {
            if let Some(k) = app.vit.off_keys.get(i - offenders::ROW_HIT).cloned() {
                app.vit.off_sel = if app.vit.off_sel.as_ref() == Some(&k) { None } else { Some(k) };
            }
        }
        // "Test host" on a row: one echo + one connect to that row's IP:port, only by this click
        Hit::VitSeg(i @ 200..=299) => {
            if let Some(&(ip, port)) = app.vit.off_targets.get(i - offenders::TEST_HIT) {
                let _ = crate::nettest::start(ip, port, &app.vit.off_targets);
            }
        }
        Hit::VitAdmin => return Action::RestartAdmin,
        _ => return Action::None,
    }
    Action::Repaint
}

// ---------- layout ----------

struct L {
    narrow: bool,
    /// detail column
    x: f32,
    w: f32,
}
impl L {
    fn geo(&self) -> sensors::Geo {
        sensors::Geo { x: self.x, w: self.w, narrow: self.narrow }
    }
}

pub fn paint(app: &mut App, g: &mut Gfx, body: Rect) {
    let s = app.snap.clone();
    // Top rows register fresh each paint, against exactly this snapshot
    app.vit.top_hits.clear();
    app.vit.shown = Some(s.clone());
    app.vit.clip = Rect::default();
    // every Vitals paint feeds the sensor rings (all devices), so unsampled time is a gap
    app.sensors.refresh(&s);
    let narrow = body.w < 1000.0;
    let list_w = if narrow { 178.0 } else { 226.0 };
    let list = Rect::new(body.x + 10.0, body.y + 10.0, list_w, body.h - 20.0);
    device_list(app, g, &s, list, narrow);
    // selection fell away (GPU / disk / adapter gone): back to CPU
    if !app.vit.devs.contains(&app.vit.sel) {
        app.vit.sel = Dev::Cpu;
    }
    let dx = list.r() + 8.0;
    let view = Rect::new(dx, body.y + 10.0, body.r() - 10.0 - dx, body.h - 20.0);
    if view.w < 200.0 {
        return;
    }
    // CPU / GPU / disk: the PawnIO notice and "Not detected" strip dock under the
    // scrolling detail column, so the scroll viewport (`view`) shrinks by their height
    let foot = sensor_dev(&s, app.vit.sel).map(|d| sensors::foot(app, g, &s, view.w, d)).filter(|f| f.h > 0.0);
    let foot_h = foot.as_ref().map_or(0.0, |f| f.h + 8.0);
    let view = Rect::new(view.x, view.y, view.w, (view.h - foot_h).max(120.0));
    let max_scroll = (app.vit.content_h - view.h).max(0.0);
    app.vit.scroll = app.vit.scroll.clamp(0.0, max_scroll);
    // the scroll bar takes its gutter from the column only while the content overflows (last paint's height)
    let cview = Rect::new(view.x, view.y, view.w - super::nav::gutter(app.vit.content_h, view.h), view.h);
    let l = L { narrow, x: view.x, w: cview.w };
    let clip_h = if foot.is_some() { view.b() + 4.0 - body.y } else { body.h };
    app.vit.clip = Rect::new(view.x - 2.0, body.y, view.w + 4.0, clip_h);
    g.clip(app.vit.clip);
    let top = view.y - app.vit.scroll;
    app.vit.plot = Rect::default();
    app.vit.plot2 = Rect::default();
    app.vit.plot3 = Rect::default();
    let end = match app.vit.sel {
        Dev::Cpu => cpu(app, g, &s, &l, top, cview),
        Dev::Mem => mem(app, g, &s, &l, top, cview),
        Dev::Gpu(i) => gpu(app, g, &s, &l, top, cview, i),
        Dev::Disk(n) => disk(app, g, &s, &l, top, cview, n),
        Dev::Nic(id) => nic(app, g, &s, &l, top, cview, id),
    };
    g.unclip();
    app.vit.content_h = end - top;
    let sc = app.vit.scroll;
    super::nav::paint_bar(app, g, super::nav::BAR_VIT_DETAIL, view, app.vit.content_h, sc);
    for slot in [0, 1, 2] {
        let p = match slot {
            0 => &mut app.vit.plot,
            1 => &mut app.vit.plot2,
            _ => &mut app.vit.plot3,
        };
        if p.b() < body.y || p.y > body.b() {
            *p = Rect::default();
        }
    }
    if let Some(f) = &foot {
        sensors::paint_foot(app, g, f, Rect::new(view.x, view.b() + 8.0, view.w, f.h));
    }
}

/// The Sensors device behind a Vitals selection (None: memory, network).
fn sensor_dev(s: &Snapshot, d: Dev) -> Option<sensors::SDev> {
    match d {
        Dev::Cpu => Some(sensors::SDev::Cpu),
        Dev::Gpu(i) => Some(sensors::SDev::Gpu(i)),
        Dev::Disk(n) => s.vit.disks.iter().position(|x| x.num == n).map(sensors::SDev::Disk),
        _ => None,
    }
}

// ---------- device list ----------

fn device_list(app: &mut App, g: &mut Gfx, s: &Snapshot, area: Rect, narrow: bool) {
    let item_h = if narrow { 52.0 } else { 56.0 };
    let (tw, th) = if narrow { (44.0, 32.0) } else { (60.0, 38.0) };
    let max = (app.vit.list_h - area.h).max(0.0);
    app.vit.list_scroll = app.vit.list_scroll.clamp(0.0, max);
    // the scroll bar takes its gutter from the entries only while the list overflows (last paint's height)
    let aw = area.w - super::nav::gutter(app.vit.list_h, area.h);
    g.clip(area);
    let mut y = area.y - app.vit.list_scroll;
    let y0 = y;
    let mut devs = Vec::new();
    let mut spans = Vec::new();
    let v = &s.vit;
    let rows = v.cores.len() / HIST;
    let total: Vec<f32> = if rows > 0 { col_mean(&v.cores, &(0..rows).collect::<Vec<_>>()) } else { s.cpu_hist.clone() };

    let mut item = |app: &mut App, g: &mut Gfx, y: &mut f32, dev: Dev, a: &str, b: &str, c: &str, spark: &[f32], hi: f32, col: u32, tp: &str, hot: sensors::St| {
        let i = devs.len();
        devs.push(dev);
        spans.push((*y - y0, item_h));
        let r = Rect::new(area.x, *y, aw, item_h);
        *y += item_h + 2.0;
        if r.b() < area.y || r.y > area.b() {
            return;
        }
        let on = app.vit.sel == dev;
        if on {
            g.round(r, 6.0, ACC_BG);
            g.round(Rect::new(r.x, r.y + 8.0, 2.0, r.h - 16.0), 1.0, ACC);
        } else if app.hover == Some(Hit::Device(i)) {
            g.round(r, 6.0, PAN2);
        }
        let tr = Rect::new(r.x + 10.0, r.y + (r.h - th) / 2.0, tw, th);
        g.round(tr, 3.0, BG);
        g.series(tr.inset(1.5, 2.0), spark, 0.0, hi.max(1e-3), col, 0.16, 1.1, false);
        g.round_stroke(tr, 3.0, if on { ACC_LN } else { LN }, 1.0);
        let tx = tr.r() + 10.0;
        let tw2 = r.r() - tx - 6.0;
        let top = r.y + (r.h - 46.0) / 2.0;
        g.text(a, Rect::new(tx, top, tw2 - 14.0, 16.0), f(12.5), TX1);
        // drives: temperature right-aligned on the value line
        let tpw = if tp.is_empty() { 0.0 } else { g.measure(tp, f(11.0).m()) + 6.0 };
        g.text(b, Rect::new(tx, top + 16.0, tw2 - tpw, 15.0), f(11.5).m(), TX2);
        if tpw > 0.0 {
            g.text(tp, Rect::new(tx, top + 16.0, tw2, 15.0), f(11.0).m().r(), TX2);
        }
        g.text(c, Rect::new(tx, top + 31.0, tw2, 15.0), f(11.0), TX3);
        // WARM / HOT dot, only when warm or hot
        if hot != sensors::St::Ok {
            g.circle(r.r() - 10.0 - 3.5, r.y + 13.0 + 3.5, 3.5, if hot == sensors::St::Hot { HOT } else { WARM });
        }
        app.hits.push((r, Hit::Device(i)));
    };
    let head = |g: &mut Gfx, y: &mut f32, t: &str, note: &str| {
        let r = Rect::new(area.x + 10.0, *y + 6.0, aw - 18.0, 20.0);
        g.text(t, r, f(10.5).b(), TX4);
        if !note.is_empty() {
            g.text(note, r, f(10.5).m().r(), TX4);
        }
        *y += 28.0;
    };

    let temp = s.cpu_temp.filter(|_| !s.cpu_temp_stale).map(|t| format!(" · {t:.0}°")).unwrap_or_default();
    let hot = app.sensors.dev_state("cpu");
    item(app, g, &mut y, Dev::Cpu, "CPU", &format!("{:.0}%{temp}", s.cpu), &short_cpu(&s.st.cpu_name), &total, 100.0, C_CPU, "", hot);
    let m = &s.mem;
    let used = m.total.saturating_sub(m.avail);
    let pc = if m.total > 0 { used as f64 / m.total as f64 * 100.0 } else { 0.0 };
    item(app, g, &mut y, Dev::Mem, "Memory", &format!("{:.1}/{:.0} GB ({pc:.0}%)", gb(used), gb(m.total)), &s.st.ram_desc, &s.mem_hist, gb(m.total) as f32, C_MEM, "", sensors::St::Ok);
    if !s.gpus.is_empty() {
        head(g, &mut y, "GPU", "");
        for (i, p) in s.gpus.iter().enumerate() {
            let raw = s.gpu_raw.get(i);
            let t = raw.and_then(|r| r.temp).map(|t| format!(" · {t:.0}°")).unwrap_or_default();
            let model = raw
                .map(|r| format!("{} · {:.1}/{:.0} GB", short_gpu(&r.name), gb(r.vram_used), gb(r.vram_total)))
                .unwrap_or_default();
            let hist = v.gpus.get(i).map(|x| &x.util[..]).unwrap_or(&p.hist[..]);
            let hot = app.sensors.dev_state(&sensors::SensorsView::gpu_key(i));
            item(app, g, &mut y, Dev::Gpu(i), &format!("GPU {i}"), &format!("{:.0}%{t}", p.value), &model, hist, 100.0, C_GPU, "", hot);
        }
    }
    if !s.disks.is_empty() {
        head(g, &mut y, "DISKS", "");
        for (i, p) in s.disks.iter().enumerate() {
            let tp = p.temp.filter(|_| !p.temp_stale).map(|t| format!("{t:.0}°")).unwrap_or_default();
            let hot = app.sensors.dev_state(&p.title);
            let Some(d) = v.disks.get(i) else {
                item(app, g, &mut y, Dev::Disk(i as u32), &p.title, &format!("{:.1} MB/s", p.value), "", &p.hist, p.max, C_DSK, &tp, hot);
                continue;
            };
            let name = if d.letters.is_empty() { format!("Disk {}", d.num) } else { format!("Disk {} ({})", d.num, d.letters) };
            let model = format!("{} · {}", if d.model.is_empty() { "Disk" } else { &d.model }, d.bus);
            item(app, g, &mut y, Dev::Disk(d.num), &name, &format!("{:.0}% · {} MB/s", d.active, num((d.read_bps + d.write_bps) / 1e6)), &model, &d.act, 100.0, C_DSK, &tp, hot);
        }
    }
    let hidden = v.nics_hidden;
    if !s.nics.is_empty() || hidden > 0 {
        let note = match (hidden, narrow) {
            (0, _) => String::new(),
            (n, true) => format!("{n} hidden"),
            (n, false) => format!("{n} disconnected hidden"),
        };
        head(g, &mut y, "NETWORK", &note);
        for (i, p) in s.nics.iter().enumerate() {
            let Some(n) = v.nics.get(i) else {
                item(app, g, &mut y, Dev::Nic(i as u64), &p.title, &format!("{:.1} Mb/s", p.value), "", &p.hist, p.max, C_NET, "", sensors::St::Ok);
                continue;
            };
            let tot: Vec<f32> = n.rx.iter().zip(&n.tx).map(|(a, b)| a + b).collect();
            let model = match n.kind {
                "virtual" => format!("Virtual · {}", n.desc),
                _ => format!("{} · {}", n.desc, link(n.link_bps)),
            };
            item(app, g, &mut y, Dev::Nic(n.luid), &n.name, &format!("↓ {} ↑ {} Mb/s", num(n.rx_bps / 1e6), num(n.tx_bps / 1e6)), &model, &tot, nice(vmax(&tot).max(1.0)), C_NET, "", sensors::St::Ok);
        }
    }
    g.unclip();
    app.vit.list_h = y - y0;
    app.vit.devs = devs;
    app.vit.dev_spans = spans;
    app.vit.list_view_h = area.h;
    let (lh, sc) = (app.vit.list_h, app.vit.list_scroll);
    super::nav::paint_bar(app, g, super::nav::BAR_VIT_LIST, area, lh, sc);
}

/// Up / Down / Page / Home / End move the device selection (as a click on the entry does)
/// and scroll the device list to it.
pub fn nav(app: &mut App, m: super::nav::Move) {
    let v = &app.vit;
    let cur = v.devs.iter().position(|d| *d == v.sel);
    let page = v.dev_spans.first().map_or(1, |&(_, h)| super::nav::page_rows(v.list_view_h, h + 2.0));
    let Some(i) = super::nav::target(v.devs.len(), cur, None, m, page) else { return };
    click(app, Hit::Device(i));
    if let Some(&(top, h)) = app.vit.dev_spans.get(i) {
        app.vit.list_scroll = super::nav::reveal(app.vit.list_scroll, top, h, app.vit.list_view_h);
    }
}

/// Per-column mean of the given rows of a rows x HIST block (NaN where none).
fn col_mean(block: &[f32], rows: &[usize]) -> Vec<f32> {
    (0..HIST)
        .map(|c| {
            let (mut s, mut n) = (0.0, 0);
            for &r in rows {
                let v = block.get(r * HIST + c).copied().unwrap_or(f32::NAN);
                if v.is_finite() {
                    s += v;
                    n += 1;
                }
            }
            if n > 0 { s / n as f32 } else { f32::NAN }
        })
        .collect()
}

// ---------- shared pieces ----------

struct Stat {
    label: String,
    value: String,
    unit: String,
    color: u32,
}
fn st(label: &str, value: String, unit: &str) -> Stat {
    Stat { label: label.into(), value, unit: unit.into(), color: TX1 }
}

/// Header panel: id (title + model), big value, stats (wrapping). Returns its bottom.
#[allow(clippy::too_many_arguments)]
fn header(app: &mut App, g: &mut Gfx, l: &L, y: f32, title: &str, model: &str, big: &str, unit: &str, stats: &[Stat]) -> f32 {
    let wd = &mut app.vit.wd;
    let idw = wd.get(g, title, f(11.0).b()).max(wd.get(g, model, f(13.0))).clamp(90.0, if l.narrow { 170.0 } else { 230.0 });
    let sep_x = l.x + 16.0 + idw + 16.0;
    let bw = wd.get(g, big, f(24.0).m().b());
    let uw = wd.get(g, unit, f(12.0));
    let big_x = sep_x + 16.0;
    let mut x = big_x + bw + 4.0 + uw + 26.0;
    let right = l.x + l.w - 14.0;
    let mut row = 0;
    let mut pos = Vec::with_capacity(stats.len());
    for s in stats {
        let vw = wd.get(g, &s.value, f(13.0).m().b()) + if s.unit.is_empty() { 0.0 } else { 3.0 + wd.get(g, &s.unit, f(11.0)) };
        let w = vw.max(wd.get(g, &s.label, f(11.0)));
        if x + w > right && x > big_x + 1.0 {
            row += 1;
            x = big_x;
        }
        pos.push((x, row, vw));
        x += w + 22.0;
    }
    let h = 64.0 + row as f32 * 40.0;
    let r = Rect::new(l.x, y, l.w, h);
    panel(g, r);
    g.text(&title.to_uppercase(), Rect::new(l.x + 16.0, y + 13.0, idw, 15.0), f(11.0).b(), TX2);
    g.text(model, Rect::new(l.x + 16.0, y + 29.0, idw, 20.0), f(13.0), TX1);
    g.fill(Rect::new(sep_x, y + 12.0, 1.0, h - 24.0), SEP);
    g.text(big, Rect::new(big_x, y + 10.0, bw + 4.0, 40.0), f(24.0).m().b(), TX1);
    g.text(unit, Rect::new(big_x + bw + 4.0, y + 22.0, uw + 4.0, 26.0), f(12.0), TX3);
    for (s, (x, row, vw)) in stats.iter().zip(pos) {
        let ry = y + 13.0 + row as f32 * 40.0;
        g.text(&s.label, Rect::new(x, ry, 200.0, 15.0), f(11.0), TX3);
        let vwid = app.vit.wd.get(g, &s.value, f(13.0).m().b());
        g.text(&s.value, Rect::new(x, ry + 16.0, vwid + 2.0, 20.0), f(13.0).m().b(), s.color);
        if !s.unit.is_empty() {
            g.text(&s.unit, Rect::new(x + vwid + 3.0, ry + 18.0, vw - vwid + 4.0, 18.0), f(11.0), TX3);
        }
    }
    r.b()
}

/// Panel with a 32 px title row (title, meta, divider). Returns the content top.
fn titled(app: &mut App, g: &mut Gfx, r: Rect, title: &str, meta: &str) -> f32 {
    titled_r(app, g, r, title, meta, 0.0)
}

/// `titled` with `reserve` px at the right kept free for controls (the meta text is clipped to it).
fn titled_r(app: &mut App, g: &mut Gfx, r: Rect, title: &str, meta: &str, reserve: f32) -> f32 {
    panel(g, r);
    let tw = app.vit.wd.get(g, title, f(11.0).b());
    g.text(title, Rect::new(r.x + 13.0, r.y, tw + 2.0, 32.0), f(11.0).b(), TX2);
    g.text(meta, Rect::new(r.x + 13.0 + tw + 9.0, r.y, (r.w - tw - 40.0 - reserve).max(0.0), 32.0), f(11.5), TX3);
    g.fill(Rect::new(r.x + 1.0, r.y + 32.0, r.w - 2.0, 1.0), SEP);
    r.y + 33.0
}

/// Segmented control, right edge at `right`, in a 32 px title row at `y`.
fn seg(app: &mut App, g: &mut Gfx, right: f32, y: f32, opts: &[(&str, usize)], on: usize) -> f32 {
    let ws: Vec<f32> = opts.iter().map(|o| app.vit.wd.get(g, o.0, f(11.5)) + 18.0).collect();
    let total: f32 = ws.iter().sum::<f32>() + 4.0;
    let r = Rect::new(right - total, y + 5.0, total, 22.0);
    g.round(r, 5.0, BG);
    g.round_stroke(r, 5.0, LN, 1.0);
    let mut x = r.x + 2.0;
    for (o, w) in opts.iter().zip(ws) {
        let b = Rect::new(x, r.y + 2.0, w, 18.0);
        let sel = o.1 == on;
        if sel {
            g.round(b, 4.0, PAN3);
        } else if app.hover == Some(Hit::VitSeg(o.1)) {
            g.round(b, 4.0, PAN2);
        }
        g.text(o.0, b, f(11.5).c(), if sel { TX1 } else { TX3 });
        app.hits.push((b, Hit::VitSeg(o.1)));
        x += w;
    }
    r.x
}

/// Timeline plot geometry inside a titled panel.
struct Plot {
    /// plot area (graph / heatmap x range)
    x: f32,
    w: f32,
    /// y-label column right edge, legend column
    lab_r: f32,
    leg: Rect,
}
fn plot_cols(l: &L, r: Rect, lab_w: f32) -> Plot {
    let leg_w = if l.narrow { 86.0 } else { 128.0 };
    let x = r.x + 12.0 + lab_w + 10.0;
    let leg = Rect::new(r.r() - 12.0 - leg_w, 0.0, leg_w, 0.0);
    Plot { x, w: leg.x - 12.0 - x, lab_r: x - 8.0, leg }
}

/// y gridlines + labels at `fr` (0 = bottom) over `[x, x + w]`.
fn ygrid(g: &mut Gfx, p: &Plot, r: Rect, marks: &[(f32, String)]) {
    for (fr, lab) in marks {
        let y = r.b() - fr * r.h;
        g.fill(Rect::new(p.x, y.round(), p.w, 1.0), SEP);
        g.text(lab, Rect::new(p.lab_r - 60.0, y - 7.0, 60.0, 14.0), f(10.0).m().r(), TX4);
    }
}
/// Faint 45 / 30 / 15 s columns.
fn xgrid(g: &mut Gfx, p: &Plot, y: f32, h: f32) {
    for k in 1..4 {
        let x = p.x + p.w * k as f32 / 4.0;
        g.fill_a(Rect::new(x.round(), y, 1.0, h), SEP, 0.9);
    }
}
fn xaxis(g: &mut Gfx, p: &Plot, y: f32) {
    let labs = ["60 s", "45", "30", "15", "now"];
    for (k, t) in labs.iter().enumerate() {
        let x = p.x + p.w * k as f32 / 4.0;
        let font = if k == 0 { f(10.0).m() } else if k == 4 { f(10.0).m().r() } else { f(10.0).m().c() };
        let r = if k == 0 { Rect::new(x, y, 40.0, 16.0) } else if k == 4 { Rect::new(x - 40.0, y, 40.0, 16.0) } else { Rect::new(x - 20.0, y, 40.0, 16.0) };
        g.text(t, r, font, TX4);
    }
}
/// Series inset by half a column so samples sit on cell centres (lines up
/// with the heatmap and the hover column).
fn inner(p: &Plot, y: f32, h: f32) -> Rect {
    let cw = p.w / HIST as f32;
    Rect::new(p.x + cw / 2.0, y, p.w - cw, h)
}
/// Legend row: swatch, label, value. Narrow: value under the label.
fn legend(g: &mut Gfx, l: &L, p: &Plot, y: f32, c: u32, label: &str, value: &str) -> f32 {
    let r = p.leg;
    g.round(Rect::new(r.x, y + 6.0, 8.0, 8.0), 2.0, c);
    g.text(label, Rect::new(r.x + 14.0, y, r.w - 14.0, 20.0), f(11.5), TX2);
    if l.narrow {
        g.text(value, Rect::new(r.x + 14.0, y + 17.0, r.w - 14.0, 18.0), f(12.5).m().b(), TX1);
        y + 38.0
    } else {
        g.text(value, Rect::new(r.x, y, r.w, 20.0), f(13.0).m().b().r(), TX1);
        y + 21.0
    }
}
fn note(g: &mut Gfx, p: &Plot, y: f32, label: &str, value: &str) {
    let w = g.measure(label, f(10.5));
    g.text(label, Rect::new(p.leg.x, y, w + 2.0, 15.0), f(10.5), TX3);
    g.text(value, Rect::new(p.leg.x + w + 4.0, y, p.leg.w - w - 4.0, 15.0), f(10.5).m(), TX2);
}

/// Hover column: vertical line from `y0` to `y1` and a readout box at `y0`.
fn cursor(app: &mut App, g: &mut Gfx, p: &Plot, y0: f32, y1: f32, text: impl FnOnce(usize) -> String) {
    app.vit.plot = Rect::new(p.x, y0, p.w, y1 - y0);
    let Some(c) = app.vit.hover_col else { return };
    let cw = p.w / HIST as f32;
    let x = (p.x + (c as f32 + 0.5) * cw).round();
    g.fill_a(Rect::new(x, y0, 1.0, y1 - y0), TX3, 0.85);
    let t = text(c);
    let tw = app.vit.wd.get(g, &t, f(11.0).m()) + 16.0;
    let bx = (x - tw / 2.0).clamp(p.x, (p.x + p.w - tw).max(p.x));
    let b = Rect::new(bx, y0 + 4.0, tw, 20.0);
    g.round(b, 4.0, PAN3);
    g.round_stroke(b, 4.0, LN, 1.0);
    g.text(&t, b, f(11.0).m().c(), TX1);
}

/// One line of a multi-line hover readout.
#[derive(Clone, Debug, PartialEq)]
struct TipLine {
    /// series colour swatch (the legend's colour for the same series)
    swatch: Option<u32>,
    label: String,
    value: String,
    /// the row the pointer is on (a heatmap block's cluster): drawn brighter
    hot: bool,
}

/// What the pointer reads at one timeline column: a time head, one line per series, an
/// optional note under them (what a shortened list leaves out).
#[derive(Clone, Debug, PartialEq)]
struct Tip {
    head: String,
    lines: Vec<TipLine>,
    foot: Option<String>,
}

const TIP_ROW: f32 = 17.0;
const TIP_PAD: f32 = 8.0;
/// widest series label before it ellipsises (long cluster names must not widen the box)
const TIP_LABEL_MAX: f32 = 150.0;

fn tip_y(anchor: f32, height: f32, clip: Rect) -> f32 {
    anchor.clamp(clip.y, (clip.b() - height).max(clip.y))
}

/// Register a timeline's hover area (`slot` 0 = main, 1 = second graph on the same time axis).
fn plot_area(app: &mut App, p: &Plot, slot: u8, y0: f32, y1: f32) {
    let r = Rect::new(p.x, y0, p.w, y1 - y0);
    match slot {
        0 => app.vit.plot = r,
        1 => app.vit.plot2 = r,
        _ => app.vit.plot3 = r,
    }
}

/// Hover column of a timeline: vertical line from `y0` to `y1` and a multi-line readout
/// beside it (right of the line, left when that would leave the plot). Width is capped to the
/// plot, labels ellipsise.
fn cursor_tip(app: &mut App, g: &mut Gfx, p: &Plot, slot: u8, y0: f32, y1: f32, tip: impl FnOnce(usize) -> Tip) {
    plot_area(app, p, slot, y0, y1);
    let Some(c) = app.vit.hover_col else { return };
    let cw = p.w / HIST as f32;
    let x = (p.x + (c as f32 + 0.5) * cw).round();
    g.fill_a(Rect::new(x, y0, 1.0, y1 - y0), TX3, 0.85);
    // graphs on one time axis share the cursor line; only the one the pointer is in reads out
    if app.vit.hover_slot != slot {
        return;
    }
    let t = tip(c);
    let (clip, px, pw) = (app.vit.clip, p.x, p.w);
    paint_tip(app, g, &t, p.w.max(80.0), |w, h| {
        let mut bx = x + 10.0;
        if bx + w > px + pw {
            bx = x - 10.0 - w;
        }
        (bx.max(px), tip_y(y0 + 4.0, h, clip))
    });
}

/// Paint a hover card: width capped to `max_w`, top-left from `place(w, h)`.
fn paint_tip(app: &mut App, g: &mut Gfx, t: &Tip, max_w: f32, place: impl FnOnce(f32, f32) -> (f32, f32)) {
    let (hf, lf, vf, ff) = (f(11.0).m(), f(11.5), f(11.5).m(), f(10.5));
    let sw = if t.lines.iter().any(|l| l.swatch.is_some()) { 14.0 } else { 0.0 };
    let wd = &mut app.vit.wd;
    // measured widths get a little slack: a text laid out to exactly its measure can ellipsise
    let head_w = wd.get(g, &t.head, hf) + 4.0;
    let lab_w = t.lines.iter().map(|l| wd.get(g, &l.label, lf)).fold(0.0f32, f32::max).min(TIP_LABEL_MAX);
    let val_w = t.lines.iter().map(|l| wd.get(g, &l.value, vf)).fold(0.0f32, f32::max) + 4.0;
    let foot_w = t.foot.as_ref().map_or(0.0, |s| wd.get(g, s, ff));
    let w = (head_w.max(sw + lab_w + 14.0 + val_w).max(foot_w) + TIP_PAD * 2.0).min(max_w);
    let rows_h = 22.0 + t.lines.len() as f32 * TIP_ROW + if t.foot.is_some() { 18.0 } else { 0.0 };
    let h = rows_h + 6.0;
    let (bx, by) = place(w, h);
    let b = Rect::new(bx, by, w, h);
    g.round(b, 4.0, PAN3);
    g.round_stroke(b, 4.0, LN, 1.0);
    let inner = b.inset(TIP_PAD, 0.0);
    g.text(&t.head, Rect::new(inner.x, b.y + 3.0, inner.w, 18.0), hf, TX3);
    let val_w = val_w.min(inner.w - sw - 24.0).max(0.0);
    let mut ly = b.y + 22.0;
    for l in &t.lines {
        if l.hot {
            // the selected-row look: ACC_BG fill and a 2 px ACC rail
            g.round(Rect::new(b.x + 3.0, ly, b.w - 6.0, TIP_ROW), 3.0, ACC_BG);
            g.fill(Rect::new(b.x + 3.0, ly + 2.0, 2.0, TIP_ROW - 4.0), ACC);
        }
        if let Some(c) = l.swatch {
            g.round(Rect::new(inner.x, ly + 5.0, 8.0, 8.0), 2.0, c);
        }
        let lw = (inner.w - sw - val_w - 10.0).max(0.0);
        g.text(&l.label, Rect::new(inner.x + sw, ly, lw, TIP_ROW), lf, if l.hot { TX1 } else { TX2 });
        g.text(&l.value, Rect::new(inner.r() - val_w, ly, val_w, TIP_ROW), vf.r(), TX1);
        ly += TIP_ROW;
    }
    if let Some(n) = &t.foot {
        g.text(n, Rect::new(inner.x, ly + 1.0, inner.w, 16.0), ff, TX3);
    }
}
/// Spark with a label + value row above it (side panels).
#[allow(clippy::too_many_arguments)]
fn spark_row(app: &mut App, g: &mut Gfx, r: Rect, label: &str, value: &str, unit: &str, vals: &[f32], c: u32) -> f32 {
    g.text(label, Rect::new(r.x, r.y, r.w * 0.5, 20.0), f(12.0), TX2);
    let uw = if unit.is_empty() { 0.0 } else { app.vit.wd.get(g, unit, f(11.0)) + 3.0 };
    g.text(unit, Rect::new(r.r() - uw + 3.0, r.y + 1.0, uw, 20.0), f(11.0), TX3);
    g.text(value, Rect::new(r.x + r.w * 0.4, r.y, r.w * 0.6 - uw, 20.0), f(13.0).m().b().r(), TX1);
    let sr = Rect::new(r.x, r.y + 24.0, r.w, 28.0);
    g.fill(sr, BG);
    let hi = vmax(vals);
    if hi > 0.0 {
        g.series(sr.inset(0.0, 1.0), vals, 0.0, hi * 1.15, c, 0.16, 1.1, false);
    }
    r.y + 62.0
}
/// Label / value row with a divider under it.
fn fact(g: &mut Gfx, x: f32, y: f32, w: f32, label: &str, value: &str) -> f32 {
    g.text(label, Rect::new(x, y, w * 0.45, 24.0), f(12.0), TX3);
    g.text(value, Rect::new(x + w * 0.3, y, w * 0.7, 24.0), f(12.0).m().r(), TX1);
    g.fill(Rect::new(x, y + 24.0, w, 1.0), SEP);
    y + 25.0
}
// ---------- consumers ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum By {
    Cpu,
    Mem,
    Gpu,
    Io,
    /// per-process network traffic (ETW, all adapters)
    Net,
}

impl By {
    fn id(self) -> u8 {
        match self {
            By::Cpu => 0,
            By::Mem => 1,
            By::Gpu => 2,
            By::Io => 3,
            By::Net => 4,
        }
    }
}

/// One consumer: a process or an app group, with the numbers its columns show
/// (text is made only for the rows on screen).
#[derive(Clone, Debug)]
struct Row {
    /// what the row IS across refreshes (held order, ghost rows)
    id: TopKey,
    /// `Snapshot.procs` index (per process) or `VitState.groups` index
    src: usize,
    /// process whose icon stands for the row
    pi: usize,
    count: usize,
    key: f64,
    cpu: f32,
    mem: u64,
    gpu: f32,
    io: f64,
    /// network bytes per second (By::Net)
    down: u64,
    up: u64,
    /// per-scope split of the row's traffic whatever the view (By::Net bars and tooltip)
    scopes: Scopes,
}

fn row_name<'a>(vit: &'a VitState, s: &'a Snapshot, r: &Row) -> &'a str {
    if vit.per_proc {
        s.procs.get(r.src).map(|p| p.meta.desc.as_deref().filter(|d| !d.is_empty()).unwrap_or(&p.name)).unwrap_or("")
    } else {
        vit.groups.get(r.src).map(|g| g.name.as_str()).unwrap_or("")
    }
}

fn row_text(vit: &VitState, s: &Snapshot, r: &Row, by: By) -> (String, String) {
    let eng = || {
        let e = if vit.per_proc { s.procs.get(r.src).and_then(|p| p.gpu_engine.as_ref()) } else { vit.groups.get(r.src).and_then(|g| g.gpu_engine.as_ref()) };
        e.map(|e| e.rsplit(" - ").next().unwrap_or(e).to_string()).unwrap_or_default()
    };
    match by {
        By::Cpu => (format!("{:.1}%", r.cpu), mem_s(r.mem)),
        By::Mem => (mem_s(r.mem), format!("{:.1}%", r.mem as f64 / s.mem.total.max(1) as f64 * 100.0)),
        By::Gpu => (format!("{:.1}%", r.gpu), eng()),
        By::Io => (format!("{} MB/s", num(r.io / 1e6)), format!("{:.1}%", r.cpu)),
        By::Net => (bits(r.down.saturating_add(r.up)), bits(r.down)),
    }
}

/// Bits-per-second text for a byte rate (adapter stats read in bits too).
fn bits(bytes_per_s: u64) -> String {
    let b = bytes_per_s as f64 * 8.0;
    if b >= 1e9 {
        format!("{:.2} Gb/s", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} Mb/s", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} kb/s", b / 1e3)
    } else if b > 0.0 {
        format!("{b:.0} b/s")
    } else {
        "0".into()
    }
}

fn blank_row(id: TopKey, src: usize, pi: usize, count: usize) -> Row {
    Row { id, src, pi, count, key: 0.0, cpu: 0.0, mem: 0, gpu: 0.0, io: 0.0, down: 0, up: 0, scopes: Scopes::default() }
}

/// Every candidate row for `by`, unordered, by identity.
fn candidates(vit: &VitState, s: &Snapshot, by: By) -> Vec<Row> {
    let key = |cpu: f32, mem: u64, gpu: f32, io: f64| match by {
        By::Cpu => cpu as f64,
        By::Mem => mem as f64,
        By::Gpu => gpu as f64,
        By::Io => io,
        By::Net => 0.0,
    };
    if by == By::Net {
        return net_candidates(vit, s);
    }
    if vit.per_proc {
        s.procs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.pid > 4)
            .map(|(i, p)| Row { key: key(p.cpu, p.mem, p.gpu, p.io_bps), cpu: p.cpu, mem: p.mem, gpu: p.gpu, io: p.io_bps, ..blank_row(TopKey::Proc(p.pid, p.create), i, i, 1) })
            .collect()
    } else {
        let trees: Vec<(&str, Row)> = vit
            .groups
            .iter()
            .enumerate()
            .filter(|(_, gr)| gr.key.0 > 4 && !gr.members.is_empty())
            .map(|(k, gr)| {
                let pi = group_icon(s, gr);
                (gr.name.as_str(), Row { key: key(gr.cpu, gr.mem, gr.gpu, gr.io), cpu: gr.cpu, mem: gr.mem, gpu: gr.gpu, io: gr.io, ..blank_row(TopKey::App(gr.name.to_lowercase()), k, pi, gr.members.len()) })
            })
            .collect();
        let mut v = merge_by_name(trees);
        for r in &mut v {
            r.key = key(r.cpu, r.mem, r.gpu, r.io);
        }
        v
    }
}

/// Process whose icon stands for a group: its root, else the first member.
fn group_icon(s: &Snapshot, gr: &procs::Group) -> usize {
    gr.members.iter().copied().find(|&i| s.procs.get(i).is_some_and(|p| p.pid == gr.key.0 && p.create == gr.key.1)).unwrap_or(gr.members[0])
}

/// Network rows: only processes the ETW feed saw moving bytes. Rates attach to
/// a process by (pid, create time); Apps merge the members of each group.
fn net_candidates(vit: &VitState, s: &Snapshot) -> Vec<Row> {
    let find = |pid: u32, create: i64| s.procs.iter().position(|p| p.pid == pid && p.create == create);
    // a LAN / WAN list holds only processes with traffic of that scope
    let view = vit.net_view;
    let seen = |n: &crate::netetw::NetRow| {
        let (d, u) = n.of(view);
        (view == crate::netetw::View::All || d.saturating_add(u) > 0).then_some((d, u))
    };
    if vit.per_proc {
        s.vit
            .net_top
            .rows
            .iter()
            .filter_map(|n| {
                let (down, up) = seen(n)?;
                let i = find(n.pid, n.create)?;
                Some(Row { key: down.saturating_add(up) as f64, down, up, scopes: Scopes::of(n), ..blank_row(TopKey::Proc(n.pid, n.create), i, i, 1) })
            })
            .collect()
    } else {
        let mut group_of = vec![usize::MAX; s.procs.len()];
        for (k, gr) in vit.groups.iter().enumerate() {
            for &m in &gr.members {
                if let Some(slot) = group_of.get_mut(m) {
                    *slot = k;
                }
            }
        }
        // sum per process tree first, then per display name like the other lists
        let mut per_group: HashMap<usize, ((u64, u64), Scopes)> = HashMap::new();
        for n in &s.vit.net_top.rows {
            let Some((down, up)) = seen(n) else { continue };
            let Some(i) = find(n.pid, n.create) else { continue };
            let k = group_of[i];
            if k == usize::MAX {
                continue;
            }
            let e = per_group.entry(k).or_default();
            e.0 .0 = e.0 .0.saturating_add(down);
            e.0 .1 = e.0 .1.saturating_add(up);
            e.1.add(&Scopes::of(n));
        }
        let mut ks: Vec<usize> = per_group.keys().copied().collect();
        ks.sort_unstable();
        let trees: Vec<(&str, Row)> = ks
            .into_iter()
            .map(|k| {
                let ((down, up), scopes) = per_group[&k];
                let gr = &vit.groups[k];
                (gr.name.as_str(), Row { key: down.saturating_add(up) as f64, down, up, scopes, ..blank_row(TopKey::App(gr.name.to_lowercase()), k, group_icon(s, gr), gr.members.len()) })
            })
            .collect();
        let mut v = merge_by_name(trees);
        for r in &mut v {
            r.key = r.down.saturating_add(r.up) as f64;
        }
        v
    }
}

/// A row that is still running but moved no bytes this interval (a held Net
/// row whose process went quiet is idle, not exited).
fn idle_row(vit: &VitState, s: &Snapshot, id: &TopKey) -> Option<Row> {
    match id {
        TopKey::Proc(pid, create) => {
            let i = s.procs.iter().position(|p| p.pid == *pid && p.create == *create)?;
            Some(blank_row(id.clone(), i, i, 1))
        }
        TopKey::App(name) => {
            let mut found: Option<Row> = None;
            for (k, gr) in vit.groups.iter().enumerate() {
                if gr.name.to_lowercase() == *name {
                    match &mut found {
                        Some(r) => r.count += gr.members.len(),
                        None => found = Some(blank_row(id.clone(), k, group_icon(s, gr), gr.members.len())),
                    }
                }
            }
            found
        }
    }
}

/// Spec of the explicit state a list is sorted / grouped by: a change applies
/// at once (see `order`).
fn list_spec(by: By, per_proc: bool, view: crate::netetw::View) -> u64 {
    // the scope view only means something for the Network list
    let v = if by == By::Net { view as u64 + 1 } else { 0 };
    v * 32 + by.id() as u64 * 2 + per_proc as u64
}

/// Consumers for `by`: values live, order held (see `order`). Only the top
/// `keep` are chosen when the order is (re)computed.
fn rows(app: &mut App, s: &Snapshot, by: By, keep: usize, panel: Rect) -> Vec<Slot<TopKey, Row>> {
    let (inside, busy) = (panel.contains(app.mx, app.my), app.down.is_some() || app.menu_open);
    arrange_rows(&mut app.vit, s, by, keep, order::now_ms(), inside, busy)
}

/// `rows` without the window state: `inside` = pointer in the panel, `busy` = button held or menu open.
fn arrange_rows(vit: &mut VitState, s: &Snapshot, by: By, keep: usize, now: u64, inside: bool, busy: bool) -> Vec<Slot<TopKey, Row>> {
    let spec = list_spec(by, vit.per_proc, vit.net_view);
    let id = by.id() * 2 + vit.per_proc as u8;
    if by == By::Net && s.vit.net_top.status != crate::netetw::Status::Live {
        // no live feed: nothing to order (a held list of stale rates would be a lie)
        vit.tops.remove(&id);
        return Vec::new();
    }
    if !vit.per_proc && vit.groups_seq != s.seq {
        vit.groups = procs::build_groups(s);
        vit.groups_seq = s.seq;
    }
    let all = candidates(vit, s, by);
    let mut ranked: Vec<(f64, String, TopKey)> = all.iter().map(|r| (r.key, row_name(vit, s, r).to_string(), r.id.clone())).collect();
    let cmp = |a: &(f64, String, TopKey), b: &(f64, String, TopKey)| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1));
    if ranked.len() > keep && keep > 0 {
        ranked.select_nth_unstable_by(keep - 1, cmp);
        ranked.truncate(keep);
    }
    ranked.sort_by(cmp);
    let live: Vec<TopKey> = ranked.into_iter().map(|x| x.2).collect();
    let mut by_id: HashMap<TopKey, Row> = all.into_iter().map(|r| (r.id.clone(), r)).collect();

    let mut st = vit.tops.remove(&id).unwrap_or_default();
    // an empty list shows nothing a new row could displace (see `Stable::restart_if_empty`)
    st.restart_if_empty();
    st.pointer(now, inside);
    st.active(now, busy);
    if vit.touch_at > 0 {
        st.touch(vit.touch_at);
    }
    let out = st.arrange(now, spec, live, |k| by_id.remove(k).or_else(|| if by == By::Net { idle_row(vit, s, k) } else { None })).slots;
    vit.tops.insert(id, st);
    // remember names for rows that may go ghost
    for sl in &out {
        if let Slot::Live(r) = sl {
            let n = row_name(vit, s, r).to_string();
            vit.top_names.insert(r.id.clone(), n);
        }
    }
    if vit.top_names.len() > 256 {
        let keep_keys: std::collections::HashSet<TopKey> = vit.tops.values().flat_map(|t| t.keys().iter().cloned()).collect();
        vit.top_names.retain(|k, _| keep_keys.contains(k));
    }
    out
}

/// Apps view: one row per display name (case-insensitive). `build_groups`
/// makes one group per process tree, so two hopper windows started apart, or
/// several Hayates, would each get a row of the same name. Sums CPU, memory,
/// GPU and I/O; ×N counts every process. The name and GPU engine come from
/// the tree using the most GPU, the icon from the busiest tree (by `key`).
fn merge_by_name(trees: Vec<(&str, Row)>) -> Vec<Row> {
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Row> = Vec::new();
    let mut best: Vec<(f64, f32)> = Vec::new();
    for (name, r) in trees {
        match at.get(&name.to_lowercase()) {
            Some(&j) => {
                let m = &mut out[j];
                if r.key > best[j].0 {
                    m.pi = r.pi;
                    best[j].0 = r.key;
                }
                if r.gpu > best[j].1 {
                    m.src = r.src;
                    best[j].1 = r.gpu;
                }
                m.count += r.count;
                m.cpu += r.cpu;
                m.mem += r.mem;
                m.gpu += r.gpu;
                m.io += r.io;
                m.down = m.down.saturating_add(r.down);
                m.up = m.up.saturating_add(r.up);
                m.scopes.add(&r.scopes);
            }
            None => {
                at.insert(name.to_lowercase(), out.len());
                best.push((r.key, r.gpu));
                out.push(r);
            }
        }
    }
    out
}

// ---------- network LOAD bars + row tooltip ----------

/// WAN reads amber next to the green LAN (the scope strip, the LOAD bars and the row tip agree).
const NET_WAN: u32 = C_DSK;
const NET_LAN: u32 = LAN_DN;
const NET_OTHER: u32 = TX4;

/// One Network row's bytes per second per remote scope, each (down, up).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Scopes {
    lan: (u64, u64),
    wan: (u64, u64),
    other: (u64, u64),
}

fn pair_sum(p: (u64, u64)) -> u64 {
    p.0.saturating_add(p.1)
}

impl Scopes {
    fn of(n: &crate::netetw::NetRow) -> Scopes {
        Scopes { lan: (n.lan_down, n.lan_up), wan: (n.wan_down, n.wan_up), other: n.other() }
    }
    fn add(&mut self, o: &Scopes) {
        for (a, b) in [(&mut self.lan, o.lan), (&mut self.wan, o.wan), (&mut self.other, o.other)] {
            a.0 = a.0.saturating_add(b.0);
            a.1 = a.1.saturating_add(b.1);
        }
    }
    fn total(&self) -> (u64, u64) {
        (self.lan.0.saturating_add(self.wan.0).saturating_add(self.other.0), self.lan.1.saturating_add(self.wan.1).saturating_add(self.other.1))
    }
}

/// What the LOAD cell of a Network row draws. Lengths are fractions (0..=1) of the busiest row's total.
#[derive(Clone, Copy, Debug, PartialEq)]
enum NetBars {
    /// no LAN / WAN traffic in the shown scope: an empty track
    Idle,
    /// one active scope: a single centred bar
    One(u32, f32),
    /// LAN above, WAN below
    Two(f32, f32),
}

/// Bars of a row. `maxt` = the busiest row's total bytes per second (all scopes, down + up), so lengths
/// compare across rows and views. Other never gets a bar; an inactive scope's bar is hidden.
fn net_bars(sc: &Scopes, view: crate::netetw::View, maxt: u64) -> NetBars {
    use crate::netetw::View;
    let frac = |v: u64| if maxt == 0 { 0.0 } else { (v as f64 / maxt as f64).min(1.0) as f32 };
    let (l, w) = (pair_sum(sc.lan), pair_sum(sc.wan));
    match view {
        View::Lan if l > 0 => NetBars::One(NET_LAN, frac(l)),
        View::Wan if w > 0 => NetBars::One(NET_WAN, frac(w)),
        View::Lan | View::Wan => NetBars::Idle,
        View::All => match (l > 0, w > 0) {
            (true, true) => NetBars::Two(frac(l), frac(w)),
            (true, false) => NetBars::One(NET_LAN, frac(l)),
            (false, true) => NetBars::One(NET_WAN, frac(w)),
            (false, false) => NetBars::Idle,
        },
    }
}

/// What a row counts toward the length of the bars: all scopes in the All view, only the shown scope in
/// the LAN / WAN views (so those views use their own busiest row as 100 %, not the all-scope one).
fn bar_basis(sc: &Scopes, view: crate::netetw::View) -> u64 {
    use crate::netetw::View;
    match view {
        View::All => pair_sum(sc.total()),
        View::Lan => pair_sum(sc.lan),
        View::Wan => pair_sum(sc.wan),
    }
}

fn scope_value(p: (u64, u64)) -> String {
    format!("↓ {}  ↑ {}", bits(p.0), bits(p.1))
}

/// The hover card of a Network row: app name (×N when grouped), a line per scope with traffic, then the total.
fn net_row_tip(name: &str, count: usize, sc: &Scopes) -> Tip {
    let mut lines = Vec::new();
    for (label, c, p) in [("LAN", NET_LAN, sc.lan), ("WAN", NET_WAN, sc.wan), ("Other", NET_OTHER, sc.other)] {
        if pair_sum(p) > 0 {
            lines.push(TipLine { swatch: Some(c), label: label.into(), value: scope_value(p), hot: false });
        }
    }
    lines.push(TipLine { swatch: None, label: "Total".into(), value: scope_value(sc.total()), hot: false });
    Tip { head: if count > 1 { format!("{name} ×{count}") } else { name.to_string() }, lines, foot: Some(NET_ROW_NOTE.into()) }
}

/// Is a Top row's name cut off by its column? (`text_w` = measured width, `avail` = the cell's width.)
/// Same slack as the Processes list's clipped-cell rule.
fn name_clipped(text_w: f32, avail: f32) -> bool {
    text_w > avail + 0.5
}

/// What a clipped Top-row name adds to the hover card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NameInfo {
    /// distinct exe names of an Apps row (first few)
    exes: Vec<String>,
    /// Apps row: process count
    count: usize,
    /// Processes row: its PID, and the exe name when the shown name is a description
    pid: Option<u32>,
}

/// Lines that carry the full-name details: exe name(s), process count (Apps) or PID (Processes).
fn name_lines(i: &NameInfo) -> Vec<TipLine> {
    let line = |label: &str, value: String| TipLine { swatch: None, label: label.into(), value, hot: false };
    let mut out = Vec::new();
    if !i.exes.is_empty() {
        out.push(line(if i.exes.len() > 1 { "Executables" } else { "Executable" }, i.exes.join(", ")));
    }
    if i.pid.is_none() && i.count > 1 {
        out.push(line("Processes", i.count.to_string()));
    }
    if let Some(p) = i.pid {
        out.push(line("PID", p.to_string()));
    }
    out
}

/// Hover card of a clipped name: the full display name as the head, then `name_lines`.
fn name_tip(name: &str, i: &NameInfo) -> Tip {
    Tip { head: name.to_string(), lines: name_lines(i), foot: None }
}

/// Distinct exe names (case-insensitive, first-seen order, at most 3 + "+N more") of a group's members.
fn exe_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut more = 0usize;
    for n in names {
        if out.iter().any(|o| o.eq_ignore_ascii_case(n)) {
            continue;
        }
        if out.len() < 3 {
            out.push(n.to_string());
        } else {
            more += 1;
        }
    }
    if more > 0 {
        out.push(format!("+{more} more"));
    }
    out
}

fn name_info(vit: &VitState, s: &Snapshot, row: &Row, shown: &str) -> NameInfo {
    if vit.per_proc {
        let p = s.procs.get(row.src);
        NameInfo { exes: p.filter(|p| &*p.name != shown).map(|p| vec![p.name.to_string()]).unwrap_or_default(), count: 1, pid: p.map(|p| p.pid) }
    } else {
        let exes = vit.groups.get(row.src).map(|g| exe_names(g.members.iter().filter_map(|&m| s.procs.get(m)).map(|p| &*p.name))).unwrap_or_default();
        NameInfo { exes, count: row.count, pid: None }
    }
}

/// Top-left of a pointer card of `size`: right of and below the pointer, flipped to the other side at the
/// right / bottom edge of `win`, never above `top` or left of 8.
fn tip_pos(m: (f32, f32), size: (f32, f32), win: (f32, f32), top: f32, bottom: f32) -> (f32, f32) {
    let mut x = m.0 + 14.0;
    if x + size.0 > win.0 - 8.0 {
        x = m.0 - 10.0 - size.0;
    }
    let mut y = m.1 + 18.0;
    if y + size.1 > bottom {
        y = m.1 - 10.0 - size.1;
    }
    (x.max(8.0), y.max(top))
}

/// LOAD cell of a Network row: the scope bars (or an empty track).
fn paint_net_bars(g: &mut Gfx, lx: f32, lw: f32, ry: f32, bars: NetBars) {
    let bar = |g: &mut Gfx, y: f32, h: f32, c: u32, fr: f32| {
        g.round(Rect::new(lx, y, lw, h), 2.0, TRACK);
        if fr > 0.0 {
            g.round(Rect::new(lx, y, (lw * fr).max(2.0), h), 2.0, c);
        }
    };
    match bars {
        NetBars::Idle => bar(g, ry + 9.0, 4.0, TRACK, 0.0),
        NetBars::One(c, fr) => bar(g, ry + 9.0, 4.0, c, fr),
        NetBars::Two(l, w) => {
            bar(g, ry + 7.0, 3.0, NET_LAN, l);
            bar(g, ry + 12.0, 3.0, NET_WAN, w);
        }
    }
}

/// "TOP BY ..." panel: two columns when wide. Returns its bottom.
#[allow(clippy::too_many_arguments)]
fn consumers(app: &mut App, g: &mut Gfx, s: &Snapshot, l: &L, r: Rect, by: By, title: &str, meta: &str, cols: (&str, &str, &str)) -> f32 {
    let net = by == By::Net;
    // the Network panel carries a second control (All / LAN / WAN): keep the title text out of its way
    let mut top = titled_r(app, g, r, title, meta, if net { 290.0 } else { 0.0 });
    let on = if app.vit.per_proc { 3 } else { 2 };
    let left = seg(app, g, r.r() - 10.0, r.y, &[("Apps", 2), ("Processes", 3)], on);
    if net {
        use crate::netetw::View;
        let on_v = match app.vit.net_view {
            View::All => 4,
            View::Lan => 5,
            View::Wan => 6,
        };
        seg(app, g, left - 8.0, r.y, &[("All", 4), ("LAN", 5), ("WAN", 6)], on_v);
        if s.vit.net_top.status == crate::netetw::Status::Live {
            top += net_scope_strip(app, g, s, r, top);
            if let Some(w) = net_warn(&s.vit.net_top) {
                g.circle(r.x + 17.0, top + 9.0, 3.0, WARN);
                g.text(&w, Rect::new(r.x + 28.0, top + 1.0, (r.w - 40.0).max(0.0), 16.0), f(11.0), TX3);
                top += 18.0;
            }
        }
    }
    // three value columns need more room per column
    let two = !l.narrow && r.w >= if net { 1060.0 } else { 760.0 };
    let ncol = if two { 2 } else { 1 };
    let rh = 22.0;
    // the Network list keeps a footnote line at the foot of its panel (what the rates count)
    let note_h = if net { NET_NOTE_H } else { 0.0 };
    let per = (((r.b() - top - 30.0 - note_h) / rh).floor() as usize).max(1);
    let all = rows(app, s, by, per * ncol, r);
    if net && all.is_empty() {
        net_state(app, g, s, r, top);
        return r.b();
    }
    let cw = (r.w - 2.0) / ncol as f32;
    let maxk = all.iter().filter_map(|x| if let Slot::Live(x) = x { Some(x.key) } else { None }).fold(0.0f64, f64::max).max(1e-9);
    // busiest row (the shown scope's down + up; all scopes in the All view): the length every LOAD bar is a fraction of
    let maxt = all.iter().filter_map(|x| if let Slot::Live(x) = x { Some(bar_basis(&x.scopes, app.vit.net_view)) } else { None }).max().unwrap_or(0);
    let mut row_tip: Option<Tip> = None;
    for c in 0..ncol {
        let x = r.x + 1.0 + c as f32 * cw;
        if c > 0 {
            g.fill(Rect::new(x - 1.0, top, 1.0, r.b() - top - 6.0), SEP);
        }
        let inner = Rect::new(x + 12.0, top, cw - 24.0, 24.0);
        let (lw, nw) = (46.0, 34.0);
        // value columns, right to left: (x, w)
        let (v3, v2w, v1w) = if net { (Some((inner.r() - 74.0, 74.0)), 74.0, 80.0) } else { (None, 66.0, 76.0) };
        let v2x = v3.map_or(inner.r(), |v| v.0 - 6.0) - v2w;
        let v1x = v2x - v1w - 6.0;
        let lx = v1x - lw - 10.0;
        let nx = lx - nw - 8.0;
        let hy = Rect::new(inner.x + 24.0, top + 2.0, 60.0, 22.0);
        g.text(cols.0, hy, f(10.5), TX4);
        g.text(cols.1, Rect::new(lx, top + 2.0, lw, 22.0), f(10.5), TX4);
        g.text(cols.2, Rect::new(v1x, top + 2.0, v1w, 22.0), f(10.5).r(), TX4);
        g.text(col4(by), Rect::new(v2x, top + 2.0, v2w, 22.0), f(10.5).r(), TX4);
        if let Some((v3x, v3w)) = v3 {
            g.text("↑ UP", Rect::new(v3x, top + 2.0, v3w, 22.0), f(10.5).r(), TX4);
        }
        for k in 0..per {
            let Some(slot) = all.get(c * per + k) else { break };
            let ry = top + 26.0 + k as f32 * rh;
            let rr = Rect::new(x + 1.0, ry, cw - 2.0, rh);
            if k % 2 == 1 {
                g.fill(rr, ZEBRA);
            }
            let row = match slot {
                Slot::Live(row) => row,
                Slot::Gone(id) => {
                    // exited while the order is held: the slot stays, dim, no values, inert
                    let name = app.vit.top_names.get(id).cloned().unwrap_or_default();
                    g.text(&name, Rect::new(inner.x + 24.0, ry, nx - inner.x - 28.0, rh), f(12.0), TX4);
                    let (ex, ew) = v3.unwrap_or((v2x, v2w));
                    g.text("exited", Rect::new(ex, ry, ew, rh), f(11.5).r(), TX4);
                    continue;
                }
            };
            let dim = row.key <= 0.0;
            let (v1, v2) = row_text(&app.vit, s, row, by);
            let name = row_name(&app.vit, s, row).to_string();
            // right-click target: the row as drawn (a hit region only on the row itself: not the
            // header, the gaps between panels, the graph or the scope strip)
            let hovered = register(app, s, row, &name, by, rr);
            if app.vit.pick.as_ref() == Some(&row.id) {
                g.fill(rr, ACC_BG);
            } else if hovered && !app.menu_open && app.cpu.is_none() && app.disallow.is_none() && app.peek.is_none() && app.setup.panel.is_none() {
                g.fill_a(rr, 0xffffff, 0.035);
                // full name when the column cuts it; the Network card keeps its traffic lines and gets them merged in
                let nw_avail = nx - inner.x - 28.0;
                let clipped = name_clipped(app.vit.wd.get(g, &name, f(12.0)), nw_avail);
                let info = clipped.then(|| name_info(&app.vit, s, row, &name));
                row_tip = match (net, info) {
                    (true, i) => {
                        let mut t = net_row_tip(&name, row.count, &row.scopes);
                        if let Some(i) = i {
                            t.lines.splice(0..0, name_lines(&NameInfo { count: 0, ..i }));
                        }
                        Some(t)
                    }
                    (false, Some(i)) => Some(name_tip(&name, &i)),
                    (false, None) => None,
                };
            }
            let ir = Rect::new(inner.x, ry + 3.0, 16.0, 16.0);
            if let Some(p) = s.procs.get(row.pi) {
                let svc = if p.name.eq_ignore_ascii_case("svchost.exe") { "svchost.exe" } else { "" };
                if !super::icons::draw(&mut app.icons, g, p.meta.path.as_deref().unwrap_or(svc), p.pid, ir) {
                    g.round(ir.inset(1.0, 1.0), 3.0, procs::name_color(&p.name));
                    let ch: String = name.chars().take(1).collect::<String>().to_uppercase();
                    g.text(&ch, ir, f(9.5).b().c(), BG);
                }
            }
            g.text(&name, Rect::new(inner.x + 24.0, ry, nx - inner.x - 28.0, rh), f(12.0), if dim { TX3 } else { TX1 });
            if row.count > 1 {
                g.text(&format!("×{}", row.count), Rect::new(nx, ry, nw, rh), f(10.5).m().r(), TX3);
            }
            if net {
                paint_net_bars(g, lx, lw, ry, net_bars(&row.scopes, app.vit.net_view, maxt));
            } else {
                let br = Rect::new(lx, ry + 9.0, lw, 4.0);
                g.fill(br, TRACK);
                let fr = (row.key / maxk) as f32;
                if fr > 0.0 {
                    g.fill(Rect::new(br.x, br.y, (br.w * fr).max(2.0), br.h), bar_c(by));
                }
            }
            g.text(&v1, Rect::new(v1x, ry, v1w, rh), f(11.5).m().b().r(), if dim { TX3 } else { TX1 });
            g.text(&v2, Rect::new(v2x, ry, v2w, rh), f(11.5).m().r(), if dim { TX4 } else { TX2 });
            if let Some((v3x, v3w)) = v3 {
                g.text(&bits(row.up), Rect::new(v3x, ry, v3w, rh), f(11.5).m().r(), if dim { TX4 } else { TX2 });
            }
        }
    }
    if net {
        let nr = Rect::new(r.x + 14.0, r.b() - NET_NOTE_H - 1.0, (r.w - 28.0).max(0.0), NET_NOTE_H - 2.0);
        let nf = f(10.5);
        let wd = &mut app.vit.wd;
        let note = pick_note(&NET_NOTES, nr.w, |t| wd.get(g, t, nf));
        g.text(note, nr, nf, TX3);
    }
    if let Some(t) = row_tip {
        let win = (app.w, app.h);
        let m = (app.mx, app.my);
        let top = app.vit.clip.y.max(TITLE_H + 4.0);
        let bottom = app.h - STATUS_H - 4.0;
        paint_tip(app, g, &t, win.0 - 16.0, |w, h| tip_pos(m, (w, h), win, top, bottom));
    }
    r.b()
}

// ---------- right-click target ----------

/// What a drawn live row stood for: its id, its representative process (the icon process) and
/// the list it was in. The membership of an Apps row is resolved from these when the menu opens.
fn top_hit(s: &Snapshot, row: &Row, name: &str, by: By, view: crate::netetw::View) -> TopHit {
    let rep = s.procs.get(row.pi).map(|p| (p.pid, p.create)).unwrap_or((0, 0));
    TopHit { id: row.id.clone(), rep, name: name.to_string(), by, view }
}

/// Make drawn row `row` (at `rr`) a right-click target: its hit region, cut to the visible detail
/// column, and what it stood for. True when the pointer is on it. Exited slots never get here.
fn register(app: &mut App, s: &Snapshot, row: &Row, name: &str, by: By, rr: Rect) -> bool {
    let Some(vr) = super::cpu::visible_rect(rr, app.vit.clip) else { return false };
    app.vit.top_hits.push(top_hit(s, row, name, by, app.vit.net_view));
    app.hits.push((vr, Hit::TopRow(app.vit.top_hits.len() - 1)));
    app.mx >= 0.0 && vr.contains(app.mx, app.my)
}

/// Right-click on Top row `i` of the last paint: what the context menu is for, fixed now,
/// and the row marked while it is open. None: not a live row any more (exited, replaced).
pub fn right_click(app: &mut App, i: usize) -> Option<Target> {
    let t = menu_target(app, i)?;
    app.vit.pick = app.vit.top_hits.get(i).map(|h| h.id.clone());
    Some(t)
}

fn menu_target(app: &App, i: usize) -> Option<Target> {
    if app.tab != Tab::Vitals {
        return None;
    }
    let h = app.vit.top_hits.get(i)?;
    top_target(h, app.vit.shown.as_ref()?, &app.snap)
}

/// The processes a Top row stood for in `shown` (the snapshot it was drawn from), checked
/// against `latest`: only those still running with the same create time are targets. A PID
/// that now names another process is not the row's process.
fn top_target(h: &TopHit, shown: &Snapshot, latest: &Snapshot) -> Option<Target> {
    let alive = |pid: u32, create: i64| latest.procs.iter().any(|p| p.pid == pid && p.create == create);
    let groups = procs::build_groups(shown);
    let (mut members, group): (Vec<(u32, i64, String)>, bool) = match &h.id {
        TopKey::Proc(pid, create) => {
            let p = shown.procs.iter().find(|p| p.pid == *pid && p.create == *create)?;
            (vec![(p.pid, p.create, p.name.to_string())], false)
        }
        TopKey::App(name) => (app_members(&groups, shown, h.by, h.view, name), true),
    };
    members.retain(|m| alive(m.0, m.1));
    if members.is_empty() {
        return None;
    }
    // the process actions reach the row's own representative, and only while it still runs
    let rep = members.iter().any(|m| (m.0, m.1) == h.rep).then(|| latest.procs.iter().find(|p| (p.pid, p.create) == h.rep)).flatten().map(|p| Rep {
        pid: p.pid,
        create: p.create,
        name: p.name.to_string(),
        path: p.meta.path.as_deref().map(String::from),
        cmd: p.meta.cmd.as_deref().map(String::from),
    });
    let anchor = rep.as_ref().map(|r| (r.pid, r.create)).unwrap_or((members[0].0, members[0].1));
    let show = show_in(&groups, shown, anchor, group);
    Some(Target { origin: Origin::Top, label: h.name.clone(), group, rep, members, show })
}

/// Every process of the Apps row `key` (lower-cased display name): the process trees the row
/// merged. In the Network list a tree with no bytes in the shown scope is not part of the row
/// (a quiet held row, which has none, stands for every tree of that name).
fn app_members(groups: &[procs::Group], s: &Snapshot, by: By, view: crate::netetw::View, key: &str) -> Vec<(u32, i64, String)> {
    let named: Vec<&procs::Group> = groups.iter().filter(|g| !g.members.is_empty() && g.name.to_lowercase() == key && (by == By::Net || g.key.0 > 4)).collect();
    let chosen: Vec<&procs::Group> = if by == By::Net {
        let moving: std::collections::HashSet<(u32, i64)> = s
            .vit
            .net_top
            .rows
            .iter()
            .filter(|n| view == crate::netetw::View::All || {
                let (d, u) = n.of(view);
                d.saturating_add(u) > 0
            })
            .map(|n| (n.pid, n.create))
            .collect();
        let with_bytes: Vec<&procs::Group> = named.iter().copied().filter(|g| g.members.iter().any(|&m| s.procs.get(m).is_some_and(|p| moving.contains(&(p.pid, p.create))))).collect();
        if with_bytes.is_empty() {
            named
        } else {
            with_bytes
        }
    } else {
        named
    };
    chosen.iter().flat_map(|g| g.members.iter()).filter_map(|&m| s.procs.get(m)).map(|p| (p.pid, p.create, p.name.to_string())).collect()
}

/// Where "Show in Processes" goes for process `id`: its group row (opened when it holds more
/// than this one process), or the process's own row inside it.
fn show_in(groups: &[procs::Group], s: &Snapshot, id: (u32, i64), group: bool) -> Option<ShowIn> {
    let pi = s.procs.iter().position(|p| (p.pid, p.create) == id)?;
    let g = groups.iter().find(|g| g.members.contains(&pi))?;
    Some(if g.members.len() == 1 {
        ShowIn { select: (g.key.0, g.key.1, true), expand: None }
    } else if group && id == g.key {
        ShowIn { select: (g.key.0, g.key.1, true), expand: Some(g.key) }
    } else {
        ShowIn { select: (id.0, id.1, false), expand: Some(g.key) }
    })
}

/// Live feed with no rows: idle, or traffic Hayate cannot read. Never "idle" while
/// events of an unknown layout arrive (a Windows update changing the payload).
fn net_live_empty(f: &crate::netetw::Feed, view: crate::netetw::View) -> (&'static str, String) {
    use crate::netetw::View;
    if f.bad == 0 && !f.rows.is_empty() && view != View::All {
        // traffic exists, none of this scope: say which list is empty, not that the machine is idle
        let head = if view == View::Lan { "No LAN traffic right now" } else { "No WAN traffic right now" };
        return (head, "Processes that only talk to other scopes show under All.".into());
    }
    if f.bad > 0 {
        ("Windows sent network events Hayate cannot read", format!("{} events in an unsupported format since the trace started, so this list may be missing traffic.", group_num(f.bad)))
    } else {
        ("No process traffic right now", "Only processes that sent or received data show here · loopback traffic is not counted.".into())
    }
}

/// Panel subtitle: what the numbers are.
const NET_META: &str = "all adapters · payload bytes";

/// Height of the footnote line under the Network Top rows.
const NET_NOTE_H: f32 = 16.0;

/// Footnote under the Network Top rows, longest first: the rates are the Kernel-Network transfer
/// sizes (application data), not wire bytes, so they read below the adapter totals, Up most (ACKs
/// and headers are not in those events). Shorter forms follow for narrow windows.
const NET_NOTES: [&str; 3] = [
    "App payload only (TCP/UDP data bytes): headers, ACKs and protocol overhead are not counted, so rates read below adapter totals, Up most.",
    "Payload only: headers, ACKs and overhead not counted, so rates read below adapter totals, Up most.",
    "Payload only · no headers or ACKs · below adapter totals",
];

/// Row tooltip footer.
const NET_ROW_NOTE: &str = "App payload only: no headers or ACKs";

/// The first of `notes` (longest first) that `measure`s within `avail`; the last one when none does
/// (it is then ellipsised by the text box).
fn pick_note<'a>(notes: &[&'a str], avail: f32, mut measure: impl FnMut(&str) -> f32) -> &'a str {
    notes.iter().copied().find(|n| measure(n) <= avail).or(notes.last().copied()).unwrap_or("")
}

/// Lost events and unreadable events both make the rates read low: said under the title, never dropped.
fn net_warn(f: &crate::netetw::Feed) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if f.lost > 0 {
        parts.push(format!("{} events lost, rates may read low", group_num(f.lost)));
    }
    if f.bad > 0 {
        parts.push(format!("{} events unreadable", group_num(f.bad)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// LAN / WAN / Other down and up, summed over every process of the live feed (all
/// adapters: not the router's WAN counters). The three always add up to the total.
/// Returns the strip height (including its separator).
fn net_scope_strip(app: &mut App, g: &mut Gfx, s: &Snapshot, r: Rect, top: f32) -> f32 {
    use crate::netetw::View;
    let t = s.vit.net_top.totals();
    let cw = (r.w - 26.0 - 30.0) / 3.0;
    let cells = [("LAN · PRIVATE", t.lan, Some(View::Lan), NET_LAN), ("WAN · PUBLIC", t.wan, Some(View::Wan), NET_WAN), ("OTHER", t.other, None, NET_OTHER)];
    for (i, (label, (d, u), v, sw)) in cells.into_iter().enumerate() {
        let x = r.x + 13.0 + i as f32 * cw;
        let on = v == Some(app.vit.net_view);
        g.round(Rect::new(x, top + 8.0, 8.0, 8.0), 2.0, sw);
        g.text(label, Rect::new(x + 14.0, top + 4.0, cw - 22.0, 14.0), f(10.5), if on { ACC_TX } else { TX4 });
        let c = if d.saturating_add(u) > 0 { TX1 } else { TX3 };
        g.text(&format!("↓ {}   ↑ {}", bits(d), bits(u)), Rect::new(x, top + 19.0, cw - 8.0, 16.0), f(11.5), c);
    }
    let ir = Rect::new(r.r() - 30.0, top + 9.0, 20.0, 20.0);
    let hov = app.hover == Some(Hit::VitInfo);
    g.text("\u{E946}", ir, Font { mono: false, ..f(13.0) }.icon().c(), if hov { TX1 } else { TX3 });
    app.hits.push((ir, Hit::VitInfo));
    if app.hovering(Hit::VitInfo) {
        app.set_tip(ir, net_scope_help());
    }
    g.fill(Rect::new(r.x + 1.0, top + 39.0, r.w - 2.0, 1.0), SEP);
    40.0
}

/// What the scope split does and does not say (shown as the (i) tip).
fn net_scope_help() -> Vec<String> {
    vec![
        "LAN / private vs WAN / public".into(),
        "Sorted by the remote end of each packet, on all adapters together. Payload bytes of the processes shown here, not your router's WAN counters.".into(),
        "LAN: private, link-local and limited broadcast addresses. WAN: public addresses. Other: multicast, loopback-adjacent, CGNAT, documentation, reserved and unclassified ranges, never counted as WAN.".into(),
        "A VPN, tunnel or relay shows its own endpoint, which can differ from the physical route, and a private address can be reached across a VPN.".into(),
        "Rates count application payload (TCP/UDP data bytes from the Kernel-Network provider). Headers, ACKs and other protocol overhead are not counted, so they read below adapter totals, Up most.".into(),
    ]
}

/// What to say in the Network Top list instead of rows (no fake zeros).
fn net_state(app: &mut App, g: &mut Gfx, s: &Snapshot, r: Rect, top: f32) {
    use crate::netetw::Status;
    let x = r.x + 26.0;
    let w = r.w - 52.0;
    let y = top + 14.0;
    let (head, body): (&str, String) = match &s.vit.net_top.status {
        Status::NeedsAdmin => ("Per-process traffic needs administrator rights", "Windows only lets an elevated program read network events per process. Adapter totals above work without it.".into()),
        Status::Unavailable(why) => ("Per-process traffic is unavailable", format!("{why} · Hayate retries automatically.")),
        Status::Collecting => ("Collecting the first interval…", "Rates appear after about a second of traffic data.".into()),
        Status::Off => ("Starting…", String::new()),
        Status::Live => net_live_empty(&s.vit.net_top, app.vit.net_view),
    };
    g.text(head, Rect::new(x, y, w, 20.0), f(12.0), TX2);
    if !body.is_empty() {
        g.text(&body, Rect::new(x, y + 22.0, w, 18.0), f(11.5), TX3);
    }
    if s.vit.net_top.status == Status::NeedsAdmin {
        let br = Rect::new(x, y + 54.0, 150.0, 28.0);
        let hov = app.hover == Some(Hit::VitAdmin);
        button(g, br, "Restart as admin", hov, true);
        app.hits.push((br, Hit::VitAdmin));
    }
}

fn bar_c(by: By) -> u32 {
    match by {
        By::Cpu => C_CPU,
        By::Mem => C_MEM,
        By::Gpu => C_GPU,
        By::Io => C_DSK,
        By::Net => C_NET,
    }
}
/// Fourth column header per metric.
fn col4(by: By) -> &'static str {
    match by {
        By::Cpu => "MEMORY",
        By::Mem => "% RAM",
        By::Gpu => "ENGINE",
        By::Io => "CPU",
        By::Net => "↓ DOWN",
    }
}

/// Height left down to `bottom` (the unscrolled view bottom), at least `min`.
fn rest(bottom: f32, y: f32, min: f32) -> f32 {
    (bottom - y).max(min)
}
/// Zero-padded history (`hist_vec`): leading zeros are "no sample yet".
fn lead_nan(v: &[f32]) -> Vec<f32> {
    let k = v.iter().position(|x| *x != 0.0).unwrap_or(v.len());
    let mut o = v.to_vec();
    o[..k].iter_mut().for_each(|x| *x = f32::NAN);
    o
}

// ---------- CPU ----------

fn cpu(app: &mut App, g: &mut Gfx, s: &Snapshot, l: &L, top: f32, view: Rect) -> f32 {
    use crate::sample::sys::Cluster;
    let v = &s.vit;
    let bottom = top + view.h;
    let nrows = v.cores.len() / HIST;
    let mut clusters: Vec<Cluster> = s.st.clusters.iter().filter(|c| !c.threads.is_empty() && c.threads.iter().all(|&t| t < nrows)).cloned().collect();
    if clusters.is_empty() && nrows > 0 {
        clusters.push(Cluster { name: "CPU".into(), role: String::new(), threads: (0..nrows).collect() });
    }
    let total = if nrows > 0 { col_mean(&v.cores, &(0..nrows).collect::<Vec<_>>()) } else { lead_nan(&s.cpu_hist) };
    let series: Vec<Vec<f32>> = clusters.iter().map(|c| col_mean(&v.cores, &c.threads)).collect();
    let multi = clusters.len() > 1;
    let ccol = |k: usize| if multi { CLUSTER_C[k % CLUSTER_C.len()] } else { C_CPU };

    // header
    let mut stats = Vec::new();
    if let Some(t) = s.cpu_temp.filter(|_| !s.cpu_temp_stale) {
        stats.push(Stat { label: "Package".into(), value: format!("{t:.0}°"), unit: String::new(), color: sensors::st_color(app.sensors.dev_state("cpu")) });
    } else if app.sensors.cpu_missing() {
        // PawnIO missing / needs admin / read failing: say so instead of hiding the stat
        stats.push(Stat { label: "Package".into(), value: "—".into(), unit: String::new(), color: TX3 });
    }
    if multi {
        for (c, ser) in clusters.iter().zip(&series) {
            let lab = if c.role.is_empty() { c.name.clone() } else { format!("{} · {}", c.name, c.role) };
            stats.push(st(&lab, pct(last(ser)), ""));
        }
    }
    stats.push(st("Processes", group_num(s.nprocs as u64), ""));
    stats.push(st("Threads", group_num(s.threads as u64), ""));
    let short = short_cpu(&s.st.cpu_name);
    let model = if s.st.cpu_name.starts_with("AMD") && !short.starts_with("AMD") { format!("AMD {short}") } else { short };
    let mut y = header(app, g, l, top, "CPU", &model, &format!("{:.0}", s.cpu), "%", &stats) + 8.0;
    y = sensors::paint_panel(app, g, s, l.geo(), y, view, sensors::SDev::Cpu) + 8.0;

    // fit graph + heatmap above a consumers panel of >= 8 rows
    let gaps = clusters.len().saturating_sub(1) as f32 * 8.0;
    let heat_at = |rh: f32| nrows as f32 * rh + gaps;
    let fixed = 33.0 + 10.0 + 10.0 + 26.0 + 8.0 + 242.0;
    let (hw_m, hw_f) = hwfacts::cpu_rows(s, s.cpu_temp.filter(|_| !s.cpu_temp_stale));
    let hw_p = hwfacts::plan_g(g, l.w - hwfacts::INSET * 2.0, l.narrow, &hw_m, &hw_f);
    let budget = bottom - y - fixed - hw_p.h;
    let (mut gh, mut rh) = (150.0f32, 11.0f32);
    if nrows > 0 && budget < gh + heat_at(rh) {
        rh = (((budget - 100.0).max(0.0) - gaps) / nrows as f32).floor().clamp(4.0, 11.0);
        gh = (budget - heat_at(rh)).clamp(90.0, 150.0);
    }
    // narrow legend rows are two lines (38 px each): the graph is at least as tall as the legend
    if l.narrow {
        let n = 1 + if multi { clusters.len() } else { 0 };
        gh = gh.max(n as f32 * 38.0);
    }
    let heat_h = if nrows > 0 { heat_at(rh) } else { 0.0 };
    let graph_h = 33.0 + 10.0 + gh + 10.0 + heat_h + 26.0;
    let r = Rect::new(l.x, y, l.w, graph_h + hw_p.h);
    let ct = titled(app, g, r, "LOAD · LAST 60 S", if l.narrow { "graph and threads share one time axis" } else { "graph and threads share one time axis · brighter = busier" });
    if nrows > 0 {
        let on = app.vit.bars as usize;
        seg(app, g, r.r() - 10.0, r.y, &[("Heat", 0), ("Bars", 1)], on);
    }
    let mut p = plot_cols(l, r, if l.narrow { 62.0 } else { 84.0 });
    let gr = Rect::new(p.x, ct + 10.0, p.w, gh);
    ygrid(g, &p, gr, &[(0.0, "0".into()), (0.5, "50".into()), (1.0, "100%".into())]);
    xgrid(g, &p, gr.y, if app.vit.bars { gh } else { gh + 10.0 + heat_h });
    let ir = inner(&p, gr.y, gr.h);
    g.series(ir, &total, 0.0, 100.0, C_CPU, 0.14, 1.6, false);
    if multi {
        for (k, ser) in series.iter().enumerate() {
            g.series(ir, ser, 0.0, 100.0, ccol(k), 0.07, 1.2, false);
        }
    }
    p.leg.y = gr.y;
    let mut ly = legend(g, l, &p, gr.y, C_CPU, "Total", &pct(last(&total)));
    if multi {
        for (k, (c, ser)) in clusters.iter().zip(&series).enumerate() {
            ly = legend(g, l, &p, ly, ccol(k), &c.name, &pct(last(ser)));
        }
    }
    if let Some((mx, avg)) = ring::stats(total.iter().copied()) {
        if gr.b() - 34.0 > ly {
            note(g, &p, gr.b() - 34.0, "peak 60 s", &format!("{mx:.0}%"));
            note(g, &p, gr.b() - 18.0, "avg 60 s", &format!("{avg:.0}%"));
        }
    }

    // per-thread heatmap / bars, one block per cluster
    let mut by = gr.b() + 10.0;
    let cw = p.w / HIST as f32;
    let mut hot: Option<usize> = None;
    for (k, c) in clusters.iter().enumerate() {
        let n = c.threads.len();
        let bh = n as f32 * rh;
        let x0 = r.x + 12.0;
        g.round(Rect::new(x0, by, 2.0, bh), 1.0, ccol(k));
        g.text(&c.name, Rect::new(x0 + 8.0, by, p.lab_r - x0 - 24.0, 16.0), f(11.5).b(), TX1);
        let cur = last(&series[k]);
        // a pointer on this cluster's heatmap block / bars marks its row in the readout
        if app.my >= by && app.my < by + bh {
            hot = Some(k);
        }
        if !l.narrow && bh >= 50.0 {
            let mut ty = by + 16.0;
            if !c.role.is_empty() {
                g.text(&c.role, Rect::new(x0 + 8.0, ty, p.lab_r - x0 - 24.0, 15.0), f(10.5), TX3);
                ty += 16.0;
            }
            g.text(&pct(cur), Rect::new(x0 + 8.0, ty, 50.0, 18.0), f(12.0).m().b(), TX1);
        }
        if app.vit.bars {
            let bw = p.w / n as f32;
            for (j, &t) in c.threads.iter().enumerate() {
                let br = Rect::new(p.x + j as f32 * bw + 1.0, by, (bw - 2.0).max(1.0), bh);
                g.fill(br, TRACK);
                if bw >= 14.0 {
                    g.text(&t.to_string(), Rect::new(br.x, br.y + 2.0, br.w, 12.0), f(9.5).m().c(), TX4);
                }
                let val = s.cores.get(t).copied().unwrap_or(f32::NAN);
                if val > 0.4 {
                    let hh = (bh * val / 100.0).max(1.0);
                    g.fill(Rect::new(br.x, br.b() - hh, br.w, hh), heat(val.max(30.0)));
                }
            }
            meter_bar(g, &p, by, 14.0, cur);
        } else {
            heat_block(app, g, s.seq, k, &v.cores, &c.threads, Rect::new(p.x, by, p.w, bh), cw, rh);
            for (j, &t) in c.threads.iter().enumerate() {
                let ry = by + j as f32 * rh;
                if rh >= 8.0 {
                    g.text(&t.to_string(), Rect::new(p.lab_r - 26.0, ry, 24.0, rh), f(9.5).m().r(), TX4);
                }
                let val = last(&v.cores[t * HIST..(t + 1) * HIST]);
                meter_bar(g, &p, ry, rh, val);
            }
        }
        by += bh + 8.0;
    }
    let hb = if nrows > 0 { by - 8.0 } else { gr.b() };
    if app.vit.bars {
        g.text("one bar per thread · current load", Rect::new(p.x, hb + 5.0, p.w, 16.0), f(10.5), TX4);
    } else {
        xaxis(g, &p, hb + 5.0);
    }
    let y1 = if app.vit.bars { gr.b() } else { hb };
    let names: Vec<String> = clusters.iter().map(|c| if c.role.is_empty() { c.name.clone() } else { format!("{} · {}", c.name, c.role) }).collect();
    let tip_series: Vec<TipSeries> = names.iter().zip(&series).enumerate().map(|(k, (n, v))| TipSeries { label: n, color: ccol(k), v }).collect();
    cursor_tip(app, g, &p, 0, gr.y, y1, |c| cpu_tip(c, &total, &tip_series, hot));
    hwfacts::paint(g, r, r.y + graph_h, &hw_p, &hw_m, &hw_f);
    y = r.b() + 8.0;
    let h = rest(bottom, y, 242.0);
    let meta = format!("{} · {} processes", if app.vit.per_proc { "per process" } else { "grouped by app" }, s.nprocs);
    consumers(app, g, s, l, Rect::new(l.x, y, l.w, h), By::Cpu, "TOP BY CPU", &meta, ("APP", "LOAD", "CPU"))
}

/// One cluster series of the CPU load graph, as the hover readout names it.
struct TipSeries<'a> {
    label: &'a str,
    color: u32,
    v: &'a [f32],
}

/// Clusters a hover readout lists in full; more than this shows the busiest few.
const TIP_CLUSTERS: usize = 4;

/// The CPU load graph's readout at column `c`: the Total, then every cluster's own value at
/// that second (a missing reading is a dash, never 0). With more than `TIP_CLUSTERS` clusters it
/// lists the busiest at that second, always including the one the pointer is on (`hot`), and the
/// note says how many are shown.
fn cpu_tip(c: usize, total: &[f32], series: &[TipSeries], hot: Option<usize>) -> Tip {
    let mut lines = vec![TipLine { swatch: Some(C_CPU), label: "Total".into(), value: pct(at(total, c)), hot: false }];
    let line = |k: usize| TipLine { swatch: Some(series[k].color), label: series[k].label.to_string(), value: pct(at(series[k].v, c)), hot: hot == Some(k) };
    let mut foot = None;
    if series.len() > 1 {
        if series.len() <= TIP_CLUSTERS {
            lines.extend((0..series.len()).map(line));
        } else {
            // busiest first at this second; no reading sorts last, equal readings keep cluster order
            let mut order: Vec<usize> = (0..series.len()).collect();
            let key = |k: usize| {
                let v = at(series[k].v, c);
                if v.is_finite() { v } else { f32::NEG_INFINITY }
            };
            order.sort_by(|&a, &b| key(b).total_cmp(&key(a)).then(a.cmp(&b)));
            let keep = TIP_CLUSTERS - 1;
            let mut shown: Vec<usize> = order.iter().copied().take(keep).collect();
            let mut pointer = false;
            if let Some(h) = hot.filter(|h| *h < series.len() && !shown.contains(h)) {
                shown[keep - 1] = h;
                pointer = true;
            }
            lines.extend(shown.iter().map(|&k| line(k)));
            foot = Some(format!("{} of {} clusters · busiest{}", shown.len(), series.len(), if pointer { " + pointer" } else { "" }));
        }
    }
    Tip { head: ago(c), lines, foot }
}

/// One cluster's heatmap as a bitmap at device resolution, cell gaps baked in
/// (one fill per gap line cost ~160 draw calls a frame). Rebuilt only when the
/// sample or the size changes; hover repaints reuse it.
#[allow(clippy::too_many_arguments)]
fn heat_block(app: &mut App, g: &mut Gfx, seq: u64, k: usize, cores: &[f32], threads: &[usize], r: Rect, cw: f32, rh: f32) {
    const GAP: u16 = u16::MAX;
    let sc = g.dpi / 96.0;
    let (x0, x1) = ((r.x * sc).round(), (r.r() * sc).round());
    let (y0, y1) = ((r.y * sc).round(), (r.b() * sc).round());
    let (w, h) = ((x1 - x0).max(1.0) as u32, (y1 - y0).max(1.0) as u32);
    let dr = Rect::new(x0 / sc, y0 / sc, w as f32 / sc, h as f32 / sc);
    let slot = k % app.vit.keys.len();
    let key = app.vit.keys[slot].clone();
    let stamp = (seq, w, h);
    if app.vit.stamps[slot] == stamp && g.grid_ready(&key, w, h) {
        g.grid(&key, w, h, None, dr);
        return;
    }
    // device pixel -> cell index (GAP on a 1 DIP gap before each boundary)
    let gap = sc.round().max(1.0);
    let map = |len: u32, origin: f32, start: f32, cells: usize, step: f32, gaps: bool| -> Vec<u16> {
        let mut m = vec![GAP; len as usize];
        let edge = |i: usize| (((start + i as f32 * step).round()) * sc).round() - origin;
        for i in 0..cells {
            let a = if i == 0 { 0.0 } else { edge(i) };
            let b = if i + 1 == cells { len as f32 } else { edge(i + 1) - if gaps { gap } else { 0.0 } };
            let (a, b) = (a.clamp(0.0, len as f32) as usize, b.clamp(0.0, len as f32) as usize);
            if a < b {
                m[a..b].fill(i as u16);
            }
        }
        m
    };
    let cols = map(w, x0, r.x, HIST, cw, cw >= 4.0);
    let rows = map(h, y0, r.y, threads.len(), rh, rh >= 5.0);
    let pan = PAN | 0xff00_0000;
    let mut px = vec![pan; (w * h) as usize];
    let mut line = vec![pan; w as usize];
    let mut built = GAP;
    for (y, &row) in rows.iter().enumerate() {
        if row == GAP {
            continue;
        }
        if built != row {
            let t = threads[row as usize];
            let cells = &cores[t * HIST..(t + 1) * HIST];
            for (d, &c) in line.iter_mut().zip(&cols) {
                *d = if c == GAP { pan } else { heat(cells[c as usize]) | 0xff00_0000 };
            }
            built = row;
        }
        px[y * w as usize..(y + 1) * w as usize].copy_from_slice(&line);
    }
    g.grid(&key, w, h, Some(&px), dr);
    app.vit.stamps[slot] = stamp;
}

/// Current-load mini bar (+ % when the row is tall enough) in the legend column.
fn meter_bar(g: &mut Gfx, p: &Plot, y: f32, h: f32, v: f32) {
    let tw = if h >= 9.0 { 38.0 } else { 0.0 };
    let br = Rect::new(p.leg.x, y + ((h - 5.0) / 2.0).max(0.0), p.leg.w - tw - 4.0, 5.0f32.min(h - 1.0).max(2.0));
    // plain rects: 24+ of these a frame, rounded ones cost a geometry each
    g.fill(br, TRACK);
    if v.is_finite() && v > 0.5 {
        g.fill(Rect::new(br.x, br.y, (br.w * v / 100.0).clamp(2.0, br.w), br.h), C_CPU);
    }
    if tw > 0.0 {
        g.text(&pct(v), Rect::new(p.leg.r() - tw, y, tw, h), f(10.0).m().r(), if v >= 65.0 { TX1 } else { TX3 });
    }
}

// ---------- Memory ----------

fn mem(app: &mut App, g: &mut Gfx, s: &Snapshot, l: &L, top: f32, view: Rect) -> f32 {
    let v = &s.vit;
    let m = &s.mem;
    let bottom = top + view.h;
    let used = m.total.saturating_sub(m.avail);
    let mut stats = vec![
        st("In use", format!("{:.0}%", used as f64 / m.total.max(1) as f64 * 100.0), ""),
        st("Available", format!("{:.1}", gb(m.avail)), "GB"),
        st("Committed", format!("{:.1} / {:.0}", gb(m.commit), gb(m.commit_limit)), "GB"),
        st("Cached", format!("{:.1}", gb(m.cached)), "GB"),
    ];
    if let Some(c) = v.compressed {
        stats.push(st("Compressed", format!("{:.1}", gb(c)), "GB"));
    }
    let mut y = header(app, g, l, top, "Memory", &s.st.ram_desc, &format!("{:.1}", gb(used)), &format!("/ {:.0} GB", gb(m.total)), &stats) + 8.0;

    // usage timeline
    let comp_h = 146.0;
    let (hw_m, hw_f) = hwfacts::mem_rows(s);
    let hw_p = hwfacts::plan_g(g, l.w - hwfacts::INSET * 2.0, l.narrow, &hw_m, &hw_f);
    let gh = (bottom - y - (33.0 + 10.0 + 26.0 + 8.0) - hw_p.h - comp_h - 8.0 - 242.0).clamp(if l.narrow { 140.0 } else { 100.0 }, 150.0);
    let graph_h = 33.0 + 10.0 + gh + 26.0;
    let r = Rect::new(l.x, y, l.w, graph_h + hw_p.h);
    let ct = titled(app, g, r, "USAGE · LAST 60 S", "in use, with commit charge and installed RAM");
    let mut p = plot_cols(l, r, if l.narrow { 56.0 } else { 84.0 });
    let gr = Rect::new(p.x, ct + 10.0, p.w, gh);
    let hi = ((gb(m.commit_limit).max(gb(m.total)) / 10.0).ceil() * 10.0).max(1.0) as f32;
    let tot = gb(m.total) as f32;
    ygrid(g, &p, gr, &[(0.0, "0".into()), (0.5, format!("{:.0}", hi / 2.0)), (tot / hi, format!("{tot:.0}")), (1.0, format!("{hi:.0} GB"))]);
    xgrid(g, &p, gr.y, gh);
    let ir = inner(&p, gr.y, gr.h);
    let inuse = lead_nan(&s.mem_hist);
    g.series(ir, &inuse, 0.0, hi, C_MEM, 0.18, 1.5, false);
    g.dash_h(p.x, p.x + p.w, (gr.b() - tot / hi * gr.h).round(), 4.0, 3.0, 0x5a6372, 0.9);
    g.series(ir, &v.commit, 0.0, hi, MEM_COMMIT, 0.0, 1.2, false);
    p.leg.y = gr.y;
    let mut ly = legend(g, l, &p, gr.y, C_MEM, "In use", &format!("{:.1} GB", gb(used)));
    ly = legend(g, l, &p, ly, MEM_COMMIT, "Commit", &format!("{:.1} GB", gb(m.commit)));
    ly = legend(g, l, &p, ly, 0x5a6372, "Installed", &format!("{tot:.1} GB"));
    legend(g, l, &p, ly, INSTALLED, "Limit", &format!("{:.1} GB", gb(m.commit_limit)));
    xaxis(g, &p, gr.b() + 5.0);
    let commit = v.commit.clone();
    cursor(app, g, &p, gr.y, gr.b(), |c| format!("{} · in use {:.1} GB · commit {:.1} GB", ago(c), at(&inuse, c), at(&commit, c)));
    hwfacts::paint(g, r, r.y + graph_h, &hw_p, &hw_m, &hw_f);
    y = r.b() + 8.0;

    // composition
    let r = Rect::new(l.x, y, l.w, comp_h);
    let mut meta = format!("{:.1} GB usable", gb(m.total));
    if s.st.ram_installed > m.total {
        meta += &format!(" · {:.1} GB hardware reserved", gb(s.st.ram_installed - m.total));
    }
    let ct = titled(app, g, r, "COMPOSITION", &meta);
    let x0 = r.x + 14.0;
    let w = r.w - 28.0;
    let segs: Vec<(u32, &str, u64, &str)> = match v.lists {
        Some(ls) => {
            let inuse = m.total.saturating_sub(ls.free + ls.standby + ls.modified);
            vec![
                (C_MEM, "In use", inuse, "processes, drivers, OS"),
                (MEM_MOD, "Modified", ls.modified, "must be written before reuse"),
                (MEM_STBY, "Standby", ls.standby, "cache, reusable at once"),
                (TRACK, "Free", ls.free, "zeroed, unused"),
            ]
        }
        None => vec![(C_MEM, "In use", used, "processes, drivers, OS"), (TRACK, "Available", m.avail, "standby + free")],
    };
    let bar = Rect::new(x0, ct + 14.0, w, 20.0);
    g.round(bar, 3.0, TRACK);
    let sum: u64 = segs.iter().map(|x| x.2).sum::<u64>().max(1);
    let mut bx = bar.x;
    for (c, _, b, _) in &segs {
        let sw = bar.w * *b as f32 / sum as f32;
        if sw >= 1.0 {
            g.fill(Rect::new(bx, bar.y, (sw - 2.0).max(1.0), bar.h), *c);
        }
        bx += sw;
    }
    let colw = w / segs.len() as f32;
    for (k, (c, lab, b, desc)) in segs.iter().enumerate() {
        let cx = x0 + k as f32 * colw;
        let ty = bar.b() + 12.0;
        g.round(Rect::new(cx, ty + 5.0, 9.0, 9.0), 2.0, if *c == TRACK { 0x3a4250 } else { *c });
        g.text(lab, Rect::new(cx + 15.0, ty, colw - 20.0, 19.0), f(12.0), TX2);
        let vs = format!("{:.1}", gb(*b));
        let vw = app.vit.wd.get(g, &vs, f(16.0).m().b());
        g.text(&vs, Rect::new(cx + 15.0, ty + 18.0, vw + 2.0, 24.0), f(16.0).m().b(), TX1);
        g.text("GB", Rect::new(cx + 18.0 + vw, ty + 22.0, 30.0, 20.0), f(11.0), TX3);
        g.text(desc, Rect::new(cx + 15.0, ty + 41.0, colw - 20.0, 16.0), f(11.0), TX3);
    }
    y = r.b() + 8.0;
    let h = rest(bottom, y, 242.0);
    let meta = if app.vit.per_proc { "private working set · per process" } else { "private working set · grouped" };
    consumers(app, g, s, l, Rect::new(l.x, y, l.w, h), By::Mem, "TOP BY MEMORY", meta, ("APP", "SHARE", "MEMORY"))
}

// ---------- GPU ----------

#[allow(clippy::too_many_arguments)]
fn gpu(app: &mut App, g: &mut Gfx, s: &Snapshot, l: &L, top: f32, view: Rect, i: usize) -> f32 {
    let bottom = top + view.h;
    let Some(raw) = s.gpu_raw.get(i) else { return top };
    let empty = crate::sample::VGpu { util: vec![], vram: vec![], clock: vec![], names: vec![], eng: vec![] };
    let vg = s.vit.gpus.get(i).unwrap_or(&empty);
    let mut stats = Vec::new();
    if let Some(t) = raw.temp {
        stats.push(Stat { label: "Temp".into(), value: format!("{t:.0}°"), unit: String::new(), color: sensors::st_color(app.sensors.dev_state(&sensors::SensorsView::gpu_key(i))) });
    }
    if let Some(c) = raw.clock_mhz {
        stats.push(st("Clock", format!("{:.2}", c as f32 / 1000.0), "GHz"));
    }
    if let Some(p) = raw.power_pct {
        stats.push(st("Power", format!("{p:.0}"), "% of limit"));
    }
    if let Some(f) = raw.fan_rpm {
        stats.push(st("Fan", group_num(f as u64), "rpm"));
    }
    stats.push(st("VRAM", format!("{:.1} / {:.0}", gb(raw.vram_used), gb(raw.vram_total)), "GB"));
    let mut y = header(app, g, l, top, &format!("GPU {i}"), &raw.name, &format!("{:.0}", raw.util), "%", &stats) + 8.0;
    y = sensors::paint_panel(app, g, s, l.geo(), y, view, sensors::SDev::Gpu(i)) + 8.0;

    let eng_h = 140.0;
    let (hw_m, hw_f) = hwfacts::gpu_rows(raw);
    let hw_p = hwfacts::plan_g(g, l.w - hwfacts::INSET * 2.0, l.narrow, &hw_m, &hw_f);
    let gh = (bottom - y - (33.0 + 10.0 + 26.0 + 8.0) - hw_p.h - eng_h - 8.0 - 300.0).clamp(100.0, 230.0);
    let graph_h = 33.0 + 10.0 + gh + 26.0;
    let r = Rect::new(l.x, y, l.w, graph_h + hw_p.h);
    let ct = titled(app, g, r, "UTILISATION · LAST 60 S", "busiest engine, with dedicated memory");
    let mut p = plot_cols(l, r, if l.narrow { 56.0 } else { 84.0 });
    let gr = Rect::new(p.x, ct + 10.0, p.w, gh);
    ygrid(g, &p, gr, &[(0.0, "0".into()), (0.5, "50".into()), (1.0, "100%".into())]);
    xgrid(g, &p, gr.y, gh);
    let ir = inner(&p, gr.y, gr.h);
    g.series(ir, &vg.util, 0.0, 100.0, C_GPU, 0.16, 1.5, false);
    g.series(ir, &vg.vram, 0.0, 100.0, GPU_VRAM, 0.0, 1.2, false);
    p.leg.y = gr.y;
    let ly = legend(g, l, &p, gr.y, C_GPU, "Load", &format!("{:.0}%", raw.util));
    legend(g, l, &p, ly, GPU_VRAM, "VRAM", &pct(last(&vg.vram)));
    xaxis(g, &p, gr.b() + 5.0);
    cursor(app, g, &p, gr.y, gr.b(), |c| format!("{} · load {} · VRAM {}", ago(c), pct(at(&vg.util, c)), pct(at(&vg.vram, c))));
    hwfacts::paint(g, r, r.y + graph_h, &hw_p, &hw_m, &hw_f);
    y = r.b() + 8.0;

    // engines
    let r = Rect::new(l.x, y, l.w, eng_h);
    let ct = titled(app, g, r, "ENGINES", "per engine type, 60 s");
    let n = vg.names.len();
    if n == 0 || vg.eng.len() < n * HIST {
        g.text("Engine loads show after the next sample", Rect::new(r.x + 14.0, ct + 10.0, r.w - 28.0, 20.0), f(12.0), TX3);
    } else {
        let k = n.min(((r.w - 2.0) / 140.0).floor().max(1.0) as usize);
        let cw = (r.w - 2.0) / k as f32;
        for j in 0..k {
            let x = r.x + 1.0 + j as f32 * cw;
            if j > 0 {
                g.fill(Rect::new(x, ct, 1.0, r.b() - ct - 1.0), SEP);
            }
            let row = &vg.eng[j * HIST..(j + 1) * HIST];
            g.text(&vg.names[j], Rect::new(x + 12.0, ct + 8.0, cw - 70.0, 20.0), f(11.5), TX2);
            g.text(&pct(last(row)), Rect::new(x + cw - 62.0, ct + 8.0, 50.0, 20.0), f(12.0).m().b().r(), TX1);
            let gr = Rect::new(x + 12.0, ct + 34.0, cw - 24.0, r.b() - ct - 46.0);
            g.fill(Rect::new(gr.x, gr.b(), gr.w, 1.0), SEP);
            g.series(gr, row, 0.0, 100.0, C_GPU, 0.16, 1.1, false);
        }
    }
    y = r.b() + 8.0;

    // consumers | memory + sensors
    let h = rest(bottom, y, 300.0);
    let side = if l.narrow { 240.0 } else { 300.0 };
    let lw = l.w - side - 8.0;
    let meta = format!("all GPUs · {}", if app.vit.per_proc { "per process" } else { "grouped by app" });
    consumers(app, g, s, l, Rect::new(l.x, y, lw, h), By::Gpu, "TOP BY GPU", &meta, ("APP", "LOAD", "GPU"));
    let r = Rect::new(l.x + lw + 8.0, y, side, h);
    let ct = titled(app, g, r, "MEMORY · CLOCK", "60 s");
    let x = r.x + 14.0;
    let w = r.w - 28.0;
    let mut sy = ct + 10.0;
    let mem_row = |g: &mut Gfx, sy: &mut f32, lab: &str, used: u64, total: u64, c: u32| {
        g.text(lab, Rect::new(x, *sy, w * 0.5, 20.0), f(12.0).b(), TX1);
        g.text(&format!("{:.1} / {:.1} GB", gb(used), gb(total)), Rect::new(x + w * 0.3, *sy, w * 0.7, 20.0), f(12.0).m().r(), TX1);
        let br = Rect::new(x, *sy + 24.0, w, 7.0);
        g.round(br, 3.0, TRACK);
        if total > 0 && used > 0 {
            g.round(Rect::new(br.x, br.y, (br.w * used as f32 / total as f32).clamp(4.0, br.w), br.h), 3.0, c);
        }
        *sy += 42.0;
    };
    mem_row(g, &mut sy, "Dedicated", raw.vram_used, raw.vram_total, C_GPU);
    if raw.shared_total > 0 {
        mem_row(g, &mut sy, "Shared", raw.shared_used, raw.shared_total, GPU_VRAM);
    }
    // temperature, power and fan live in the Sensors panel now
    let rows: [(&str, &[f32], String, &str); 1] = [("Clock", &vg.clock, raw.clock_mhz.map(|c| group_num(c as u64)).unwrap_or_default(), "MHz")];
    for (lab, vals, val, unit) in rows {
        if val.is_empty() || sy + 62.0 > r.b() {
            continue;
        }
        sy = spark_row(app, g, Rect::new(x, sy, w, 60.0), lab, &val, unit, vals, C_GPU);
    }
    r.b()
}

// ---------- Disk ----------

#[allow(clippy::too_many_arguments)]
fn disk(app: &mut App, g: &mut Gfx, s: &Snapshot, l: &L, top: f32, view: Rect, num_: u32) -> f32 {
    let bottom = top + view.h;
    let Some(di) = s.vit.disks.iter().position(|d| d.num == num_) else { return top };
    let d = &s.vit.disks[di];
    let mut stats = vec![
        st("Read", num(d.read_bps / 1e6), "MB/s"),
        st("Write", num(d.write_bps / 1e6), "MB/s"),
        st("Response", d.resp_ms.map(|r| if r >= 10.0 { format!("{r:.0}") } else { format!("{r:.1}") }).unwrap_or_else(|| "—".into()), "ms"),
    ];
    if !d.vols.is_empty() {
        let (fr, tot) = d.vols.iter().fold((0, 0), |a, v| (a.0 + v.free, a.1 + v.total));
        stats.push(st("Free", format!("{:.0} / {:.0}", fr as f64 / 1e9, tot as f64 / 1e9), "GB"));
    }
    match (d.temp, s.disks.get(di)) {
        (Some(t), Some(p)) => stats.push(Stat { label: "Temp".into(), value: format!("{t:.0}°"), unit: String::new(), color: sensors::st_color(app.sensors.dev_state(&p.title)) }),
        _ => stats.push(Stat { label: "Temp".into(), value: "—".into(), unit: String::new(), color: TX3 }),
    }
    let title = if d.letters.is_empty() { format!("Disk {}", d.num) } else { format!("Disk {} ({})", d.num, d.letters) };
    let model = format!("{} · {}", if d.model.is_empty() { "Disk" } else { &d.model }, d.bus);
    let mut y = header(app, g, l, top, &title, &model, &format!("{:.0}", d.active), "% active", &stats) + 8.0;
    y = sensors::paint_panel(app, g, s, l.geo(), y, view, sensors::SDev::Disk(di)) + 8.0;

    // activity: active time above, read up / write down below
    let (hw_m, hw_f) = hwfacts::disk_rows(d, !d.letters.is_empty());
    let hw_p = hwfacts::plan_g(g, l.w - hwfacts::INSET * 2.0, l.narrow, &hw_m, &hw_f);
    let room = bottom - y - (33.0 + 10.0 + 28.0 + 26.0 + 8.0) - hw_p.h - 300.0;
    let k = (room / 290.0).clamp(0.55, 1.0);
    let (h1, h2) = ((150.0 * k).round(), (140.0 * k).round());
    let graph_h = 33.0 + 10.0 + h1 + 28.0 + h2 + 26.0;
    let r = Rect::new(l.x, y, l.w, graph_h + hw_p.h);
    let ct = titled(app, g, r, "ACTIVITY · LAST 60 S", "active time above, transfer rate below (read up, write down)");
    let mut p = plot_cols(l, r, if l.narrow { 56.0 } else { 84.0 });
    let ar = Rect::new(p.x, ct + 10.0, p.w, h1);
    ygrid(g, &p, ar, &[(0.0, "0".into()), (1.0, "100%".into())]);
    let mr = Rect::new(p.x, ar.b() + 28.0, p.w, h2);
    xgrid(g, &p, ar.y, mr.b() - ar.y);
    g.series(inner(&p, ar.y, ar.h), &d.act, 0.0, 100.0, C_DSK, 0.2, 1.5, false);
    let scale = nice(vmax(&d.read).max(vmax(&d.write)).max(1.0));
    let mid = (mr.y + mr.h / 2.0).round();
    g.fill(Rect::new(p.x, mid, p.w, 1.0), LN);
    g.series(inner(&p, mr.y, mid - mr.y), &d.read, 0.0, scale, C_DSK, 0.2, 1.3, false);
    g.series(inner(&p, mid + 1.0, mr.b() - mid - 1.0), &d.write, 0.0, scale, DSK_W, 0.2, 1.3, true);
    for (yy, t) in [(mr.y + 6.0, "read"), (mid, "0"), (mr.b() - 6.0, "write")] {
        g.text(t, Rect::new(p.lab_r - 60.0, yy - 7.0, 60.0, 14.0), f(10.0).m().r(), TX4);
    }
    p.leg.y = ar.y;
    legend(g, l, &p, ar.y, C_DSK, "Active", &format!("{:.0}%", d.active));
    if let Some((mx, _)) = ring::stats(d.act.iter().copied()) {
        note(g, &p, ar.y + if l.narrow { 40.0 } else { 24.0 }, "peak 60 s", &format!("{mx:.0}%"));
    }
    let ly = legend(g, l, &p, mr.y, C_DSK, "Read", &format!("{} MB/s", num(d.read_bps / 1e6)));
    let ly = legend(g, l, &p, ly, DSK_W, "Write", &format!("{} MB/s", num(d.write_bps / 1e6)));
    note(g, &p, ly + 4.0, "scale", &format!("{} MB/s", num(scale as f64)));
    xaxis(g, &p, mr.b() + 5.0);
    cursor(app, g, &p, ar.y, mr.b(), |c| format!("{} · {} active · R {} W {} MB/s", ago(c), pct(at(&d.act, c)), num(at(&d.read, c).max(0.0) as f64), num(at(&d.write, c).max(0.0) as f64)));
    hwfacts::paint(g, r, r.y + graph_h, &hw_p, &hw_m, &hw_f);
    y = r.b() + 8.0;

    // consumers | drive
    let h = rest(bottom, y, 300.0);
    // narrow: the drive panel goes under the list (side by side the list is ~120 px wide and its columns collide)
    let (lw, side) = if l.narrow { (l.w, l.w) } else { (l.w - 300.0 - 8.0, 300.0) };
    consumers(app, g, s, l, Rect::new(l.x, y, lw, h), By::Io, "TOP BY DISK I/O", "all disks combined", ("APP", "LOAD", "I/O"));
    let r = if l.narrow { Rect::new(l.x, y + h + 8.0, side, h) } else { Rect::new(l.x + lw + 8.0, y, side, h) };
    let nv = d.vols.len();
    let ct = titled(app, g, r, "DRIVE", &match nv {
        0 => String::new(),
        1 => "1 volume".into(),
        n => format!("{n} volumes"),
    });
    let x = r.x + 14.0;
    let w = r.w - 28.0;
    let mut sy = ct + 10.0;
    if d.vols.is_empty() && !d.letters.is_empty() {
        g.text("reading volumes…", Rect::new(x, sy, w, 20.0), f(12.0), TX3);
        sy += 28.0;
    }
    for vol in &d.vols {
        let lab = if vol.label.is_empty() { format!("{}:  {}", vol.letter, vol.fs) } else { format!("{}:  {} · {}", vol.letter, vol.label, vol.fs) };
        g.text(&lab, Rect::new(x, sy, w * 0.5, 20.0), f(12.0), TX2);
        let used = vol.total.saturating_sub(vol.free);
        g.text(&format!("{} used · {} free", size(used).trim_end_matches(" GB"), size(vol.free).trim_end_matches(" GB")), Rect::new(x + w * 0.35, sy, w * 0.65, 20.0), f(11.5).m().r(), TX1);
        let br = Rect::new(x, sy + 23.0, w, 7.0);
        g.round(br, 3.0, TRACK);
        if vol.total > 0 {
            g.round(Rect::new(br.x, br.y, (br.w * used as f32 / vol.total as f32).clamp(4.0, br.w), br.h), 3.0, C_DSK);
        }
        sy += 42.0;
    }
    let resp = d.resp_ms.map(|r| if r >= 10.0 { format!("{r:.0}") } else { format!("{r:.1}") }).unwrap_or_else(|| "—".into());
    sy = spark_row(app, g, Rect::new(x, sy, w, 60.0), "Avg response", &resp, "ms", &d.resp, C_DSK);
    sy = spark_row(app, g, Rect::new(x, sy, w, 60.0), "Queue length", &d.queue.to_string(), "", &d.qlen, C_DSK);
    sy += 4.0;
    // capacity, type, firmware, system disk and page file: the facts footer under the graph
    let (warn, crit, ty) = sensors::disk_limits(d.bus);
    fact(g, x, sy, w, "Warm / hot at", &format!("{warn:.0} / {crit:.0} °C ({ty})"));
    r.b()
}

// ---------- Network ----------

/// Down / up colours of the LAN source (the green family: a hue the blue Throughput graph and
/// the CPU / memory / disk series don't use here).
const LAN_DN: u32 = C_GPU;
const LAN_UP: u32 = GPU_VRAM;
/// Combined graph colours of (Total, LAN, WAN): blue as on the adapter graph, the green LAN
/// family, and the amber disk family for WAN; the up direction takes each family's dim shade.
/// The graph's controls are `VitSeg` 7 (Combined), 8 (Adapter), 9 (Down), 10 (Up); Top's
/// All / LAN / WAN filter keeps 4..=6.
fn combined_colors(up: bool) -> [u32; 3] {
    if up { [NET_UP, LAN_UP, DSK_W] } else { [C_NET, LAN_DN, C_DSK] }
}
/// Height of one titled graph panel around its plot (title row, gaps, axis row).
const NET_PANEL_PAD: f32 = 33.0 + 10.0 + 26.0;
/// Width of the all-adapters connection counts beside the adapter facts.
const NET_SIDE: f32 = 300.0;

/// Plot height of the throughput graph from the vertical room the window leaves for it (the page
/// scrolls when there is less than the floor).
fn net_plot_height(avail: f32) -> f32 {
    avail.clamp(120.0, 230.0)
}

/// Height of the unified details surface (adapter facts, plus the all-adapters connection counts
/// beside them, or below them when narrow) and of its adapter part.
fn net_facts_h(adapter_rows: usize, stacked: bool) -> (f32, f32) {
    let adapter = 33.0 + 8.0 + adapter_rows as f32 * 25.0 + 12.0;
    // title row, three counts, the "system-wide" note
    let conns = 33.0 + 8.0 + 3.0 * 25.0 + 22.0 + 10.0;
    if stacked { (adapter + conns, adapter) } else { (adapter.max(conns), adapter) }
}

/// One line of the throughput graph.
struct NetLine<'a> {
    /// legend label (short: the legend column is narrow) and hover readout label
    legend: &'static str,
    tip: &'static str,
    color: u32,
    v: &'a [f32],
    /// current rate (Mb/s); `None` = no valid measurement now
    cur: Option<f32>,
}

/// One throughput graph: what it draws, how it is named and coloured. The first line gets the
/// area fill, as on the CPU graph's Total.
struct NetPanel<'a> {
    title: &'static str,
    meta: &'static str,
    /// source named in the hover readout
    label: &'static str,
    lines: Vec<NetLine<'a>>,
    /// a selected-adapter graph: only this one compares with the link speed
    adapter: bool,
}

/// The graph of the chosen mode. Adapter: this NIC's down and up. Combined: Total (all traffic),
/// LAN and WAN of all adapters for one direction, on one scale.
fn net_panel<'a>(combined: bool, up: bool, n: &'a crate::sample::VNic, v: &'a crate::sample::Vit) -> NetPanel<'a> {
    use crate::netscope::Graph;
    let title = "THROUGHPUT · LAST 60 S";
    if !combined {
        let a = graph_src(Graph::Adapter, n, v);
        let cur = a.cur;
        return NetPanel {
            title,
            meta: a.meta,
            label: a.label,
            lines: vec![
                NetLine { legend: "Down", tip: "↓ Down", color: C_NET, v: a.down, cur: cur.map(|c| c.0) },
                NetLine { legend: "Up", tip: "↑ Up", color: NET_UP, v: a.up, cur: cur.map(|c| c.1) },
            ],
            adapter: true,
        };
    }
    let col = combined_colors(up);
    let line = |graph, legend, tip, color| {
        let s = graph_src(graph, n, v);
        NetLine { legend, tip, color, v: if up { s.up } else { s.down }, cur: s.cur.map(|c| if up { c.1 } else { c.0 }) }
    };
    NetPanel {
        title,
        meta: if up { "all adapters · up · total, LAN, WAN · auto scale" } else { "all adapters · down · total, LAN, WAN · auto scale" },
        label: if up { "all adapters · up" } else { "all adapters · down" },
        lines: vec![line(Graph::All, "Total", "Total", col[0]), line(Graph::Lan, "LAN", "LAN", col[1]), line(Graph::Wan, "WAN", "WAN", col[2])],
        adapter: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn net_plot(app: &mut App, g: &mut Gfx, l: &L, feed: &crate::netetw::Feed, link_bps: u64, r: Rect, gh: f32, sp: &NetPanel, marks: Option<&offenders::Marks>) {
    let ct = titled_r(app, g, r, sp.title, sp.meta, 330.0);
    let left = seg(app, g, r.r() - 10.0, r.y, &[("Combined", 7), ("Adapter", 8)], if app.vit.net_combined { 7 } else { 8 });
    if app.vit.net_combined {
        seg(app, g, left - 8.0, r.y, &[("Down", 9), ("Up", 10)], if app.vit.net_up { 10 } else { 9 });
    }
    let mut p = plot_cols(l, r, if l.narrow { 56.0 } else { 84.0 });
    let gr = Rect::new(p.x, ct + 10.0, p.w, gh);
    let none = sp.lines.iter().map(|ln| finite_n(ln.v)).sum::<usize>() == 0;
    let peak = sp.lines.iter().map(|ln| vmax(ln.v)).fold(0.0, f32::max);
    let scale = nice(peak.max(0.1));
    if none {
        ygrid(g, &p, gr, &[(0.0, "0".into())]);
    } else {
        ygrid(g, &p, gr, &[(0.0, "0".into()), (0.5, num(scale as f64 / 2.0)), (1.0, format!("{} Mb/s", num(scale as f64)))]);
    }
    xgrid(g, &p, gr.y, gh);
    let ir = inner(&p, gr.y, gr.h);
    for (k, ln) in sp.lines.iter().enumerate() {
        g.series(ir, ln.v, 0.0, scale, ln.color, if k == 0 { 0.16 } else { 0.0 }, 1.5, false);
    }
    p.leg.y = gr.y;
    let shown = |v: Option<f32>| v.map_or("—".to_string(), |v| format!("{} Mb/s", num(v as f64)));
    let mut ly = gr.y;
    for ln in &sp.lines {
        ly = legend(g, l, &p, ly, ln.color, ln.legend, &shown(ln.cur));
    }
    let peak_t = if none { "—".to_string() } else { format!("{} Mb/s", num(peak as f64)) };
    let share = link_share(sp.adapter, peak, link_bps);
    // the legend comes first; a short plot drops the notes it has no room for
    let free = gr.b() - ly;
    if free >= 36.0 {
        note(g, &p, gr.b() - 34.0, if l.narrow { "peak" } else { "peak 60 s" }, &peak_t);
        match &share {
            Some(sh) => note(g, &p, gr.b() - 18.0, "of link", sh),
            // an all-adapters scope is never compared with one adapter's link speed
            None if !sp.adapter => note(g, &p, gr.b() - 18.0, "all adapters", ""),
            None => {}
        }
    } else if free >= 18.0 {
        note(g, &p, gr.b() - 16.0, if l.narrow { "peak" } else { "peak 60 s" }, &peak_t);
    }
    if !sp.adapter {
        scope_notice(app, g, feed, &gr, none);
    }
    xaxis(g, &p, gr.b() + 5.0);
    // the failed-connect / retransmit marks sit under the axis; the cursor runs through them
    let end = match marks {
        Some(m) => {
            offenders::strip(app, g, &p, gr.b() + 24.0, m);
            gr.b() + 24.0 + offenders::MARKS_H
        }
        None => gr.b(),
    };
    if none {
        // nothing measured in the whole window: no crosshair readout of dashes over the message
        plot_area(app, &p, 0, gr.y, end);
    } else {
        let tip: Vec<TipSeries> = sp.lines.iter().map(|ln| TipSeries { label: ln.tip, color: ln.color, v: ln.v }).collect();
        cursor_tip(app, g, &p, 0, gr.y, end, |c| net_tip(c, sp.label, &tip));
    }
}

/// Feed the incident tracker the verdict of this Network paint. Runs only while the Network detail
/// is drawn (that is also the only time the counters behind the verdict are read); writes at most
/// one local Watch event per incident, no toast.
fn track_incident(app: &mut App, s: &Snapshot, ov: &offenders::View, verdict: &crate::sample::nethealth::Verdict) {
    let Some(ft) = crate::features::get() else { return };
    let bad = verdict.lv != crate::sample::nethealth::Lv::Ok && verdict.label != "Checking…";
    let culprit = ov.culprit();
    let path = culprit.and_then(|r| s.procs.get(r.pi)).and_then(|p| p.meta.path.as_deref());
    let obs = offenders::incident_obs(bad, &verdict.label, culprit, path);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let clock = crate::ui::watch::clock;
    match app.vit.incident.observe(now, &obs) {
        Some(crate::netincident::Action::Open { start, info }) => {
            let detail = info.detail(start, None, &clock);
            let id = ft.watch.net_incident_open(&info, start, detail);
            app.vit.incident.opened(id);
        }
        Some(crate::netincident::Action::Close { id, start, end, info }) => {
            ft.watch.net_incident_close(id, &info.detail(start, Some(end), &clock));
        }
        None => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn nic(app: &mut App, g: &mut Gfx, s: &Snapshot, l: &L, top: f32, view: Rect, luid: u64) -> f32 {
    let bottom = top + view.h;
    let v = &s.vit;
    let Some(n) = v.nics.iter().find(|n| n.luid == luid) else { return top };
    let addr = v.addrs.iter().find(|a| a.luid == luid);
    let mut stats = vec![st("Down", num(n.rx_bps / 1e6), "Mb/s"), st("Up", num(n.tx_bps / 1e6), "Mb/s"), st("Link", link(n.link_bps).replace(" Gb/s", "").replace(" Mb/s", ""), if n.link_bps >= 1_000_000_000 { "Gb/s" } else { "Mb/s" })];
    if let Some(a) = addr.filter(|a| !a.ipv4.is_empty()) {
        stats.push(st("IPv4", a.ipv4.split(' ').next().unwrap_or("").to_string(), ""));
    }
    stats.push(st("Session ↓ / ↑", format!("{:.1} / {:.1}", gb(n.rx_total), gb(n.tx_total)), "GB"));
    let mut y = header(app, g, l, top, &n.name, &n.desc, &num((n.rx_bps + n.tx_bps) / 1e6), "Mb/s", &stats) + 8.0;

    // one throughput graph (Combined Total / LAN / WAN, or the selected adapter), the unified details surface,
    // then TOP BY NETWORK; what the window leaves goes to the graph
    let adapter_rows = 4 + addr.map_or(0, |a| {
        [&a.ipv4, &a.ipv6, &a.mac].iter().filter(|v| !v.is_empty()).count()
            + usize::from(!a.gateway.is_empty() || !a.dns.is_empty())
    });
    let stacked = l.narrow;
    let (facts_h, adapter_h) = net_facts_h(adapter_rows, stacked);
    // title + scope strip (40) + header + 8 rows + payload footnote + padding
    let net_h = 33.0 + 40.0 + 30.0 + 8.0 * 22.0 + NET_NOTE_H + 10.0;
    // passive network health (verdict, tiles) from the counters the sampler diffed
    let pr = nethealth::pr(v, addr);
    let ov = offenders::view(app, s);
    let top = ov.culprit();
    let top_ip = top.map(|r| r.ip.to_string()).unwrap_or_default();
    let offender = top.and_then(|r| Some(crate::sample::nethealth::Offender { app: &r.name, ip: &top_ip, port: r.port, lan: r.scope == crate::netetw::Scope::Lan, per_min: r.per_min(), kind: r.kind? }));
    let verdict = nethealth::verdict(n, &v.health, &pr, offender, matches!(ov.state, offenders::State::Live));
    track_incident(app, s, &ov, &verdict);
    let tl = nethealth::tiles(n, &v.health);
    let ptl = nethealth::probe_tiles(&pr);
    let lay = nethealth::layout(app, g, l, &verdict, tl.len(), &pr, ptl.len(), offenders::height(&ov));
    // the failed-connect marks need the live ETW feed
    let marks_h = if matches!(ov.state, offenders::State::Live) { offenders::MARKS_H } else { 0.0 };
    let avail = bottom - y - NET_PANEL_PAD - marks_h - nethealth::LINK_USE_H - 8.0 - lay.h - 8.0 - facts_h - 8.0 - net_h;
    let gh = net_plot_height(avail);

    let main = net_panel(app.vit.net_combined, app.vit.net_up, n, v);
    // the Link use bars sit at the foot of the throughput panel
    let r = Rect::new(l.x, y, l.w, NET_PANEL_PAD + gh + marks_h + nethealth::LINK_USE_H);
    net_plot(app, g, l, &v.net_top, n.link_bps, r, gh, &main, (marks_h > 0.0).then_some(&ov.marks));
    nethealth::link_use(app, g, l, Rect::new(r.x, r.b() - nethealth::LINK_USE_H, r.w, nethealth::LINK_USE_H), n);
    y = r.b() + 8.0;
    let hr = Rect::new(l.x, y, l.w, lay.h);
    nethealth::health(app, g, l, hr, &lay, &verdict, n, &v.health, &tl, &ptl, &pr, s, &ov);
    y = hr.b() + 8.0;

    // adapter facts of the selected NIC | connection counts of the whole system, one surface
    let r = Rect::new(l.x, y, l.w, facts_h);
    let side = if stacked { 0.0 } else { NET_SIDE };
    titled_r(app, g, r, "ADAPTER", &format!("connected · {}", n.kind), side);
    let (x, w) = (r.x + 14.0, r.w - 28.0 - side);
    let mut sy = r.y + 33.0 + 8.0;
    sy = fact(g, x, sy, w, "Adapter", &n.desc);
    sy = fact(g, x, sy, w, "Link", &link(n.link_bps));
    if let Some(a) = addr {
        let gw_dns = match (a.gateway.is_empty(), a.dns.is_empty()) {
            (false, false) if a.gateway != a.dns => format!("{} · {}", a.gateway, a.dns),
            (false, _) => a.gateway.clone(),
            (true, false) => a.dns.clone(),
            _ => String::new(),
        };
        for (lab, val) in [("IPv4", &a.ipv4), ("Gateway · DNS", &gw_dns), ("IPv6", &a.ipv6), ("MAC", &a.mac)] {
            if !val.is_empty() {
                sy = fact(g, x, sy, w, lab, val);
            }
        }
    }
    sy = fact(g, x, sy, w, "Session ↓", &format!("{:.1} GB", gb(n.rx_total)));
    fact(g, x, sy, w, "Session ↑", &format!("{:.1} GB", gb(n.tx_total)));
    // the connections section: same title type, a rule between it and the adapter facts
    let (cx, cy, cw) = if stacked {
        let cy = r.y + adapter_h;
        g.fill(Rect::new(r.x + 1.0, cy - 1.0, r.w - 2.0, 1.0), SEP);
        (r.x, cy, r.w)
    } else {
        let cx = r.r() - NET_SIDE;
        g.fill(Rect::new(cx, r.y + 1.0, 1.0, r.h - 2.0), SEP);
        (cx, r.y, NET_SIDE)
    };
    let tw = app.vit.wd.get(g, "CONNECTIONS", f(11.0).b());
    g.text("CONNECTIONS", Rect::new(cx + 14.0, cy, tw + 2.0, 32.0), f(11.0).b(), TX2);
    g.text("all adapters", Rect::new(cx + 14.0 + tw + 9.0, cy, (cw - tw - 36.0).max(0.0), 32.0), f(11.5), TX3);
    if stacked {
        g.fill(Rect::new(r.x + 1.0, cy + 32.0, r.w - 2.0, 1.0), SEP);
    }
    let (x, w) = (cx + 14.0, cw - 28.0);
    match v.conns {
        Some(c) => {
            let mut sy = cy + 33.0 + 8.0;
            sy = fact(g, x, sy, w, "TCP established", &group_num(c.tcp_established as u64));
            sy = fact(g, x, sy, w, "TCP listening", &group_num(c.tcp_listening as u64));
            sy = fact(g, x, sy, w, "UDP endpoints", &group_num(c.udp as u64));
            g.text("system-wide, not this adapter only", Rect::new(x, sy + 4.0, w, 16.0), f(11.0), TX3);
        }
        None => g.text("reading…", Rect::new(x, cy + 33.0 + 10.0, w, 20.0), f(12.0), TX3),
    }
    let nr = Rect::new(l.x, r.b() + 8.0, l.w, net_h);
    consumers(app, g, s, l, nr, By::Net, "TOP BY NETWORK", NET_META, ("APP", "LOAD", "TOTAL"))
}

// ---------- throughput graph source ----------

/// What a Throughput graph draws and says for its source. Adapter: the selected NIC's own
/// counters. Everything else: ETW totals of all adapters (attributed processes).
struct GraphSrc<'a> {
    /// Mb/s per second slot, oldest first, NaN = no data
    down: &'a [f32],
    up: &'a [f32],
    /// current rates (Mb/s); `None` = no valid measurement now
    cur: Option<(f32, f32)>,
    /// panel subtitle (what the lines are)
    meta: &'static str,
    /// source named in the hover readout
    label: &'static str,
}

fn graph_src<'a>(graph: crate::netscope::Graph, n: &'a crate::sample::VNic, v: &'a crate::sample::Vit) -> GraphSrc<'a> {
    use crate::netscope::Graph;
    match graph.view() {
        None => GraphSrc { down: &n.rx, up: &n.tx, cur: Some(((n.rx_bps / 1e6) as f32, (n.tx_bps / 1e6) as f32)), meta: "selected adapter · down and up · auto scale", label: "adapter" },
        Some(view) => match graph {
            Graph::Lan => scope_src(v, view, "all adapters · private remote end · auto scale", "LAN · all adapters"),
            Graph::Wan => scope_src(v, view, "all adapters · public remote end · auto scale", "WAN · all adapters"),
            _ => scope_src(v, view, "all adapters · all app traffic · auto scale", "all adapters"),
        },
    }
}

/// An ETW scope of all adapters: its stored 60 s history and its current rate.
fn scope_src<'a>(v: &'a crate::sample::Vit, view: crate::netetw::View, meta: &'static str, label: &'static str) -> GraphSrc<'a> {
    let (down, up) = v.net_scope.of(view);
    let cur = crate::netscope::current(&v.net_top, view).map(|r| (r.down, r.up));
    GraphSrc { down, up, cur, meta, label }
}

fn finite_n(v: &[f32]) -> usize {
    v.iter().filter(|x| x.is_finite()).count()
}

/// "12.3 Mb/s" of one column's value, a dash where nothing was measured (never 0).
fn rate_mbps(v: f32) -> String {
    if v.is_finite() { format!("{} Mb/s", crate::netscope::rate_text(v)) } else { "—".into() }
}

/// Hover readout of one column: the time and source, then every series' value at that second
/// (a missing reading is a dash, never 0).
fn net_tip(c: usize, label: &str, series: &[TipSeries]) -> Tip {
    Tip {
        head: format!("{} · {label}", ago(c)),
        lines: series.iter().map(|s| TipLine { swatch: Some(s.color), label: s.label.to_string(), value: rate_mbps(at(s.v, c)), hot: false }).collect(),
        foot: None,
    }
}

/// Peak against the adapter's link speed. Only for the selected-adapter graph: an all-adapters
/// scope (incl. LAN) has no single link to compare with.
fn link_share(adapter: bool, peak_mbps: f32, link_bps: u64) -> Option<String> {
    (adapter && link_bps > 0 && peak_mbps.is_finite()).then(|| format!("{:.1}%", peak_mbps as f64 * 1e6 / link_bps as f64 * 100.0))
}

/// Why an ETW graph has no (or only partial) line: (head, body, needs the admin button).
/// `None` when all is well. `none`: no sample at all in the window.
fn scope_state(f: &crate::netetw::Feed, none: bool) -> Option<(String, String, bool)> {
    use crate::netetw::Status;
    let (head, body, admin): (&str, String, bool) = match &f.status {
        Status::NeedsAdmin => ("All-adapter graphs need administrator rights", "Windows only lets an elevated program read network events per process. The Adapter graph works without it.".into(), true),
        Status::Unavailable(why) => ("Traffic by scope is unavailable", format!("{why} · Hayate retries automatically."), false),
        Status::Collecting | Status::Off => ("Collecting the first interval…", "A gap here means no measurement, not zero traffic.".into(), false),
        Status::Live if f.rows.is_empty() && f.bad > 0 => ("Windows sent network events Hayate cannot read", format!("{} events in an unsupported format since the trace started.", group_num(f.bad)), false),
        Status::Live => return none.then(|| ("Collecting the first interval…".to_string(), "A gap here means no measurement, not zero traffic.".to_string(), false)),
    };
    Some((head.to_string(), body, admin))
}

/// Status of an ETW graph over the plot: the full message when there is nothing to draw,
/// a one-line chip when an interruption left gaps or the rates read low.
fn scope_notice(app: &mut App, g: &mut Gfx, feed: &crate::netetw::Feed, gr: &Rect, none: bool) {
    let state = scope_state(feed, none);
    if none {
        if let Some((head, body, admin)) = state {
            let (x, w) = (gr.x + 16.0, gr.w - 32.0);
            let y = gr.y + 14.0;
            g.text(&head, Rect::new(x, y, w, 20.0), f(12.0), TX2);
            g.text(&body, Rect::new(x, y + 22.0, w, 18.0), f(11.5), TX3);
            if admin && gr.h > 100.0 {
                let br = Rect::new(x, y + 50.0, 150.0, 28.0);
                let hov = app.hover == Some(Hit::VitAdmin);
                button(g, br, "Restart as admin", hov, true);
                app.hits.push((br, Hit::VitAdmin));
            }
        }
        return;
    }
    let warn = (feed.status == crate::netetw::Status::Live).then(|| net_warn(feed)).flatten();
    let text = match (state, warn) {
        (Some((head, _, _)), _) => head,
        (None, Some(w)) => w,
        _ => return,
    };
    let tw = app.vit.wd.get(g, &text, f(11.0)) + 16.0;
    let chip = Rect::new(gr.x + 6.0, gr.y + 6.0, tw.min(gr.w - 12.0).max(0.0), 20.0);
    g.round(chip, 4.0, PAN2);
    g.round_stroke(chip, 4.0, LN, 1.0);
    g.text(&text, chip.inset(8.0, 0.0), f(11.0), TX2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_tip_stays_visible_when_heatmap_is_partly_scrolled() {
        let clip = Rect::new(290.0, 40.0, 800.0, 500.0);
        assert_eq!(tip_y(-150.0, 100.0, clip), 40.0);
        assert_eq!(tip_y(100.0, 100.0, clip), 100.0);
        assert_eq!(tip_y(520.0, 100.0, clip), 440.0);
    }

    #[test]
    fn memory_text_three_digits_from_a_gigabyte() {
        const MB: u64 = 1 << 20;
        assert_eq!(mem_s(87 * MB + MB / 2), "87.5 MB");
        assert_eq!(mem_s(504 * MB), "504 MB");
        assert_eq!(mem_s(1219 * MB), "1.19 GB");
        assert_eq!(mem_s(17_000 * MB), "16.6 GB");
        assert_eq!(mem_s(200 * 1024 * MB), "200 GB");
    }

    #[test]
    fn apps_merge_by_display_name() {
        let row = |src: usize, count: usize, cpu: f32, mem: u64, gpu: f32| Row { id: TopKey::App(format!("a{src}")), src, pi: src * 10, count, key: cpu as f64, cpu, mem, gpu, io: 0.0, down: 0, up: 0, scopes: Scopes::default() };
        let v = merge_by_name(vec![
            ("hopper", row(0, 2, 1.0, 100, 0.0)),
            ("Hayate Task Manager", row(1, 1, 0.5, 30, 0.0)),
            ("Hopper", row(2, 2, 3.0, 50, 4.0)),
            ("hayate task manager", row(3, 1, 0.2, 30, 0.0)),
            ("HAYATE TASK MANAGER", row(4, 1, 0.1, 30, 0.0)),
            ("chrome", row(5, 9, 2.0, 900, 1.0)),
        ]);
        assert_eq!(v.len(), 3);
        let h = &v[0];
        assert_eq!((h.count, h.mem), (4, 150));
        assert!((h.cpu - 4.0).abs() < 1e-6 && (h.gpu - 4.0).abs() < 1e-6);
        assert_eq!(h.pi, 20, "icon from the busiest tree");
        assert_eq!(h.src, 2, "name and engine from the tree using the GPU");
        assert_eq!((v[1].count, v[1].mem, v[1].src), (3, 90, 1));
        assert_eq!((v[2].count, v[2].src), (9, 5));
    }

    #[test]
    fn nice_scale_steps() {
        assert_eq!(nice(0.7), 1.0);
        assert_eq!(nice(3.0), 5.0);
        assert_eq!(nice(41.6), 50.0);
        assert_eq!(nice(220.0), 250.0);
    }

    #[test]
    fn cpu_names_shorten() {
        assert_eq!(short_cpu("AMD Ryzen 9 5900X 12-Core Processor"), "Ryzen 9 5900X");
        assert_eq!(short_cpu("Intel(R) Core(TM) i7-9700K CPU @ 3.60GHz"), "i7-9700K");
    }

    #[test]
    fn column_mean_skips_gaps() {
        let mut block = vec![f32::NAN; 2 * HIST];
        block[HIST - 1] = 10.0;
        block[2 * HIST - 1] = 30.0;
        block[HIST - 2] = 4.0;
        let m = col_mean(&block, &[0, 1]);
        assert_eq!(m[HIST - 1], 20.0);
        assert_eq!(m[HIST - 2], 4.0, "a row with no sample does not count");
        assert!(m[0].is_nan());
    }

    // ---------- Top lists: identity, held order, network rows ----------

    use crate::netetw::{Feed, NetRow, Status};
    use crate::sample::{Proc, Vit};

    fn proc(pid: u32, create: i64, name: &str, cpu: f32) -> Proc {
        Proc { pid, ppid: 0, name: name.into(), create, cpu, mem: 1 << 20, io_bps: 0.0, threads: 1, handles: 1, session: 1, windowed: true, meta: Arc::new(Default::default()), gpu: 0.0, gpu_engine: None }
    }
    fn child(pid: u32, create: i64, ppid: u32, name: &str) -> Proc {
        Proc { ppid, windowed: false, ..proc(pid, create, name, 0.0) }
    }
    fn net(pid: u32, create: i64, down: u64, up: u64) -> NetRow {
        NetRow { pid, create, down, up, ..Default::default() }
    }
    fn snap(seq: u64, procs: Vec<Proc>, status: Status, rows: Vec<NetRow>) -> Snapshot {
        Snapshot { seq, procs, vit: Vit { net_top: Feed { status, rows, lost: 0, unattributed: 0, bad: 0, iv: None, conn: Default::default() }, ..Default::default() }, ..Default::default() }
    }
    fn names(vit: &VitState, s: &Snapshot, v: &[Slot<TopKey, Row>]) -> Vec<String> {
        v.iter()
            .map(|x| match x {
                Slot::Live(r) => row_name(vit, s, r).to_string(),
                Slot::Gone(k) => format!("gone:{}", vit.top_names.get(k).cloned().unwrap_or_default()),
            })
            .collect()
    }

    #[test]
    fn bits_text() {
        assert_eq!(bits(0), "0");
        assert_eq!(bits(1), "8 b/s");
        assert_eq!(bits(1_250), "10 kb/s");
        assert_eq!(bits(1_250_000), "10.0 Mb/s");
        assert_eq!(bits(125_000_000), "1.00 Gb/s");
    }

    #[test]
    fn net_rows_attach_by_pid_and_create_time_only() {
        let mut vit = VitState { per_proc: true, ..Default::default() };
        // pid 2 in the feed has another create time than the running pid 2: a replaced process, no row
        let s = snap(1, vec![proc(1, 10, "a.exe", 0.0), proc(2, 20, "b.exe", 0.0)], Status::Live, vec![net(1, 10, 1000, 500), net(2, 99, 7000, 7000), net(3, 5, 1, 1)]);
        let v = arrange_rows(&mut vit, &s, By::Net, 8, 0, false, false);
        assert_eq!(names(&vit, &s, &v), vec!["a.exe"]);
        let Slot::Live(r) = &v[0] else { panic!() };
        assert_eq!((r.down, r.up, r.key), (1000, 500, 1500.0), "download, upload and combined as sampled");
        assert_eq!(r.id, TopKey::Proc(1, 10));
    }

    #[test]
    fn net_apps_sum_every_process_of_a_name() {
        let mut vit = VitState::default();
        let s = snap(
            1,
            vec![proc(10, 1, "app.exe", 0.0), child(11, 2, 10, "app.exe"), proc(20, 3, "App.exe", 0.0), proc(30, 4, "other.exe", 0.0)],
            Status::Live,
            vec![net(10, 1, 100, 10), net(11, 2, 200, 20), net(20, 3, 300, 30), net(30, 4, 50, 5)],
        );
        let v = arrange_rows(&mut vit, &s, By::Net, 8, 0, false, false);
        assert_eq!(v.len(), 2);
        let Slot::Live(a) = &v[0] else { panic!() };
        assert_eq!((a.down, a.up, a.key, a.count), (600, 60, 660.0, 3), "two trees of one display name merge");
        assert_eq!(a.id, TopKey::App("app.exe".into()));
        let Slot::Live(b) = &v[1] else { panic!() };
        assert_eq!((b.down, b.up), (50, 5));
    }

    #[test]
    fn net_list_shows_nothing_unless_the_feed_is_live() {
        for st in [Status::Off, Status::Collecting, Status::NeedsAdmin, Status::Unavailable("x".into())] {
            let mut vit = VitState { per_proc: true, ..Default::default() };
            let s = snap(1, vec![proc(1, 10, "a.exe", 0.0)], st, vec![net(1, 10, 1, 1)]);
            assert!(arrange_rows(&mut vit, &s, By::Net, 8, 0, false, false).is_empty());
            assert!(vit.tops.is_empty(), "no held order of stale rates");
        }
    }

    #[test]
    fn net_order_holds_values_live_and_idle_is_not_exited() {
        let mut vit = VitState { per_proc: true, ..Default::default() };
        let procs = || vec![proc(1, 10, "a.exe", 0.0), proc(2, 20, "b.exe", 0.0)];
        let s0 = snap(1, procs(), Status::Live, vec![net(1, 10, 900, 0), net(2, 20, 100, 0)]);
        let v = arrange_rows(&mut vit, &s0, By::Net, 8, 0, false, false);
        assert_eq!(names(&vit, &s0, &v), vec!["a.exe", "b.exe"]);
        // b overtakes: order holds, numbers are live
        let s1 = snap(2, procs(), Status::Live, vec![net(1, 10, 10, 0), net(2, 20, 5000, 0)]);
        let v = arrange_rows(&mut vit, &s1, By::Net, 8, 1000, false, false);
        assert_eq!(names(&vit, &s1, &v), vec!["a.exe", "b.exe"]);
        let Slot::Live(b) = &v[1] else { panic!() };
        assert_eq!(b.down, 5000);
        // a stops sending: still running, so a quiet row (0), not an exit
        let s2 = snap(3, procs(), Status::Live, vec![net(2, 20, 5000, 0)]);
        let v = arrange_rows(&mut vit, &s2, By::Net, 8, 2000, false, false);
        let Slot::Live(a) = &v[0] else { panic!("idle row became a ghost") };
        assert_eq!((a.down, a.up, a.key), (0, 0, 0.0));
        // a exits for real: its slot stays, marked gone
        let s3 = snap(4, vec![proc(2, 20, "b.exe", 0.0)], Status::Live, vec![net(2, 20, 5000, 0)]);
        let v = arrange_rows(&mut vit, &s3, By::Net, 8, 3000, false, false);
        assert_eq!(names(&vit, &s3, &v), vec!["gone:a.exe", "b.exe"]);
        // timer: ghost leaves
        let v = arrange_rows(&mut vit, &s3, By::Net, 8, 5000, false, false);
        assert_eq!(names(&vit, &s3, &v), vec!["b.exe"]);
    }

    #[test]
    fn replaced_pid_in_a_held_list_is_a_gone_slot_not_the_new_process() {
        let mut vit = VitState { per_proc: true, ..Default::default() };
        let s0 = snap(1, vec![proc(100, 1, "old.exe", 50.0), proc(200, 2, "keep.exe", 10.0)], Status::Off, vec![]);
        arrange_rows(&mut vit, &s0, By::Cpu, 8, 0, true, false);
        let s1 = snap(2, vec![proc(100, 99, "new.exe", 90.0), proc(200, 2, "keep.exe", 10.0)], Status::Off, vec![]);
        let v = arrange_rows(&mut vit, &s1, By::Cpu, 8, 1000, true, false);
        assert_eq!(names(&vit, &s1, &v), vec!["gone:old.exe", "keep.exe"], "pointer inside: dead slot reserved, new pid-100 process waits");
        let v = arrange_rows(&mut vit, &s1, By::Cpu, 8, 2000, false, false);
        assert_eq!(names(&vit, &s1, &v), vec!["gone:old.exe", "keep.exe"], "2 s grace after the pointer left");
        let v = arrange_rows(&mut vit, &s1, By::Cpu, 8, 3000, false, false);
        let v = if names(&vit, &s1, &v)[0].starts_with("gone") { arrange_rows(&mut vit, &s1, By::Cpu, 8, 4000, false, false) } else { v };
        let v = if names(&vit, &s1, &v)[0].starts_with("gone") { arrange_rows(&mut vit, &s1, By::Cpu, 8, 5000, false, false) } else { v };
        assert_eq!(names(&vit, &s1, &v), vec!["new.exe", "keep.exe"]);
    }

    #[test]
    fn top_list_arranged_while_empty_fills_with_the_first_rows() {
        // hidden-state snapshot (no procs) is painted once before the first real one
        for (mode, inside, busy) in [("quiet", false, false), ("pointer", true, false), ("menu", false, true)] {
            let mut vit = VitState { per_proc: true, ..Default::default() };
            let empty = snap(1, vec![], Status::Off, vec![]);
            assert!(arrange_rows(&mut vit, &empty, By::Cpu, 8, 0, inside, busy).is_empty());
            let s = snap(2, vec![proc(100, 10, "a.exe", 50.0), proc(200, 20, "b.exe", 10.0)], Status::Off, vec![]);
            let v = arrange_rows(&mut vit, &s, By::Cpu, 8, 1000, inside, busy);
            assert_eq!(names(&vit, &s, &v), vec!["a.exe", "b.exe"], "{mode}");
            // then held as usual: b overtakes, order keeps
            let s2 = snap(3, vec![proc(100, 10, "a.exe", 5.0), proc(200, 20, "b.exe", 90.0)], Status::Off, vec![]);
            let v = arrange_rows(&mut vit, &s2, By::Cpu, 8, 2000, inside, busy);
            assert_eq!(names(&vit, &s2, &v), vec!["a.exe", "b.exe"], "{mode}: held");
        }
    }

    #[test]
    fn each_list_and_view_has_its_own_held_order() {
        let mut vit = VitState { per_proc: true, ..Default::default() };
        let s = snap(1, vec![proc(1, 10, "a.exe", 50.0), proc(2, 20, "b.exe", 10.0)], Status::Off, vec![]);
        arrange_rows(&mut vit, &s, By::Cpu, 8, 0, false, false);
        arrange_rows(&mut vit, &s, By::Mem, 8, 0, false, false);
        vit.per_proc = false;
        arrange_rows(&mut vit, &s, By::Cpu, 8, 0, false, false);
        assert_eq!(vit.tops.len(), 3);
    }

    /// 64 independent processes at 2 KB/s each, and one browser whose 100 workers send 1 KB/s each:
    /// from the real attribution step through to the Apps ranking.
    fn crowded() -> (Snapshot, Vec<(u32, i64)>) {
        let mut procs: Vec<Proc> = (1..=64).map(|i| proc(i, i as i64, &format!("solo{i:02}.exe"), 0.0)).collect();
        procs.push(proc(1000, 1000, "browser.exe", 0.0));
        procs.extend((1..=100).map(|i| child(2000 + i, 2000 + i as i64, 1000, "browser.exe")));
        let ids: Vec<(u32, i64)> = procs.iter().map(|p| (p.pid, p.create)).collect();
        let prev: HashMap<u32, i64> = ids.iter().copied().collect();
        let mut drained: HashMap<u32, crate::netetw::Bytes> = HashMap::new();
        for i in 1..=64 {
            drained.insert(i, crate::netetw::Bytes { down: 2000, ..Default::default() });
        }
        for i in 1..=100 {
            drained.insert(2000 + i, crate::netetw::Bytes { down: 1000, ..Default::default() });
        }
        let (rows, lost) = crate::netetw::attribute(&drained, &prev, &prev, 1000);
        assert_eq!((rows.len(), lost), (164, 0), "the attribution step keeps every process");
        (snap(1, procs, Status::Live, rows), ids)
    }

    #[test]
    fn apps_rank_the_browser_by_the_sum_of_its_workers() {
        let (s, _) = crowded();
        let mut vit = VitState { per_proc: false, ..Default::default() };
        let v = arrange_rows(&mut vit, &s, By::Net, 8, 0, false, false);
        let Slot::Live(top) = &v[0] else { panic!() };
        assert_eq!(top.id, TopKey::App("browser.exe".into()), "100 workers at 1 KB/s beat any single 2 KB/s app");
        assert_eq!((top.down, top.up, top.count), (100_000, 0, 101));
        assert_eq!(v.len(), 8, "the cut to the visible rows happens after the apps are summed");
        let Slot::Live(second) = &v[1] else { panic!() };
        assert_eq!(second.down, 2000);
    }

    #[test]
    fn processes_view_is_cut_separately_from_apps() {
        let (s, _) = crowded();
        let mut vit = VitState { per_proc: true, ..Default::default() };
        let v = arrange_rows(&mut vit, &s, By::Net, 8, 0, false, false);
        assert_eq!(v.len(), 8);
        for sl in &v {
            let Slot::Live(r) = sl else { panic!() };
            assert_eq!(r.down, 2000, "the busiest single processes, not the workers");
        }
        // asking for more reaches the workers: nothing was dropped upstream
        let mut vit = VitState { per_proc: true, ..Default::default() };
        let all = arrange_rows(&mut vit, &s, By::Net, 500, 0, false, false);
        assert_eq!(all.len(), 164);
    }

    #[test]
    fn unreadable_events_are_not_shown_as_idle() {
        let mut s = snap(1, vec![], Status::Live, vec![]);
        use crate::netetw::View;
        assert_eq!(net_live_empty(&s.vit.net_top, View::All).0, "No process traffic right now");
        s.vit.net_top.bad = 12;
        let (title, why) = net_live_empty(&s.vit.net_top, View::All);
        assert!(title.contains("cannot read") && why.contains("12"), "{title} / {why}");
        // unreadable events stay the headline whatever scope is selected
        assert!(net_live_empty(&s.vit.net_top, View::Wan).0.contains("cannot read"));
        assert!(net_warn(&s.vit.net_top).unwrap().contains("unreadable"));
        s.vit.net_top.bad = 0;
        assert!(net_warn(&s.vit.net_top).is_none());
        s.vit.net_top.lost = 3;
        assert!(net_warn(&s.vit.net_top).unwrap().contains("lost"));
    }

    /// A process with explicit scope components (the rest of `down` is Other).
    fn netx(pid: u32, create: i64, lan: (u64, u64), wan: (u64, u64), other_down: u64) -> NetRow {
        NetRow { pid, create, down: lan.0 + wan.0 + other_down, up: lan.1 + wan.1, lan_down: lan.0, lan_up: lan.1, wan_down: wan.0, wan_up: wan.1 }
    }

    fn sc(lan: (u64, u64), wan: (u64, u64), other: (u64, u64)) -> Scopes {
        Scopes { lan, wan, other }
    }

    #[test]
    fn net_bars_scale_to_busiest_total_and_hide_inactive_scope() {
        use crate::netetw::View;
        // busiest row: 1000 lan + 500 wan + 500 other = 2000
        let max = pair_sum(sc((600, 400), (300, 200), (400, 100)).total());
        assert_eq!(max, 2000);
        let both = net_bars(&sc((600, 400), (300, 200), (400, 100)), View::All, max);
        assert_eq!(both, NetBars::Two(0.5, 0.25));
        // only WAN: a single amber bar, no LAN bar
        assert_eq!(net_bars(&sc((0, 0), (500, 0), (0, 0)), View::All, max), NetBars::One(NET_WAN, 0.25));
        // only LAN: a single green bar
        assert_eq!(net_bars(&sc((0, 100), (0, 0), (0, 0)), View::All, max), NetBars::One(NET_LAN, 0.05));
        // Other alone never draws a bar
        assert_eq!(net_bars(&sc((0, 0), (0, 0), (900, 900)), View::All, max), NetBars::Idle);
        assert_eq!(net_bars(&Scopes::default(), View::All, max), NetBars::Idle);
        // a row bigger than the stale max or an empty list never goes past 100 % or divides by 0
        assert_eq!(net_bars(&sc((5000, 0), (0, 0), (0, 0)), View::All, max), NetBars::One(NET_LAN, 1.0));
        assert_eq!(net_bars(&sc((5, 0), (0, 0), (0, 0)), View::All, 0), NetBars::One(NET_LAN, 0.0));
    }

    #[test]
    fn scope_views_scale_to_their_own_busiest_row() {
        use crate::netetw::View;
        // a LAN-heavy row and a WAN-only row: in the WAN view the WAN row is 100 %, not diluted by the LAN row
        let (a, b) = (sc((9000, 0), (100, 0), (0, 0)), sc((0, 0), (400, 0), (0, 0)));
        assert_eq!([&a, &b].map(|r| bar_basis(r, View::All)), [9100, 400]);
        let max_wan = [&a, &b].iter().map(|r| bar_basis(r, View::Wan)).max().unwrap();
        assert_eq!(max_wan, 400);
        assert_eq!(net_bars(&b, View::Wan, max_wan), NetBars::One(NET_WAN, 1.0));
        assert_eq!(net_bars(&a, View::Wan, max_wan), NetBars::One(NET_WAN, 0.25));
        let max_lan = [&a, &b].iter().map(|r| bar_basis(r, View::Lan)).max().unwrap();
        assert_eq!(net_bars(&a, View::Lan, max_lan), NetBars::One(NET_LAN, 1.0));
    }

    #[test]
    fn net_bars_in_scope_views_show_one_bar_of_that_scope() {
        use crate::netetw::View;
        let s = sc((1000, 0), (500, 500), (0, 0));
        assert_eq!(net_bars(&s, View::Lan, 2000), NetBars::One(NET_LAN, 0.5));
        assert_eq!(net_bars(&s, View::Wan, 2000), NetBars::One(NET_WAN, 0.5));
        assert_eq!(net_bars(&sc((0, 0), (500, 0), (0, 0)), View::Lan, 2000), NetBars::Idle);
        assert_eq!(net_bars(&sc((10, 0), (0, 0), (0, 0)), View::Wan, 2000), NetBars::Idle);
        assert_ne!(NET_LAN, NET_WAN);
    }

    #[test]
    fn net_group_row_sums_scopes_of_members() {
        let mut a = Scopes::of(&netx(1, 1, (10, 1), (20, 2), 5));
        a.add(&Scopes::of(&netx(2, 2, (1, 1), (0, 0), 3)));
        assert_eq!(a, sc((11, 2), (20, 2), (8, 0)));
        assert_eq!(a.total(), (39, 4));
    }

    #[test]
    fn net_row_tip_lines_use_table_units_and_omit_empty_scopes() {
        // 1.25 MB/s = 10 Mb/s, 20 kB/s = 160 kb/s, 50 B/s = 400 b/s
        let t = net_row_tip("Zen", 1, &sc((1_250_000, 20_000), (0, 0), (50, 0)));
        assert_eq!(t.head, "Zen");
        let got: Vec<(&str, &str, Option<u32>)> = t.lines.iter().map(|l| (l.label.as_str(), l.value.as_str(), l.swatch)).collect();
        assert_eq!(
            got,
            vec![
                ("LAN", "↓ 10.0 Mb/s  ↑ 160 kb/s", Some(NET_LAN)),
                ("Other", "↓ 400 b/s  ↑ 0", Some(NET_OTHER)),
                ("Total", "↓ 10.0 Mb/s  ↑ 160 kb/s", None),
            ]
        );
        // grouped: ×N in the title, WAN swatch amber, an idle row keeps only Total
        let g = net_row_tip("Discord", 3, &sc((0, 0), (125_000, 0), (0, 0)));
        assert_eq!(g.head, "Discord ×3");
        assert_eq!(g.lines[0].swatch, Some(NET_WAN));
        let idle = net_row_tip("adb.exe", 1, &Scopes::default());
        assert_eq!(idle.lines.len(), 1);
        assert_eq!(idle.lines[0].value, "↓ 0  ↑ 0");
    }

    #[test]
    fn net_notes_say_payload_only_and_pick_by_width() {
        // every form names what is left out; the row tooltip footer does too
        for n in NET_NOTES.iter().chain([&NET_ROW_NOTE]) {
            assert!(n.to_lowercase().contains("payload") && n.contains("ACK"), "{n}");
        }
        // longest first: shorter forms only when the longer does not fit
        assert!(NET_NOTES.windows(2).all(|w| w[0].len() > w[1].len()));
        let m = |t: &str| t.chars().count() as f32 * 6.0;
        assert_eq!(pick_note(&NET_NOTES, 2000.0, m), NET_NOTES[0]);
        assert_eq!(pick_note(&NET_NOTES, NET_NOTES[1].chars().count() as f32 * 6.0, m), NET_NOTES[1]);
        assert_eq!(pick_note(&NET_NOTES, 10.0, m), NET_NOTES[2], "nothing fits: shortest, ellipsised by the box");
        assert_eq!(pick_note(&[], 10.0, m), "");
        // the (i) help carries the full wording
        assert!(net_scope_help().iter().any(|l| l.contains("Kernel-Network") && l.contains("ACKs") && l.contains("Up most")));
        // row tooltip carries the footer; numbers untouched
        assert_eq!(net_row_tip("Zen", 1, &Scopes::default()).foot.as_deref(), Some(NET_ROW_NOTE));
    }

    #[test]
    fn name_clipped_only_when_wider_than_cell() {
        assert!(!name_clipped(100.0, 100.0));
        assert!(!name_clipped(100.4, 100.0));
        assert!(name_clipped(100.6, 100.0));
        assert!(!name_clipped(40.0, 100.0));
    }

    #[test]
    fn name_tip_text_for_groups_and_processes() {
        let g = NameInfo { exes: vec!["svchost.exe".into()], count: 102, pid: None };
        let t = name_tip("Host Process for Windows Tasks", &g);
        assert_eq!(t.head, "Host Process for Windows Tasks");
        assert_eq!((t.lines[0].label.as_str(), t.lines[0].value.as_str()), ("Executable", "svchost.exe"));
        assert_eq!((t.lines[1].label.as_str(), t.lines[1].value.as_str()), ("Processes", "102"));
        let p = NameInfo { exes: vec!["svchost.exe".into()], count: 1, pid: Some(4242) };
        let t = name_tip("Host Process", &p);
        assert_eq!(t.lines.len(), 2);
        assert_eq!(t.lines[1].value, "4242");
        // single process app: no "Processes 1" line
        assert_eq!(name_lines(&NameInfo { exes: vec![], count: 1, pid: None }).len(), 0);
    }

    #[test]
    fn exe_names_dedupe_and_cap() {
        assert_eq!(exe_names(["a.exe", "A.EXE", "b.exe"].into_iter()), vec!["a.exe", "b.exe"]);
        assert_eq!(exe_names(["a", "b", "c", "d", "e"].into_iter()), vec!["a", "b", "c", "+2 more"]);
    }

    #[test]
    fn network_tip_merge_keeps_traffic_lines() {
        let mut t = net_row_tip("Steam Client WebHelper", 3, &Scopes::default());
        let n = t.lines.len();
        t.lines.splice(0..0, name_lines(&NameInfo { exes: vec!["steamwebhelper.exe".into()], count: 0, pid: None }));
        assert_eq!(t.lines.len(), n + 1);
        assert_eq!(t.head, "Steam Client WebHelper ×3");
        assert_eq!(t.lines.last().unwrap().label, "Total");
    }

    #[test]
    fn tip_pos_flips_at_edges_and_stays_inside() {
        let win = (1000.0, 700.0);
        // room right and below: offset from the pointer
        assert_eq!(tip_pos((100.0, 100.0), (200.0, 90.0), win, 40.0, 670.0), (114.0, 118.0));
        // near the right edge: card goes left of the pointer
        let (x, _) = tip_pos((950.0, 100.0), (200.0, 90.0), win, 40.0, 670.0);
        assert_eq!(x, 950.0 - 10.0 - 200.0);
        // near the bottom: card goes above the pointer
        let (_, y) = tip_pos((100.0, 650.0), (200.0, 90.0), win, 40.0, 670.0);
        assert_eq!(y, 650.0 - 10.0 - 90.0);
        // never off the top / left
        let (x, y) = tip_pos((5.0, 45.0), (900.0, 400.0), win, 40.0, 670.0);
        assert!(x >= 8.0 && y >= 40.0);
    }

    fn scoped() -> Snapshot {
        snap(
            1,
            vec![proc(1, 10, "a.exe", 0.0), proc(2, 20, "b.exe", 0.0), proc(3, 30, "c.exe", 0.0)],
            Status::Live,
            // a: mostly LAN; b: mostly WAN; c: only unclassified (CGNAT / reserved)
            vec![netx(1, 10, (900, 100), (10, 0), 0), netx(2, 20, (0, 0), (5000, 500), 0), netx(3, 30, (0, 0), (0, 0), 300)],
        )
    }

    #[test]
    fn the_scope_view_filters_and_ranks_by_that_scope() {
        use crate::netetw::View;
        let s = scoped();
        let run = |view: View, per_proc: bool| {
            let mut vit = VitState { per_proc, net_view: view, ..Default::default() };
            let v = arrange_rows(&mut vit, &s, By::Net, 8, 0, false, false);
            v.iter().map(|x| if let Slot::Live(r) = x { (row_name(&vit, &s, r).to_string(), r.down, r.up, r.key) } else { panic!() }).collect::<Vec<_>>()
        };
        for per_proc in [true, false] {
            assert_eq!(run(View::All, per_proc).iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), vec!["b.exe", "a.exe", "c.exe"], "All keeps every process, by total");
            let wan = run(View::Wan, per_proc);
            assert_eq!(wan.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), vec!["b.exe", "a.exe"], "c has no WAN bytes: no WAN row");
            assert_eq!((wan[0].1, wan[0].2, wan[0].3), (5000, 500, 5500.0), "a row shows the WAN part, not the total");
            assert_eq!((wan[1].1, wan[1].2), (10, 0));
            let lan = run(View::Lan, per_proc);
            assert_eq!(lan.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), vec!["a.exe"], "only a talks to the LAN, and ranks by LAN bytes");
            assert_eq!((lan[0].1, lan[0].2, lan[0].3), (900, 100, 1000.0));
        }
    }

    #[test]
    fn apps_sum_the_scope_before_the_cut() {
        use crate::netetw::View;
        // 64 solo processes with 2 KB/s of WAN each, one browser: 100 workers with 1 KB/s of LAN and 10 B/s of WAN
        let mut procs: Vec<Proc> = (1..=64).map(|i| proc(i, i as i64, &format!("solo{i:02}.exe"), 0.0)).collect();
        procs.push(proc(1000, 1000, "browser.exe", 0.0));
        procs.extend((1..=100).map(|i| child(2000 + i, 2000 + i as i64, 1000, "browser.exe")));
        let mut rows: Vec<NetRow> = (1..=64).map(|i| netx(i, i as i64, (0, 0), (2000, 0), 0)).collect();
        rows.extend((1..=100).map(|i| netx(2000 + i, 2000 + i as i64, (1000, 0), (10, 0), 0)));
        let s = snap(1, procs, Status::Live, rows);
        let first = |view: View| {
            let mut vit = VitState { per_proc: false, net_view: view, ..Default::default() };
            let v = arrange_rows(&mut vit, &s, By::Net, 8, 0, false, false);
            // LAN has one app only; WAN and All have 65, cut to the visible 8 after the sums
            assert_eq!(v.len(), if view == View::Lan { 1 } else { 8 }, "the cut to the visible rows comes after the sums");
            let Slot::Live(r) = &v[0] else { panic!() };
            (r.id.clone(), r.down, r.count)
        };
        assert_eq!(first(View::Lan), (TopKey::App("browser.exe".into()), 100_000, 101), "LAN: only the browser, summed over its workers");
        // WAN: a solo app (2 KB/s) beats the browser (100 x 10 B/s = 1 KB/s)
        let (id, down, _) = first(View::Wan);
        assert_eq!(down, 2000);
        assert!(matches!(id, TopKey::App(n) if n.starts_with("solo")));
        // All: browser = 101 KB/s total
        assert_eq!(first(View::All).1, 101_000);
    }

    #[test]
    fn changing_the_scope_reorders_at_once_not_after_the_hold() {
        use crate::netetw::View;
        assert_ne!(list_spec(By::Net, true, View::All), list_spec(By::Net, true, View::Lan));
        assert_ne!(list_spec(By::Net, true, View::Lan), list_spec(By::Net, true, View::Wan));
        assert_ne!(list_spec(By::Net, true, View::All), list_spec(By::Net, false, View::All));
        // other lists do not care about the net scope
        assert_eq!(list_spec(By::Cpu, true, View::All), list_spec(By::Cpu, true, View::Wan));
        let s = scoped();
        let mut vit = VitState { per_proc: true, ..Default::default() };
        let v = arrange_rows(&mut vit, &s, By::Net, 8, 0, true, false);
        assert_eq!(names(&vit, &s, &v), vec!["b.exe", "a.exe", "c.exe"]);
        // pointer inside the panel (order held), 100 ms later: the explicit scope change still applies immediately
        vit.net_view = View::Lan;
        let v = arrange_rows(&mut vit, &s, By::Net, 8, 100, true, false);
        assert_eq!(names(&vit, &s, &v), vec!["a.exe"]);
    }

    #[test]
    fn an_empty_scope_says_which_scope_not_idle() {
        use crate::netetw::View;
        let s = scoped();
        let (head, _) = net_live_empty(&s.vit.net_top, View::Lan);
        assert_eq!(head, "No LAN traffic right now");
        assert_eq!(net_live_empty(&s.vit.net_top, View::Wan).0, "No WAN traffic right now");
        assert_eq!(net_live_empty(&s.vit.net_top, View::All).0, "No process traffic right now", "All with rows is not asked");
        // a feed without any rows is idle whatever the scope
        let idle = snap(1, vec![], Status::Live, vec![]);
        assert_eq!(net_live_empty(&idle.vit.net_top, View::Wan).0, "No process traffic right now");
    }

    #[test]
    fn app_rows_merge_net_fields() {
        let row = |down: u64, up: u64| Row { id: TopKey::App("x".into()), src: 0, pi: 0, count: 1, key: 0.0, cpu: 0.0, mem: 0, gpu: 0.0, io: 0.0, down, up, scopes: Scopes::default() };
        let v = merge_by_name(vec![("X", row(1, 2)), ("x", row(10, 20))]);
        assert_eq!((v.len(), v[0].down, v[0].up), (1, 11, 22));
    }

    // ---------- throughput graph source ----------

    use crate::netetw::Interval;
    use crate::netscope::{Graph, ScopeHist};
    use crate::sample::VNic;

    fn vnic() -> VNic {
        let mut rx = vec![f32::NAN; HIST];
        let mut tx = vec![f32::NAN; HIST];
        rx[HIST - 1] = 7.0;
        tx[HIST - 1] = 3.0;
        VNic { luid: 1, name: "Ethernet".into(), desc: "NIC".into(), kind: "wired", link_bps: 1_000_000_000, rx_bps: 8e6, tx_bps: 2e6, rx_total: 0, tx_total: 0, rx, tx }
    }

    /// Live feed whose attributed processes moved 2.5 MB/s down (LAN 1 MB/s, WAN 1.25 MB/s, rest Other).
    fn scope_vit() -> Vit {
        let row = NetRow { pid: 4242, create: 9, down: 2_500_000, up: 125_000, lan_down: 1_000_000, lan_up: 0, wan_down: 1_250_000, wan_up: 125_000 };
        let feed = Feed { status: Status::Live, rows: vec![row], lost: 0, unattributed: 0, bad: 0, iv: Some(Interval { id: 1, end_ms: 5_000, span_ms: 1_000 }), conn: Default::default() };
        let mut h = ScopeHist::default();
        h.observe(5_000, &feed);
        Vit { net_scope: h.series(), net_top: feed, ..Default::default() }
    }

    #[test]
    fn graph_defaults_to_the_combined_down_view() {
        let s = VitState::default();
        assert_eq!((s.net_combined, s.net_up), (true, false));
    }

    #[test]
    fn adapter_graph_reads_the_nic_and_scopes_read_etw_in_megabits() {
        let (n, v) = (vnic(), scope_vit());
        let a = graph_src(Graph::Adapter, &n, &v);
        assert_eq!(a.cur, Some((8.0, 2.0)), "rx_bps / 1e6");
        assert_eq!((a.down[HIST - 1], a.up[HIST - 1]), (7.0, 3.0));
        assert!(a.meta.starts_with("selected adapter") && a.label == "adapter");
        // ETW: bytes per second * 8 / 1e6
        let all = graph_src(Graph::All, &n, &v);
        assert_eq!(all.cur, Some((20.0, 1.0)));
        assert_eq!((all.down[HIST - 1], all.up[HIST - 1]), (20.0, 1.0));
        let wan = graph_src(Graph::Wan, &n, &v);
        assert_eq!((wan.cur, wan.down[HIST - 1]), (Some((10.0, 1.0)), 10.0));
        // the LAN source reads the all-adapters LAN scope, never the NIC or the WAN/All series
        let lan = graph_src(Graph::Lan, &n, &v);
        assert!(lan.meta.contains("private remote end"));
        assert_eq!((lan.cur, lan.down[HIST - 1], lan.up[HIST - 1]), (Some((8.0, 0.0)), 8.0, 0.0));
        for s in [&all, &lan, &wan] {
            assert!(s.label.contains("all adapters"), "{}", s.label);
        }
        assert!(lan.label.starts_with("LAN") && wan.label.starts_with("WAN"));
        // the adapter's own rows never leak into an ETW scope
        assert!(lan.down.iter().filter(|x| x.is_finite()).all(|x| *x != 7.0));
    }

    #[test]
    fn graph_controls_are_clear_of_the_top_filter_and_pick_independently() {
        use crate::netetw::View;
        // Combined / Adapter / Down / Up are VitSeg 7..=10; the Top filter keeps 4..=6
        let s = VitState::default();
        assert_eq!(s.net_view, View::All);
        assert!(s.net_combined);
    }

    #[test]
    fn combined_series_have_three_distinct_colours_per_direction() {
        assert_eq!(combined_colors(false), [C_NET, LAN_DN, C_DSK]);
        assert_eq!(combined_colors(true), [NET_UP, LAN_UP, DSK_W]);
        for up in [false, true] {
            let c = combined_colors(up);
            assert!(c[0] != c[1] && c[1] != c[2] && c[0] != c[2], "Total, LAN and WAN read as three series");
        }
    }

    #[test]
    fn combined_panel_has_total_lan_wan_of_the_chosen_direction() {
        let (n, v) = (vnic(), scope_vit());
        let d = net_panel(true, false, &n, &v);
        assert!(!d.adapter);
        assert_eq!(d.lines.iter().map(|l| l.legend).collect::<Vec<_>>(), ["Total", "LAN", "WAN"]);
        // down: ETW all 20, LAN 8, WAN 10 Mb/s
        assert_eq!(d.lines.iter().map(|l| l.cur).collect::<Vec<_>>(), [Some(20.0), Some(8.0), Some(10.0)]);
        let u = net_panel(true, true, &n, &v);
        assert_eq!(u.lines.iter().map(|l| l.cur).collect::<Vec<_>>(), [Some(1.0), Some(0.0), Some(1.0)]);
        assert!(u.label.ends_with("up") && d.label.ends_with("down"));
    }

    #[test]
    fn adapter_panel_is_the_nic_down_and_up_with_a_link_comparison() {
        let (n, v) = (vnic(), scope_vit());
        let a = net_panel(false, true, &n, &v);
        assert!(a.adapter, "direction chip does not apply");
        assert_eq!(a.lines.iter().map(|l| l.legend).collect::<Vec<_>>(), ["Down", "Up"]);
        assert_eq!(a.lines.iter().map(|l| l.cur).collect::<Vec<_>>(), [Some(8.0), Some(2.0)]);
        assert_eq!((a.lines[0].color, a.lines[1].color), (C_NET, NET_UP));
    }

    #[test]
    fn every_source_labels_all_adapters_but_the_adapter_and_has_no_link_ratio() {
        let (n, v) = (vnic(), scope_vit());
        for g in [Graph::All, Graph::Lan, Graph::Wan] {
            let s = graph_src(g, &n, &v);
            assert!(s.label.contains("all adapters") && s.meta.starts_with("all adapters"), "{g:?}");
            assert_eq!(link_share(g == Graph::Adapter, 100.0, 1_000_000_000), None);
        }
    }

    #[test]
    fn hover_lists_every_series_at_the_column() {
        let (n, v) = (vnic(), scope_vit());
        let tip_of = |p: &NetPanel, c: usize| {
            let t: Vec<TipSeries> = p.lines.iter().map(|l| TipSeries { label: l.tip, color: l.color, v: l.v }).collect();
            net_tip(c, p.label, &t)
        };
        let main = net_panel(false, false, &n, &v);
        let m = tip_of(&main, HIST - 1);
        assert_eq!(m.head, "now · adapter");
        assert_eq!((m.lines[0].value.as_str(), m.lines[1].value.as_str()), ("7.0 Mb/s", "3.0 Mb/s"));
        assert_eq!((m.lines[0].swatch, m.lines[1].swatch), (Some(C_NET), Some(NET_UP)));
        let c = net_panel(true, false, &n, &v);
        let t = tip_of(&c, HIST - 1);
        assert_eq!(t.head, "now · all adapters · down");
        let rows: Vec<_> = t.lines.iter().map(|l| (l.label.as_str(), l.value.as_str(), l.swatch)).collect();
        assert_eq!(rows, [("Total", "20.0 Mb/s", Some(C_NET)), ("LAN", "8.0 Mb/s", Some(LAN_DN)), ("WAN", "10.0 Mb/s", Some(C_DSK))], "a measured zero stays a zero");
        // same column, an unmeasured second: dashes everywhere, never 0
        for t in [tip_of(&main, HIST - 2), tip_of(&c, HIST - 2)] {
            assert_eq!(t.head.split(" · ").next(), Some("−1 s"));
            assert!(t.lines.iter().all(|x| x.value == "—"), "{t:?}");
        }
    }

    #[test]
    fn current_and_hovered_values_come_from_different_places() {
        let (n, mut v) = (vnic(), scope_vit());
        // the current value follows the live feed, the hovered one the stored history
        v.net_top = Feed { status: Status::Collecting, ..Feed::OFF };
        let c = net_panel(true, false, &n, &v);
        assert!(c.lines.iter().all(|l| l.cur.is_none()), "no valid measurement now: the legend shows a dash");
        let t = [TipSeries { label: "Total", color: C_NET, v: c.lines[0].v }];
        assert_eq!(net_tip(HIST - 1, c.label, &t).lines[0].value, "20.0 Mb/s", "history of the last real second stays");
        // adapter graph: a column with no sample is a dash as well
        let a = net_panel(false, false, &n, &v);
        let t = [TipSeries { label: "Down", color: C_NET, v: a.lines[0].v }];
        assert_eq!(net_tip(0, a.label, &t).lines[0].value, "—");
        assert_eq!(net_tip(HIST - 1, a.label, &t).lines[0].value, "7.0 Mb/s");
    }

    #[test]
    fn scope_graphs_never_compare_with_the_adapter_link() {
        assert_eq!(link_share(true, 100.0, 1_000_000_000).as_deref(), Some("10.0%"));
        assert_eq!(link_share(false, 100.0, 1_000_000_000), None, "all adapters / LAN: no single link to compare with");
        assert_eq!(link_share(true, 100.0, 0), None, "no link speed, no ratio");
        assert_eq!(link_share(true, f32::NAN, 1_000_000_000), None);
    }

    #[test]
    fn one_graph_and_the_unified_facts_fit_and_stay_compact() {
        // the one graph takes the room the window leaves, between a floor and a ceiling
        assert_eq!(net_plot_height(180.0), 180.0);
        assert_eq!(net_plot_height(-50.0), 120.0, "little room: the page scrolls instead");
        assert_eq!(net_plot_height(5000.0), 230.0, "no giant blank pane");
        // connection counts sit beside the adapter facts when wide, below them when narrow
        let (wide, wide_adapter) = net_facts_h(9, false);
        let (stacked, stacked_adapter) = net_facts_h(9, true);
        assert_eq!(wide, wide_adapter, "wide: the tall adapter column sets the height");
        assert!(stacked > stacked_adapter + 100.0, "narrow: the connection block adds its own rows");
        let (few, _) = net_facts_h(4, false);
        assert!(few >= 33.0 + 8.0 + 75.0 + 22.0, "even a short adapter list holds the three counts and note: {few}");
    }

    fn ser(v: &[f32]) -> Vec<f32> {
        let mut o = vec![f32::NAN; HIST];
        for (i, x) in v.iter().enumerate() {
            o[HIST - v.len() + i] = *x;
        }
        o
    }

    #[test]
    fn cpu_readout_lists_total_and_each_cluster_at_the_hovered_second() {
        let (total, c0, c1) = (ser(&[30.0, 31.0]), ser(&[20.0, 10.0]), ser(&[40.0, 52.0]));
        let s = [TipSeries { label: "CCD0 · V-Cache", color: 1, v: &c0 }, TipSeries { label: "CCD1", color: 2, v: &c1 }];
        let t = cpu_tip(HIST - 1, &total, &s, None);
        assert_eq!(t.head, "now");
        let rows: Vec<(&str, &str)> = t.lines.iter().map(|l| (l.label.as_str(), l.value.as_str())).collect();
        assert_eq!(rows, [("Total", "31%"), ("CCD0 · V-Cache", "10%"), ("CCD1", "52%")]);
        assert_eq!(t.foot, None);
        // the busiest cluster changes between seconds, and the readout follows the data
        let t = cpu_tip(HIST - 2, &total, &s, None);
        let rows: Vec<(&str, &str)> = t.lines.iter().map(|l| (l.label.as_str(), l.value.as_str())).collect();
        assert_eq!(rows, [("Total", "30%"), ("CCD0 · V-Cache", "20%"), ("CCD1", "40%")]);
        let (c0, c1) = (ser(&[90.0, 10.0]), ser(&[40.0, 52.0]));
        let s = [TipSeries { label: "CCD0", color: 1, v: &c0 }, TipSeries { label: "CCD1", color: 2, v: &c1 }];
        let v = |c: usize| cpu_tip(c, &total, &s, None).lines.iter().map(|l| l.value.clone()).collect::<Vec<_>>();
        assert_eq!(v(HIST - 2), ["30%", "90%", "40%"], "CCD0 busiest here");
        assert_eq!(v(HIST - 1), ["31%", "10%", "52%"], "CCD1 busiest here");
        // the pointer's own cluster is marked
        let t = cpu_tip(HIST - 1, &total, &s, Some(0));
        assert_eq!(t.lines.iter().map(|l| l.hot).collect::<Vec<_>>(), [false, true, false]);
    }

    #[test]
    fn cpu_readout_missing_values_are_dashes_never_zero() {
        // before the first sample, and a cluster whose threads have no reading yet
        let total = ser(&[12.0]);
        let (c0, c1) = (ser(&[12.0]), vec![f32::NAN; HIST]);
        let s = [TipSeries { label: "P-cores", color: 1, v: &c0 }, TipSeries { label: "E-cores", color: 2, v: &c1 }];
        let t = cpu_tip(HIST - 1, &total, &s, None);
        assert_eq!(t.lines.iter().map(|l| l.value.as_str()).collect::<Vec<_>>(), ["12%", "12%", "—"]);
        let t = cpu_tip(3, &total, &s, None);
        assert_eq!(t.lines.iter().map(|l| l.value.as_str()).collect::<Vec<_>>(), ["—", "—", "—"]);
        // a hybrid or single-cluster CPU still gets its Total
        let t = cpu_tip(HIST - 1, &total, &[TipSeries { label: "CPU", color: 1, v: &c0 }], None);
        assert_eq!(t.lines.len(), 1);
        assert_eq!((t.lines[0].label.as_str(), t.lines[0].value.as_str()), ("Total", "12%"));
        assert_eq!(cpu_tip(HIST + 40, &total, &[], None).lines[0].value, "—", "a column past the data is a dash");
    }

    #[test]
    fn cpu_readout_with_many_clusters_is_bounded_and_says_so() {
        let data: Vec<Vec<f32>> = [5.0, 80.0, f32::NAN, 60.0, 80.0, 10.0].iter().map(|v| ser(&[*v])).collect();
        let names: Vec<String> = (0..data.len()).map(|k| format!("Cluster {k}")).collect();
        let s: Vec<TipSeries> = data.iter().zip(&names).map(|(d, n)| TipSeries { label: n, color: 1, v: d }).collect();
        let total = ser(&[40.0]);
        let t = cpu_tip(HIST - 1, &total, &s, None);
        let rows: Vec<(&str, &str)> = t.lines.iter().map(|l| (l.label.as_str(), l.value.as_str())).collect();
        // Total + the three busiest (ties keep cluster order), busiest first
        assert_eq!(rows, [("Total", "40%"), ("Cluster 1", "80%"), ("Cluster 4", "80%"), ("Cluster 3", "60%")]);
        assert_eq!(t.foot.as_deref(), Some("3 of 6 clusters · busiest"));
        assert!(t.lines.len() <= TIP_CLUSTERS + 1, "the box height stays bounded");
        // a cluster without a reading never outranks one with a reading
        let t = cpu_tip(HIST - 1, &total, &s[..5], Some(2));
        assert!(t.lines.iter().any(|l| l.label == "Cluster 2" && l.value == "—" && l.hot), "the pointer's cluster is kept, as a dash");
        assert_eq!(t.foot.as_deref(), Some("3 of 5 clusters · busiest + pointer"));
        // exactly the limit lists everything in cluster order
        let t = cpu_tip(HIST - 1, &total, &s[..4], None);
        assert_eq!(t.lines.len(), 5);
        assert_eq!(t.foot, None);
    }

    #[test]
    fn empty_scope_graphs_say_why() {
        let live = |rows: Vec<NetRow>, bad: u64| Feed { status: Status::Live, rows, lost: 0, unattributed: 0, bad, iv: Some(Interval { id: 1, end_ms: 1000, span_ms: 1000 }), conn: Default::default() };
        let admin = scope_state(&Feed { status: Status::NeedsAdmin, ..Feed::OFF }, true).unwrap();
        assert!(admin.0.contains("administrator") && admin.2, "needs admin offers the restart button");
        let down = scope_state(&Feed { status: Status::Unavailable("session ended".into()), ..Feed::OFF }, true).unwrap();
        assert!(down.0.contains("unavailable") && down.1.contains("session ended") && !down.2);
        assert!(scope_state(&Feed { status: Status::Collecting, ..Feed::OFF }, true).unwrap().0.starts_with("Collecting"));
        assert!(scope_state(&live(vec![], 5), true).unwrap().0.contains("cannot read"), "unreadable events are not idle");
        assert_eq!(scope_state(&live(vec![], 0), false), None, "an idle live feed has real zeros to draw");
        assert!(scope_state(&live(vec![], 0), true).is_some(), "live but nothing recorded yet");
        // an interruption with older samples on screen still names itself
        assert!(scope_state(&Feed { status: Status::Collecting, ..Feed::OFF }, false).is_some());
    }

    // ---------- right-click target ----------

    use crate::netetw::View;

    /// Two trees of one name (a browser started twice), one solo app, one other app.
    ///   Browser.exe 100 > 101, 102     browser.exe 200 > 201     solo.exe 300     other.exe 400
    fn rc_procs() -> Vec<Proc> {
        vec![
            proc(100, 1, "Browser.exe", 30.0),
            child(101, 2, 100, "browser.exe"),
            child(102, 3, 100, "browser.exe"),
            proc(200, 4, "browser.exe", 20.0),
            child(201, 5, 200, "browser.exe"),
            proc(300, 6, "solo.exe", 10.0),
            proc(400, 7, "other.exe", 5.0),
        ]
    }
    fn rc_snap(procs: Vec<Proc>, rows: Vec<NetRow>) -> Snapshot {
        snap(1, procs, Status::Live, rows)
    }
    /// every live row of one list, as the paint would register it
    fn rc_hits(s: &Snapshot, by: By, per_proc: bool, view: View) -> Vec<TopHit> {
        let mut vit = VitState { per_proc, net_view: view, ..Default::default() };
        let v = arrange_rows(&mut vit, s, by, 16, 0, false, false);
        v.iter()
            .filter_map(|x| if let Slot::Live(r) = x { Some(top_hit(s, r, row_name(&vit, s, r), by, view)) } else { None })
            .collect()
    }
    fn ids_of(t: &Target) -> Vec<(u32, i64)> {
        let mut v: Vec<(u32, i64)> = t.members.iter().map(|m| (m.0, m.1)).collect();
        v.sort();
        v
    }

    #[test]
    fn every_list_mode_and_scope_gives_each_live_row_a_target() {
        let rows = vec![netx(100, 1, (10, 0), (500, 0), 0), netx(101, 2, (0, 0), (100, 0), 0), netx(201, 5, (300, 0), (0, 0), 0), netx(300, 6, (0, 0), (50, 0), 0), netx(400, 7, (20, 0), (0, 0), 0)];
        let s = rc_snap(rc_procs(), rows);
        let alive: Vec<(u32, i64)> = s.procs.iter().map(|p| (p.pid, p.create)).collect();
        for by in [By::Cpu, By::Mem, By::Gpu, By::Io, By::Net] {
            for per_proc in [true, false] {
                for view in [View::All, View::Lan, View::Wan] {
                    let hits = rc_hits(&s, by, per_proc, view);
                    if matches!(by, By::Cpu | By::Mem) || (by == By::Net && view == View::All) {
                        assert!(!hits.is_empty(), "{by:?} per_proc={per_proc} {view:?}: the list has rows");
                    }
                    for h in &hits {
                        let t = top_target(h, &s, &s).unwrap_or_else(|| panic!("{by:?} per_proc={per_proc} {view:?}: no target for {}", h.name));
                        assert_eq!(t.origin, Origin::Top);
                        assert!(t.members.iter().all(|m| alive.contains(&(m.0, m.1))), "only running processes");
                        assert_eq!(t.label, h.name, "the menu is named after the row");
                        if per_proc {
                            assert!(!t.group, "a Processes row is one process");
                            assert_eq!(t.members.len(), 1);
                            let TopKey::Proc(pid, create) = h.id else { panic!("per-process row with an app id") };
                            assert_eq!((t.members[0].0, t.members[0].1), (pid, create));
                            let r = t.rep.as_ref().expect("its own process is the representative");
                            assert_eq!((r.pid, r.create), (pid, create));
                        } else {
                            assert!(t.group, "an Apps row is a group");
                            let TopKey::App(key) = &h.id else { panic!("apps row with a process id") };
                            for m in &t.members {
                                assert_eq!(m.2.to_lowercase(), *key, "every member carries the row's name");
                            }
                        }
                        assert!(t.show.is_some(), "a running row can always be shown in Processes");
                    }
                }
            }
        }
    }

    #[test]
    fn an_apps_row_stands_for_every_tree_of_its_name() {
        let s = rc_snap(rc_procs(), vec![]);
        let hits = rc_hits(&s, By::Cpu, false, View::All);
        let b = hits.iter().find(|h| h.id == TopKey::App("browser.exe".into())).expect("browser row");
        let t = top_target(b, &s, &s).unwrap();
        assert_eq!(ids_of(&t), vec![(100, 1), (101, 2), (102, 3), (200, 4), (201, 5)], "both trees, every process, in one row");
        assert!(t.group && t.members.len() == 5);
        let r = t.rep.as_ref().unwrap();
        assert_eq!((r.pid, r.create), b.rep, "the representative is the icon process, named");
        // the other apps are one process each
        let o = hits.iter().find(|h| h.id == TopKey::App("solo.exe".into())).unwrap();
        assert_eq!(ids_of(&top_target(o, &s, &s).unwrap()), vec![(300, 6)]);
    }

    #[test]
    fn the_target_is_the_row_as_drawn_not_whatever_is_there_now() {
        let shown = rc_snap(rc_procs(), vec![]);
        let hits = rc_hits(&shown, By::Cpu, true, View::All);
        let solo = hits.iter().find(|h| h.name == "solo.exe").unwrap();
        // latest snapshot: another order, other numbers, same processes
        let mut procs = rc_procs();
        procs.reverse();
        procs[0].cpu = 99.0;
        let latest = rc_snap(procs, vec![]);
        let t = top_target(solo, &shown, &latest).unwrap();
        assert_eq!(ids_of(&t), vec![(300, 6)], "reordering and new rates do not retarget");
        assert_eq!(t.rep.as_ref().map(|r| r.name.as_str()), Some("solo.exe"));
    }

    #[test]
    fn a_reused_pid_or_an_exit_opens_no_menu() {
        let shown = rc_snap(rc_procs(), vec![]);
        let hits = rc_hits(&shown, By::Cpu, true, View::All);
        let solo = hits.iter().find(|h| h.name == "solo.exe").unwrap();
        // pid 300 now names another process
        let mut reused = rc_procs();
        reused[5] = proc(300, 77, "unrelated.exe", 1.0);
        assert!(top_target(solo, &shown, &rc_snap(reused, vec![])).is_none(), "same pid, new create time: not the row's process");
        // pid 300 exited
        let mut gone = rc_procs();
        gone.remove(5);
        assert!(top_target(solo, &shown, &rc_snap(gone, vec![])).is_none());
        // an Apps row whose every process is gone
        let apps = rc_hits(&shown, By::Cpu, false, View::All);
        let o = apps.iter().find(|h| h.id == TopKey::App("other.exe".into())).unwrap();
        let mut none = rc_procs();
        none.pop();
        assert!(top_target(o, &shown, &rc_snap(none, vec![])).is_none());
        // a row for a process that was never in the snapshot it was drawn from
        let ghost = TopHit { id: TopKey::Proc(999, 9), rep: (999, 9), name: "x".into(), by: By::Cpu, view: View::All };
        assert!(top_target(&ghost, &shown, &shown).is_none());
    }

    #[test]
    fn a_held_list_gone_slot_is_not_a_target() {
        let mut vit = VitState { per_proc: true, ..Default::default() };
        let s0 = rc_snap(rc_procs(), vec![]);
        arrange_rows(&mut vit, &s0, By::Cpu, 16, 0, true, false);
        let mut p = rc_procs();
        p.remove(5);
        let s1 = rc_snap(p, vec![]);
        let v = arrange_rows(&mut vit, &s1, By::Cpu, 16, 1000, true, false);
        let gone = v.iter().filter(|x| matches!(x, Slot::Gone(_))).count();
        assert_eq!(gone, 1, "the exited process keeps a dead slot");
        // the paint registers a hit for live rows only; a target from the old one fails too
        let hits: Vec<TopHit> = v.iter().filter_map(|x| if let Slot::Live(r) = x { Some(top_hit(&s1, r, "x", By::Cpu, View::All)) } else { None }).collect();
        assert_eq!(hits.len(), v.len() - 1, "no hit for the ghost");
        assert!(hits.iter().all(|h| h.id != TopKey::Proc(300, 6)));
        let was = TopHit { id: TopKey::Proc(300, 6), rep: (300, 6), name: "solo.exe".into(), by: By::Cpu, view: View::All };
        assert!(top_target(&was, &s0, &s1).is_none());
    }

    #[test]
    fn group_membership_is_what_the_row_showed() {
        let shown = rc_snap(rc_procs(), vec![]);
        let hits = rc_hits(&shown, By::Mem, false, View::All);
        let b = hits.iter().find(|h| h.id == TopKey::App("browser.exe".into())).unwrap();
        // a worker exits, and a new one starts, between paint and click
        let mut latest = rc_procs();
        latest.remove(2); // 102 gone
        latest.push(child(103, 8, 100, "browser.exe"));
        let t = top_target(b, &shown, &rc_snap(latest, vec![])).unwrap();
        assert_eq!(ids_of(&t), vec![(100, 1), (101, 2), (200, 4), (201, 5)], "the exited worker is dropped, the new one was not on the row");
        // the representative exits: the group stays, process actions have no stand-in
        let mut latest = rc_procs();
        let rep = b.rep;
        latest.retain(|p| (p.pid, p.create) != rep);
        let t = top_target(b, &shown, &rc_snap(latest, vec![])).unwrap();
        assert!(t.rep.is_none(), "no arbitrary member takes over as the representative");
        assert!(!t.members.iter().any(|m| (m.0, m.1) == rep) && !t.members.is_empty());
        assert!(t.show.is_some(), "the rest can still be shown in Processes");
    }

    #[test]
    fn network_apps_row_covers_only_the_trees_with_bytes_in_scope() {
        // tree 100 has WAN traffic, tree 200 only LAN
        let rows = vec![netx(101, 2, (0, 0), (900, 0), 0), netx(201, 5, (400, 0), (0, 0), 0)];
        let s = rc_snap(rc_procs(), rows);
        let key = TopKey::App("browser.exe".into());
        let members = |view: View| {
            let h = rc_hits(&s, By::Net, false, view).into_iter().find(|h| h.id == key).unwrap_or_else(|| panic!("browser row in {view:?}"));
            ids_of(&top_target(&h, &s, &s).unwrap())
        };
        assert_eq!(members(View::Wan), vec![(100, 1), (101, 2), (102, 3)], "WAN: the tree that moved bytes there");
        assert_eq!(members(View::Lan), vec![(200, 4), (201, 5)], "LAN: the other tree");
        assert_eq!(members(View::All), vec![(100, 1), (101, 2), (102, 3), (200, 4), (201, 5)]);
        // a quiet held row (no bytes in any tree) is every tree of the name
        let quiet_s = rc_snap(rc_procs(), vec![]);
        let g = procs::build_groups(&quiet_s);
        assert_eq!(app_members(&g, &quiet_s, By::Net, View::All, "browser.exe").len(), 5);
    }

    #[test]
    fn show_in_processes_names_the_group_or_the_process() {
        let s = rc_snap(rc_procs(), vec![]);
        let g = procs::build_groups(&s);
        // a lone process: its own row
        let one = show_in(&g, &s, (300, 6), false).unwrap();
        assert_eq!((one.select, one.expand), ((300, 6, true), None));
        // the root of a tree: the group row, opened
        let root = show_in(&g, &s, (100, 1), true).unwrap();
        assert_eq!((root.select, root.expand), ((100, 1, true), Some((100, 1))));
        // a worker: its own row inside the opened group
        let single_root = show_in(&g, &s, (100, 1), false).unwrap();
        assert_eq!((single_root.select, single_root.expand), ((100, 1, false), Some((100, 1))), "a process-mode root must not select the whole tree");
        let w = show_in(&g, &s, (102, 3), false).unwrap();
        assert_eq!((w.select, w.expand), ((102, 3, false), Some((100, 1))));
        assert!(show_in(&g, &s, (555, 5), false).is_none(), "not in this snapshot");
    }

    #[test]
    fn right_click_opens_a_menu_only_on_a_top_row() {
        let s = Arc::new(rc_snap(rc_procs(), vec![]));
        let hits = rc_hits(&s, By::Cpu, true, View::All);
        let mut app = App::new();
        app.tab = Tab::Vitals;
        app.snap = s.clone();
        app.vit.shown = Some(s.clone());
        app.vit.top_hits = hits;
        app.hits.clear();
        // a panel, graph or header behind everything, then two rows on top of it
        app.hits.push((Rect::new(0.0, 0.0, 400.0, 400.0), Hit::VitInfo));
        app.hits.push((Rect::new(10.0, 100.0, 300.0, 20.0), Hit::TopRow(0)));
        app.hits.push((Rect::new(10.0, 130.0, 300.0, 20.0), Hit::TopRow(99))); // a stale index
        let id0 = app.vit.top_hits[0].id.clone();
        assert!(matches!(app.right_click(50.0, 110.0), Action::RowMenu(..)), "on the row");
        assert_eq!(app.vit.pick, Some(id0), "the row is marked while its menu is up");
        app.vit.pick = None;
        assert!(matches!(app.right_click(50.0, 125.0), Action::None), "between rows is the panel behind");
        assert!(matches!(app.right_click(500.0, 500.0), Action::None), "outside everything");
        assert!(matches!(app.right_click(50.0, 140.0), Action::Repaint), "an index with no row opens nothing");
        assert!(app.vit.pick.is_none());
        // a Top row of a tab that is no longer shown is not offered
        app.tab = Tab::Processes;
        assert!(!matches!(app.right_click(50.0, 110.0), Action::RowMenu(..)));
    }
}
