//! The Task Manager style footer inside a Vitals graph panel (CPU load,
//! memory usage, GPU utilisation, disk activity): live readings on the left,
//! hardware facts on the right, one divider under the graph. Wide windows put
//! them side by side; narrow ones stack them. A reading or fact that cannot
//! be read shows a dash / "Unavailable", never a zero or a guess.
//!
//! The rows are built from the snapshot by pure functions (`cpu_rows`,
//! `mem_rows`, `gpu_rows`, `disk_rows`) so their unknown-value rules are
//! unit-tested; `plan` / `paint` only lay them out.

use super::vitals::{gb, mem_s, num, size};
use super::*;
use crate::sample::{gpu, hw, Snapshot, VDisk};

pub const UNAVAILABLE: &str = "Unavailable";

/// A live reading: small label over a larger value and its unit.
#[derive(Clone, Debug, PartialEq)]
pub struct Metric {
    pub label: String,
    pub value: String,
    pub unit: String,
    pub color: u32,
}

impl Metric {
    pub fn new(label: &str, value: impl Into<String>, unit: &str) -> Metric {
        Metric { label: label.into(), value: value.into(), unit: unit.into(), color: TX1 }
    }
    /// A reading that may be missing: a dash with no unit when it is.
    pub fn opt(label: &str, value: Option<String>, unit: &str) -> Metric {
        match value {
            Some(v) => Metric::new(label, v, unit),
            None => Metric { label: label.into(), value: "—".into(), unit: String::new(), color: TX3 },
        }
    }
}

/// A label / value pair of the facts column. `wide` rows take the whole column
/// (long values: model names, module lists, PCI location).
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    pub label: String,
    pub value: String,
    pub wide: bool,
}

impl Fact {
    pub fn new(label: &str, value: impl Into<String>) -> Fact {
        Fact { label: label.into(), value: value.into(), wide: false }
    }
    /// `UNAVAILABLE` when the fact could not be read.
    pub fn opt(label: &str, value: Option<String>) -> Fact {
        Fact::new(label, value.unwrap_or_else(|| UNAVAILABLE.into()))
    }
    pub fn wide(mut self) -> Fact {
        self.wide = true;
        self
    }
    fn missing(&self) -> bool {
        self.value == UNAVAILABLE || self.value == "—"
    }
}

// ---------- layout ----------

const CELL_W: f32 = 136.0;
const CELL_H: f32 = 44.0;
const ROW_H: f32 = 21.0;
/// space above the first row (below the divider) and below the last
const PAD_T: f32 = 12.0;
const PAD_B: f32 = 12.0;
const GAP: f32 = 24.0;
/// Side padding of the footer inside its panel.
pub const INSET: f32 = 14.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plan {
    /// facts below the readings instead of beside them
    pub stacked: bool,
    pub left_w: f32,
    pub right_x: f32,
    pub right_w: f32,
    pub m_cols: usize,
    pub f_cols: usize,
    /// footer height: divider, padding and the taller column
    pub h: f32,
    /// height of the facts column
    pub right_h: f32,
}

const COL_GAP: f32 = 18.0;
/// extra height of a fact whose value moved to its own line
const LINE2: f32 = 17.0;

/// Where one fact goes: its column (`None`: the whole width), top and height.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Slot {
    col: Option<usize>,
    y: f32,
    h: f32,
    /// label and value do not fit side by side: value on a second line
    two_line: bool,
}

/// `meas(text, value)`: text width (`value`: in the value font, else the label's).
type Meas<'a> = &'a mut dyn FnMut(&str, bool) -> f32;

/// Lay the facts out in `cols` columns; a wide fact takes a row of
/// its own (`full_w`), and a second line when it does not fit on one.
fn place(facts: &[Fact], cols: usize, full_w: f32, meas: Meas) -> (Vec<Slot>, f32) {
    let (mut out, mut y, mut col) = (Vec::with_capacity(facts.len()), 0.0f32, 0usize);
    for fa in facts {
        if fa.wide {
            if col > 0 {
                y += ROW_H;
                col = 0;
            }
            let two_line = meas(&fa.label, false) + meas(&fa.value, true) + 14.0 > full_w;
            let h = ROW_H + if two_line { LINE2 } else { 0.0 };
            out.push(Slot { col: None, y, h, two_line });
            y += h;
        } else {
            out.push(Slot { col: Some(col), y, h: ROW_H, two_line: false });
            col += 1;
            if col == cols {
                y += ROW_H;
                col = 0;
            }
        }
    }
    if col > 0 {
        y += ROW_H;
    }
    (out, y)
}

fn col_width(p: &Plan) -> f32 {
    (p.right_w - COL_GAP * (p.f_cols as f32 - 1.0)) / p.f_cols as f32
}

/// `plan` with the real text measurer.
pub fn plan_g(g: &mut Gfx, w: f32, narrow: bool, metrics: &[Metric], facts: &[Fact]) -> Plan {
    plan(w, narrow, metrics, facts, &mut |t, value| g.measure(t, if value { f(12.0).m() } else { f(11.5) }))
}

/// Footer geometry for a panel `w` wide (inner width: panel minus padding).
pub fn plan(w: f32, narrow: bool, metrics: &[Metric], facts: &[Fact], meas: Meas) -> Plan {
    let stacked = narrow || w < 640.0;
    let m_rows = |cols: usize| metrics.len().div_ceil(cols.max(1));
    let (left_w, right_w, right_x) = if metrics.is_empty() || stacked {
        (w, w, 0.0)
    } else {
        let l = (w * 0.42).clamp(230.0, 440.0);
        (l, w - l - GAP, l + GAP)
    };
    let m_cols = ((left_w / CELL_W).floor() as usize).clamp(1, metrics.len().max(1));
    let f_cols = if right_w >= if stacked { 430.0 } else { 470.0 } { 2 } else { 1 };
    let left_h = if metrics.is_empty() { 0.0 } else { m_rows(m_cols) as f32 * CELL_H };
    let mut p = Plan { stacked, left_w, right_x, right_w, m_cols, f_cols, h: 0.0, right_h: 0.0 };
    p.right_h = place(facts, f_cols, right_w, meas).1;
    let body = if stacked { left_h + if left_h > 0.0 && p.right_h > 0.0 { 12.0 } else { 0.0 } + p.right_h } else { left_h.max(p.right_h) };
    p.h = 1.0 + PAD_T + body + PAD_B;
    p
}

/// Paint the footer into `panel`, below the graph that ends at `y`. `p` is
/// the plan for `panel.w - 2 * INSET`.
pub fn paint(g: &mut Gfx, panel: Rect, y: f32, p: &Plan, metrics: &[Metric], facts: &[Fact]) {
    let x = panel.x + INSET;
    g.fill(Rect::new(panel.x + 1.0, y, panel.w - 2.0, 1.0), SEP);
    let top = y + 1.0 + PAD_T;
    // live readings
    for (k, m) in metrics.iter().enumerate() {
        let (cx, cy) = (x + (k % p.m_cols) as f32 * CELL_W, top + (k / p.m_cols) as f32 * CELL_H);
        g.text(&m.label, Rect::new(cx, cy, CELL_W - 6.0, 15.0), f(11.0), TX3);
        let vf = f(16.0).m().b();
        let vw = g.measure(&m.value, vf);
        g.text(&m.value, Rect::new(cx, cy + 15.0, (vw + 2.0).min(CELL_W - 6.0), 24.0), vf, m.color);
        if !m.unit.is_empty() {
            g.text(&m.unit, Rect::new(cx + vw + 3.0, cy + 20.0, (CELL_W - vw - 9.0).max(0.0), 18.0), f(11.0), TX3);
        }
    }
    // hardware facts
    let left_h = if metrics.is_empty() { 0.0 } else { metrics.len().div_ceil(p.m_cols) as f32 * CELL_H };
    let (fx, fy0) = if p.stacked { (x, top + left_h + if left_h > 0.0 { 12.0 } else { 0.0 }) } else { (x + p.right_x, top) };
    if !p.stacked && !metrics.is_empty() {
        g.fill(Rect::new(fx - GAP / 2.0, top, 1.0, left_h.max(p.right_h).max(ROW_H)), SEP);
    }
    let col_w = col_width(p);
    let font = |fa: &Fact| if fa.missing() { f(12.0) } else { f(12.0).m() };
    let (slots, _) = place(facts, p.f_cols, p.right_w, &mut |t, value| {
        // the value font is only measured for wide facts; its mono width does not depend on the text
        g.measure(t, if value { f(12.0).m() } else { f(11.5) })
    });
    for (fa, sl) in facts.iter().zip(&slots) {
        let (w, cx) = match sl.col {
            Some(c) => (col_w, fx + c as f32 * (col_w + COL_GAP)),
            None => (p.right_w, fx),
        };
        let cy = fy0 + sl.y;
        let color = if fa.missing() { TX3 } else { TX1 };
        if sl.two_line {
            g.text(&fa.label, Rect::new(cx, cy, w, ROW_H), f(11.5), TX3);
            g.text(&fa.value, Rect::new(cx, cy + ROW_H - 3.0, w, LINE2 + 2.0), font(fa), color);
        } else {
            let vw = g.measure(&fa.value, font(fa)).min(w * 0.72);
            g.text(&fa.label, Rect::new(cx, cy, (w - vw - 10.0).max(w * 0.28), ROW_H), f(11.5), TX3);
            g.text(&fa.value, Rect::new(cx + w - vw - 2.0, cy, vw + 2.0, ROW_H), font(fa).r(), color);
        }
        g.fill(Rect::new(cx, cy + sl.h - 1.0, w, 1.0), SEP);
    }
}

// ---------- rows ----------

/// "0:03:21:45" (days:hh:mm:ss, Task Manager's form).
pub fn fmt_uptime(s: u64) -> String {
    format!("{}:{:02}:{:02}:{:02}", s / 86400, s / 3600 % 24, s / 60 % 60, s % 60)
}

/// CPU readings and facts. `temp`: the package temperature when it is
/// current (None shows a dash).
pub fn cpu_rows(s: &Snapshot, temp: Option<f32>) -> (Vec<Metric>, Vec<Fact>) {
    let c = &s.st.cpu_hw;
    let t = &c.topo;
    let m = vec![
        Metric::new("Utilization", format!("{:.0}", s.cpu), "%"),
        Metric::opt("Speed", s.cpu_clock.map(|k| hw::fmt_ghz(k.avg_mhz as f64)), "GHz"),
        Metric::opt("Temperature", temp.map(|t| format!("{t:.0}")), "°C"),
        Metric::new("Processes", group_num(s.nprocs as u64), ""),
        Metric::new("Threads", group_num(s.threads as u64), ""),
        Metric::new("Handles", group_num(s.handles as u64), ""),
        Metric::new("Up time", fmt_uptime(s.uptime), ""),
    ];
    let cores = (t.cores > 0).then(|| {
        let split = hw::hybrid_split(&t.classes);
        if split.is_empty() { t.cores.to_string() } else { format!("{} ({split})", t.cores) }
    });
    let mut f = vec![
        Fact::opt("Base speed", c.base_mhz.map(hw::fmt_base)),
        Fact::opt("Sockets", (t.packages > 0).then(|| t.packages.to_string())),
        Fact::opt("Cores", cores),
        Fact::opt("Logical processors", (t.logical > 0).then(|| t.logical.to_string())),
        Fact::opt("Virtualization", c.virt.map(|v| if v { "Enabled" } else { "Disabled" }.to_string())),
    ];
    if let Some(k) = s.cpu_clock.filter(|k| k.cores < t.logical) {
        f.push(Fact::new("Speed coverage", format!("{} of {} logical processors", k.cores, t.logical)));
    }
    for (i, name) in ["L1 cache", "L2 cache", "L3 cache", "L4 cache"].iter().enumerate() {
        let cache = hw::fmt_cache(t.caches[i]);
        // L1-L3 are always listed (a dash says the OS did not report it); L4 only where it exists
        if i < 3 || cache.is_some() {
            f.push(Fact::opt(name, cache));
        }
    }
    if t.numa > 1 {
        f.push(Fact::new("NUMA nodes", t.numa.to_string()));
    }
    if t.groups > 1 {
        f.push(Fact::new("Processor groups", t.groups.to_string()));
    }
    (m, f)
}

/// Memory readings and facts.
pub fn mem_rows(s: &Snapshot) -> (Vec<Metric>, Vec<Fact>) {
    let (m, st) = (&s.mem, &s.st);
    let used = m.total.saturating_sub(m.avail);
    let g1 = |b: u64| format!("{:.1}", gb(b));
    let mut mt = vec![Metric::new("In use", g1(used), "GB")];
    if let Some(c) = s.vit.compressed {
        mt.push(Metric::new("Compressed", g1(c), "GB"));
    }
    mt.push(Metric::new("Available", g1(m.avail), "GB"));
    mt.push(Metric::new("Committed", format!("{} / {:.0}", g1(m.commit), gb(m.commit_limit)), "GB"));
    mt.push(Metric::new("Cached", g1(m.cached), "GB"));
    mt.push(Metric::new("Paged pool", mem_s(m.paged_pool), ""));
    mt.push(Metric::new("Non-paged pool", mem_s(m.nonpaged_pool), ""));

    let r = hw::ram_summary(&st.dimms);
    let mut f = Vec::new();
    match hw::speed_text(&r) {
        Some((label, v)) => f.push(Fact::new(label, v)),
        None => f.push(Fact::opt("Speed", None)),
    }
    // the module's rated speed, when the board runs it slower than rated
    if r.cfg.1 > 0 && r.rated.1 > 0 && r.rated != r.cfg {
        f.push(Fact::new("Rated speed", if r.rated.0 == r.rated.1 { format!("{} MT/s", r.rated.1) } else { format!("{}–{} MT/s", r.rated.0, r.rated.1) }));
    }
    f.push(Fact::opt("Slots used", (r.slots > 0).then(|| format!("{} of {}", r.populated, r.slots))));
    f.push(Fact::opt("Form factor", Some(r.form.clone()).filter(|x| !x.is_empty())));
    f.push(Fact::opt("Type", Some(r.kind.clone()).filter(|x| !x.is_empty())));
    f.push(Fact::opt("Installed", (st.ram_installed > 0).then(|| format!("{:.0} GB", gb(st.ram_installed)))));
    f.push(Fact::opt("Hardware reserved", (st.ram_installed >= m.total && m.total > 0).then(|| mem_s(st.ram_installed - m.total))));
    // every module on its own line: mixed modules stay visible as mixed
    let shown = 8;
    // boards that repeat "DIMM 0" per channel get the bank in the label
    let pop: Vec<&hw::Dimm> = st.dimms.iter().filter(|d| d.populated).collect();
    let speed_known = pop.iter().filter(|d| if r.cfg.1 > 0 { d.cfg_mts > 0 } else { d.rated_mts > 0 }).count();
    if speed_known < pop.len() {
        f.push(Fact::new("Speed coverage", format!("{} of {} modules", speed_known, pop.len())));
    }
    let dup = pop.iter().enumerate().any(|(i, a)| pop[..i].iter().any(|b| b.locator == a.locator));
    for d in pop.iter().take(shown) {
        let (l, v) = hw::dimm_line(d, dup);
        f.push(Fact::new(&l, v).wide());
    }
    let more = pop.len().saturating_sub(shown);
    if more > 0 {
        f.push(Fact::new("More modules", format!("{more} not listed")).wide());
    }
    (mt, f)
}

/// GPU readings and facts.
pub fn gpu_rows(raw: &gpu::Gpu) -> (Vec<Metric>, Vec<Fact>) {
    let x = &raw.facts;
    let mut m = vec![Metric::new("Utilization", format!("{:.0}", raw.util), "%")];
    if raw.vram_total > 0 {
        m.push(Metric::new("Dedicated memory", format!("{:.1} / {:.0}", gb(raw.vram_used), gb(raw.vram_total)), "GB"));
    }
    if raw.shared_total > 0 {
        m.push(Metric::new("Shared memory", format!("{:.1} / {:.0}", gb(raw.shared_used), gb(raw.shared_total)), "GB"));
    }
    let mut f = vec![
        Fact::opt("Model", Some(x.full_name.clone()).filter(|n| !n.is_empty())).wide(),
        Fact::opt("Driver version", x.driver_version.clone()),
        Fact::opt("Driver date", x.driver_date.clone()),
    ];
    if x.wddm.is_some() {
        f.push(Fact::opt("Driver model", x.wddm.clone()));
    }
    if x.pci_id.is_some() {
        f.push(Fact::opt("PCI ID", x.pci_id.clone()));
    }
    f.push(Fact::opt("Physical location", x.location.clone()).wide());
    (m, f)
}

/// Disk readings and facts. `reading`: volumes are still being read.
pub fn disk_rows(d: &VDisk, reading: bool) -> (Vec<Metric>, Vec<Fact>) {
    let m = vec![
        Metric::new("Active time", format!("{:.0}", d.active), "%"),
        Metric::new("Read", num(d.read_bps / 1e6), "MB/s"),
        Metric::new("Write", num(d.write_bps / 1e6), "MB/s"),
        // no I/O finished in the interval: there is no average to show
        Metric::opt("Avg response", d.resp_ms.map(|r| if r >= 10.0 { format!("{r:.0}") } else { format!("{r:.1}") }), "ms"),
    ];
    let formatted: u64 = d.vols.iter().map(|v| v.total).sum();
    let formatted = if formatted > 0 {
        Some(size(formatted))
    } else if reading {
        Some("Reading…".to_string())
    } else {
        None
    };
    let mut f = vec![
        Fact::opt("Model", Some(d.model.clone()).filter(|n| !n.is_empty())).wide(),
        Fact::new("Type", format!("{} · {}", hw::disk_media(d.bus, d.rotational), hw::disk_bus(d.bus))),
        Fact::opt("Capacity", (d.size > 0).then(|| size(d.size))),
        Fact::opt("Mapped volumes", formatted),
    ];
    if !d.firmware.is_empty() {
        f.push(Fact::new("Firmware", d.firmware.clone()));
    }
    f.push(Fact::new("System disk", if d.system { "Yes" } else { "No" }));
    f.push(Fact::new("Page file", if d.pagefile { "Yes" } else { "No" }));
    (m, f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::hw::{CacheTotal, CpuClock, CpuHw, CpuTopo, Dimm};
    use crate::sample::sys::Static;
    use std::sync::Arc;

    fn snap() -> Snapshot {
        let topo = CpuTopo {
            packages: 1,
            cores: 16,
            logical: 32,
            groups: 1,
            numa: 1,
            classes: vec![(0, 16)],
            caches: [CacheTotal { bytes: 1 << 20, count: 32 }, CacheTotal { bytes: 16 << 20, count: 16 }, CacheTotal { bytes: 128 << 20, count: 2 }, CacheTotal::default()],
        };
        let dimm = |loc: &str, mb: u64, cfg: u32| Dimm { locator: loc.into(), bank: String::new(), size_mb: mb, populated: mb > 0, kind: if mb > 0 { "DDR5" } else { "" }, form: "DIMM", cfg_mts: cfg, rated_mts: 6400, maker: "Maker".into(), part: "P1".into() };
        let st = Static {
            cpu_hw: CpuHw { topo, base_mhz: Some((4300, 4300)), virt: Some(true) },
            dimms: vec![dimm("A1", 32768, 6000), dimm("A2", 0, 0), dimm("B1", 32768, 6000), dimm("B2", 0, 0)],
            ram_installed: 64 << 30,
            ..Default::default()
        };
        Snapshot { st: Arc::new(st), cpu: 12.4, nprocs: 300, threads: 4000, handles: 90_000, uptime: 90_061, ..Default::default() }
    }
    fn val<'a>(f: &'a [Fact], l: &str) -> &'a str {
        &f.iter().find(|x| x.label == l).unwrap_or_else(|| panic!("no fact {l}")).value
    }

    #[test]
    fn cpu_rows_show_what_was_read_and_dash_what_was_not() {
        let mut s = snap();
        let (m, f) = cpu_rows(&s, None);
        let mv = |l: &str| m.iter().find(|x| x.label == l).unwrap().clone();
        assert_eq!((mv("Utilization").value.as_str(), mv("Utilization").unit.as_str()), ("12", "%"));
        assert_eq!(mv("Speed").value, "—", "no clock reading: a dash, not the base speed");
        assert_eq!(mv("Temperature").value, "—");
        assert_eq!(mv("Up time").value, "1:01:01:01");
        assert_eq!(mv("Handles").value, "90,000");
        assert_eq!(val(&f, "Base speed"), "4.30 GHz");
        assert_eq!((val(&f, "Sockets"), val(&f, "Cores"), val(&f, "Logical processors"), val(&f, "Virtualization")), ("1", "16", "32", "Enabled"));
        assert_eq!((val(&f, "L1 cache"), val(&f, "L2 cache"), val(&f, "L3 cache")), ("1 MB", "16 MB", "128 MB"));
        assert!(f.iter().all(|x| x.label != "L4 cache" && x.label != "NUMA nodes" && x.label != "Processor groups"));
        s.cpu_clock = Some(CpuClock { avg_mhz: 5012.0, max_mhz: 5400.0, cores: 32 });
        let (m, _) = cpu_rows(&s, Some(61.4));
        assert_eq!(m.iter().find(|x| x.label == "Speed").unwrap().value, "5.01");
        assert_eq!(m.iter().find(|x| x.label == "Temperature").unwrap().value, "61");
    }

    #[test]
    fn cpu_facts_the_os_did_not_report_are_unavailable_not_zero() {
        let s = Snapshot { st: Arc::new(Static::default()), ..Default::default() };
        let (_, f) = cpu_rows(&s, None);
        for l in ["Base speed", "Sockets", "Cores", "Logical processors", "Virtualization", "L1 cache", "L3 cache"] {
            assert_eq!(val(&f, l), UNAVAILABLE, "{l}");
        }
    }

    #[test]
    fn cpu_hybrid_multi_socket_and_groups_are_spelled_out() {
        let mut s = snap();
        let mut st = (*s.st).clone();
        st.cpu_hw.topo.classes = vec![(1, 8), (0, 16)];
        st.cpu_hw.topo.cores = 24;
        st.cpu_hw.topo.packages = 2;
        st.cpu_hw.topo.numa = 2;
        st.cpu_hw.topo.groups = 2;
        st.cpu_hw.base_mhz = Some((2100, 3400));
        st.cpu_hw.topo.caches[3] = CacheTotal { bytes: 128 << 20, count: 1 };
        s.st = Arc::new(st);
        let (_, f) = cpu_rows(&s, None);
        assert_eq!(val(&f, "Cores"), "24 (8 P + 16 E)");
        assert_eq!((val(&f, "Sockets"), val(&f, "NUMA nodes"), val(&f, "Processor groups")), ("2", "2", "2"));
        assert_eq!(val(&f, "Base speed"), "2.10–3.40 GHz");
        assert_eq!(val(&f, "L4 cache"), "128 MB");
    }

    #[test]
    fn memory_rows_report_mts_slots_form_and_reserved() {
        let mut s = snap();
        s.mem.total = 62 << 30;
        s.mem.avail = 40 << 30;
        let (m, f) = mem_rows(&s);
        assert_eq!(val(&f, "Speed"), "6000 MT/s");
        assert_eq!(val(&f, "Rated speed"), "6400 MT/s", "the board runs the modules below their rating");
        assert_eq!((val(&f, "Slots used"), val(&f, "Form factor"), val(&f, "Type"), val(&f, "Installed")), ("2 of 4", "DIMM", "DDR5", "64 GB"));
        assert_eq!(val(&f, "Hardware reserved"), "2.00 GB");
        assert_eq!(f.iter().filter(|x| x.wide).count(), 2, "one line per populated module");
        assert_eq!(val(&f, "A1"), "32 GB DDR5 · 6000 MT/s · Maker P1");
        // the same locator in two channels: the bank tells them apart
        let mut st = (*s.st).clone();
        for (d, b) in st.dimms.iter_mut().zip(["CH A", "CH A", "CH B", "CH B"]) {
            d.bank = b.into();
            d.locator = "DIMM 0".into();
        }
        s.st = Arc::new(st);
        let f = mem_rows(&s).1;
        assert!(f.iter().any(|x| x.label == "CH A DIMM 0") && f.iter().any(|x| x.label == "CH B DIMM 0"));
        assert!(m.iter().any(|x| x.label == "Non-paged pool"));
        assert!(m.iter().all(|x| x.label != "Compressed"), "no compression reading: no tile");
        s.vit.compressed = Some(3 << 30);
        assert!(mem_rows(&s).0.iter().any(|x| x.label == "Compressed" && x.value == "3.0"));
    }

    #[test]
    fn memory_without_smbios_says_unavailable() {
        let s = Snapshot { st: Arc::new(Static::default()), ..Default::default() };
        let (_, f) = mem_rows(&s);
        for l in ["Speed", "Slots used", "Form factor", "Type", "Installed", "Hardware reserved"] {
            assert_eq!(val(&f, l), UNAVAILABLE, "{l}");
        }
        assert!(f.iter().all(|x| !x.wide));
        // installed smaller than usable (not a real board): no negative reserved figure
        let mut s = snap();
        s.mem.total = 80 << 30;
        assert_eq!(val(&mem_rows(&s).1, "Hardware reserved"), UNAVAILABLE);
    }

    #[test]
    fn gpu_rows_leave_out_what_the_driver_did_not_give() {
        let g = gpu::Gpu { util: 7.0, vram_used: 1 << 30, vram_total: 10 << 30, ..Default::default() };
        let (m, f) = gpu_rows(&g);
        assert!(m.iter().all(|x| x.label != "Shared memory"), "no shared budget reported: no tile");
        assert_eq!(m.iter().find(|x| x.label == "Dedicated memory").unwrap().value, "1.0 / 10");
        assert_eq!((val(&f, "Driver version"), val(&f, "Driver date"), val(&f, "Physical location"), val(&f, "Model")), (UNAVAILABLE, UNAVAILABLE, UNAVAILABLE, UNAVAILABLE));
        assert!(f.iter().all(|x| x.label != "Driver model" && x.label != "PCI ID"));
        let facts = hw::GpuFacts {
            full_name: "NVIDIA GeForce RTX 3080".into(),
            driver_version: Some("31.0.15.4601".into()),
            driver_date: Some("2023-10-27".into()),
            wddm: Some("WDDM 3.1".into()),
            location: Some("PCI bus 1, device 0, function 0".into()),
            pci_id: Some("10DE:2206".into()),
        };
        let g = gpu::Gpu { facts: Arc::new(facts), shared_total: 16 << 30, ..g };
        let (m, f) = gpu_rows(&g);
        assert_eq!(m.iter().find(|x| x.label == "Shared memory").unwrap().value, "0.0 / 16");
        assert_eq!((val(&f, "Model"), val(&f, "Driver version"), val(&f, "Driver model")), ("NVIDIA GeForce RTX 3080", "31.0.15.4601", "WDDM 3.1"));
    }

    fn disk(vols: Vec<crate::sample::disk::Volume>) -> VDisk {
        VDisk {
            num: 0,
            model: "Samsung SSD 990 PRO 2TB".into(),
            bus: "NVMe",
            rotational: None,
            firmware: "4B2QJXD7".into(),
            letters: "C:".into(),
            size: 2_000_398_934_016,
            read_bps: 1.5e6,
            write_bps: 0.0,
            active: 3.0,
            resp_ms: None,
            queue: 0,
            system: true,
            pagefile: true,
            temp: None,
            vols,
            read: vec![],
            write: vec![],
            act: vec![],
            resp: vec![],
            qlen: vec![],
        }
    }

    #[test]
    fn disk_rows_type_capacity_formatted_and_idle_response() {
        let (m, f) = disk_rows(&disk(vec![]), true);
        assert_eq!(m.iter().find(|x| x.label == "Avg response").unwrap().value, "—", "no I/O completed: no average, not 0 ms");
        assert_eq!(val(&f, "Type"), "SSD · NVMe");
        assert_eq!(val(&f, "Capacity"), "2.0 TB");
        assert_eq!(val(&f, "Mapped volumes"), "Reading…");
        assert_eq!((val(&f, "Firmware"), val(&f, "System disk"), val(&f, "Page file")), ("4B2QJXD7", "Yes", "Yes"));
        let v = |t: u64| crate::sample::disk::Volume { letter: 'C', label: String::new(), fs: "NTFS".into(), free: 1, total: t };
        let (_, f) = disk_rows(&disk(vec![v(1_000_000_000_000), v(500_000_000_000)]), false);
        assert_eq!(val(&f, "Mapped volumes"), "1.5 TB");
        // a disk with no lettered volume has no formatted size to give
        let (_, f) = disk_rows(&disk(vec![]), false);
        assert_eq!(val(&f, "Mapped volumes"), UNAVAILABLE);
        // unknown media over a USB bridge, no model, no firmware
        let mut d = disk(vec![]);
        d.bus = "USB";
        d.model.clear();
        d.firmware.clear();
        d.size = 0;
        let (_, f) = disk_rows(&d, false);
        assert_eq!((val(&f, "Type"), val(&f, "Model"), val(&f, "Capacity")), ("Unknown · USB", UNAVAILABLE, UNAVAILABLE));
        assert!(f.iter().all(|x| x.label != "Firmware"));
    }

    #[test]
    fn layout_goes_side_by_side_wide_and_stacks_narrow() {
        let m: Vec<Metric> = (0..7).map(|i| Metric::new(&format!("m{i}"), "1", "")).collect();
        let f: Vec<Fact> = (0..9).map(|i| Fact::new(&format!("f{i}"), "1")).collect();
        let mut meas = |t: &str, _: bool| t.chars().count() as f32 * 7.0;
        let wide = plan(1100.0, false, &m, &f, &mut meas);
        assert!(!wide.stacked && wide.f_cols == 2 && wide.m_cols >= 3);
        let narrow = plan(420.0, true, &m, &f, &mut meas);
        assert!(narrow.stacked && narrow.m_cols >= 3 && narrow.m_cols <= 7);
        assert_eq!(narrow.f_cols, 1);
        // stacked footers are taller than side by side ones, and never zero
        assert!(narrow.h > wide.h);
        let tiny = plan(200.0, true, &m, &f, &mut meas);
        assert!(tiny.m_cols >= 1 && tiny.h.is_finite() && tiny.h > 0.0);
        // nothing to show: just the divider and padding
        assert_eq!(plan(800.0, false, &[], &[], &mut meas).h, 1.0 + PAD_T + PAD_B);
    }

    #[test]
    fn wide_facts_take_a_row_of_their_own() {
        let f = [Fact::new("a", "1"), Fact::new("b", "2"), Fact::new("c", "3").wide(), Fact::new("d", "4")];
        let mut m = |t: &str, _: bool| t.chars().count() as f32 * 7.0;
        assert_eq!(place(&f, 2, 420.0, &mut m).1, 3.0 * ROW_H);
        assert_eq!(place(&f, 1, 420.0, &mut m).1, 4.0 * ROW_H);
        assert_eq!(place(&[], 2, 420.0, &mut m).1, 0.0);
        let (slots, _) = place(&f, 2, 420.0, &mut m);
        assert_eq!((slots[0].col, slots[1].col, slots[2].col, slots[3].col), (Some(0), Some(1), None, Some(0)));
    }

    #[test]
    fn a_wide_fact_too_long_for_its_row_takes_a_second_line() {
        let long = Fact::new("P0 CHANNEL A DIMM 0", "16 GB DDR4 · 3600 MT/s · Corsair CMG32GX4M2D3600C18").wide();
        let short = Fact::new("Model", "NVIDIA GeForce RTX 3080").wide();
        let mut m = |t: &str, _: bool| t.chars().count() as f32 * 7.0;
        let (s, h) = place(&[long.clone(), short.clone()], 1, 270.0, &mut m);
        assert!(s[0].two_line && s[0].h == ROW_H + LINE2 && !s[1].two_line);
        assert_eq!(h, ROW_H * 2.0 + LINE2);
        // a wide row has room for both on one line
        let (s, _) = place(&[long], 2, 900.0, &mut m);
        assert!(!s[0].two_line);
    }

    #[test]
    fn uptime_form() {
        assert_eq!(fmt_uptime(0), "0:00:00:00");
        assert_eq!(fmt_uptime(86400 * 12 + 3 * 3600 + 21 * 60 + 45), "12:03:21:45");
    }
}
