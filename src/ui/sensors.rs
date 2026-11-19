//! Sensors, folded into the Vitals detail views (the design mock):
//! a panel directly under the CPU / GPU / disk header with a temperature (or
//! fan / power) graph, WARM / HOT thresholds and one dense row per channel, and
//! a docked foot (PawnIO notice, "Not detected" strip) under the scrolling
//! detail column.
//!
//! Sources: CPU package temperature (PawnIO), GPU temperature / fan / power
//! (D3DKMT, `sample::gpu`) and storage temperatures (the background storage
//! query). Paint never calls a driver: the sampler reads the GPU and the I/O
//! worker reads PawnIO and the drives; this module only takes the latest
//! snapshot (once per second, from every Vitals paint) and marks values whose
//! refresh hangs or keeps failing as stale. CPU and GPU temperature graphs come
//! from the sampler's 1 h history (`cput`, `gput{i}`); fan, power and drive
//! temperature keep a ring here that only fills while Vitals is painted, so
//! hidden time shows as a gap. No drive polling is added for this.

use super::*;
use crate::sample::Snapshot;
use crate::sensors::CpuRead;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const BAND: u32 = 0x3a414d;
const SPARK: u32 = 0x9aa4b2;
const SW_MM: u32 = 0x3a4250;
const CHIP_ON_LN: u32 = 0x343c49;
/// ACC lifted for hover (not in the token sheet)
const BTN_HOVER: u32 = 0x7ad5e2;

/// Graph windows: (seconds, chip label, row column header, axis label)
/// Range a fresh launch opens on (`RANGES` index 0 = 1 min): the history only starts when the
/// app does, so the hour view would be mostly empty. A range the user picks sticks for the session.
const DEFAULT_RANGE: usize = 0;
const RANGES: [(usize, &str, &str, &str); 3] = [(60, "1m", "LAST 1 MIN", "60 s"), (600, "10m", "LAST 10 MIN", "10 min"), (3600, "1h", "LAST HOUR", "1 h")];
const RING: usize = 3600;

// Hit::Sensor ids. 0 / 1 are the existing Install PawnIO / Restart as admin.
const H_INSTALL: usize = 0;
const H_ADMIN: usize = 1;
const H_RANGE: usize = 2; // 2..=4
const H_ND: usize = 5;
/// the whole docked foot: swallows clicks on it so rows scrolled under it stay inert
const H_FOOT: usize = 6;
const H_PLOT: usize = 7;
/// GPU channel rows
const H_ROW: usize = 100;

/// Which device a Sensors panel / foot belongs to. `Disk` is the index into
/// `Snapshot::disks` (= `vit.disks`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SDev {
    Cpu,
    Gpu(usize),
    Disk(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Cpu,
    Gpu,
    Ssd,
    Hdd,
    Fan,
    Pow,
}

impl Kind {
    /// (warn, crit); None = no state colour (fan, power)
    fn limits(self) -> Option<(f32, f32)> {
        match self {
            Kind::Cpu => Some((80.0, 90.0)),
            Kind::Gpu => Some((80.0, 88.0)),
            Kind::Ssd => Some((60.0, 70.0)),
            Kind::Hdd => Some((45.0, 55.0)),
            Kind::Fan | Kind::Pow => None,
        }
    }
    /// graph / range-bar scale
    fn scale(self) -> (f32, f32) {
        match self {
            Kind::Cpu => (20.0, 100.0),
            Kind::Gpu => (20.0, 95.0),
            Kind::Ssd => (20.0, 80.0),
            Kind::Hdd => (20.0, 65.0),
            Kind::Fan => (0.0, 3000.0),
            Kind::Pow => (0.0, 100.0),
        }
    }
    /// y gridline spacing
    fn step(self) -> f32 {
        match self {
            Kind::Fan => 1000.0,
            Kind::Pow => 25.0,
            _ => 20.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub enum St {
    Ok,
    Warm,
    Hot,
}

/// v ≥ crit → HOT, v ≥ warn → WARM, with 1 °C hysteresis on the way down so
/// a value hovering at the threshold does not flicker the tag.
fn state(kind: Kind, v: Option<f32>, prev: St) -> St {
    let (Some(v), Some((w, c))) = (v, kind.limits()) else { return St::Ok };
    if v >= c || (prev == St::Hot && v >= c - 1.0) {
        St::Hot
    } else if v >= w || (prev >= St::Warm && v >= w - 1.0) {
        St::Warm
    } else {
        St::Ok
    }
}

/// Value colour for a state (header stats, device list).
pub fn st_color(st: St) -> u32 {
    match st {
        St::Ok => TX1,
        St::Warm => WARM,
        St::Hot => HOT,
    }
}

/// Min / max of a graph window.
#[derive(Clone, Copy)]
struct Mm {
    min: f32,
    max: f32,
}

fn window_mm(v: &[f32]) -> Option<Mm> {
    let mut it = v.iter().copied().filter(|x| x.is_finite());
    let first = it.next()?;
    Some(it.fold(Mm { min: first, max: first }, |m, x| Mm { min: m.min.min(x), max: m.max.max(x) }))
}

#[derive(Clone, Copy, PartialEq)]
enum CpuState {
    Unknown,
    Ok,
    NotInstalled,
    NeedsAdmin,
    Failed,
}

pub struct SensorsView {
    cpu: Option<f32>,
    cpu_state: CpuState,
    /// the CPU value is an old one (PawnIO hangs or keeps failing)
    cpu_stale: bool,
    last: Option<Instant>,
    /// last good reading per channel (for "no reading · 40 s")
    last_ok: HashMap<String, Instant>,
    /// 1 Hz samples recorded on every Vitals paint; NaN = no reading / Vitals not shown
    ring: HashMap<String, VecDeque<f32>>,
    /// last WARM/HOT state per temperature channel (hysteresis; drives the device-list dots)
    st: HashMap<String, St>,
    range: usize,
    nd_open: bool,
    /// selected GPU channel
    ch: usize,
    pub hover_x: f32,
}

impl Default for SensorsView {
    fn default() -> Self {
        SensorsView {
            cpu: None,
            cpu_state: CpuState::Unknown,
            cpu_stale: false,
            last: None,
            last_ok: HashMap::new(),
            ring: HashMap::new(),
            st: HashMap::new(),
            range: DEFAULT_RANGE,
            nd_open: false,
            ch: 0,
            hover_x: -1.0,
        }
    }
}

fn disk_kind(d: &crate::sample::Part) -> Kind {
    let bus = d.sub.split(" · ").next().unwrap_or("");
    if crate::sample::disk::is_rotational(bus) { Kind::Hdd } else { Kind::Ssd }
}

/// WARM / HOT thresholds and the drive type for a bus label ("Drive" panel fact).
pub fn disk_limits(bus: &str) -> (f32, f32, &'static str) {
    let (k, ty) = if crate::sample::disk::is_rotational(bus) { (Kind::Hdd, "HDD") } else { (Kind::Ssd, "SSD") };
    let (w, c) = k.limits().unwrap_or((0.0, 0.0));
    (w, c, ty)
}

impl SensorsView {
    /// The user's range choice: kept for the session (device changes and new data never touch it).
    fn set_range(&mut self, i: usize) {
        self.range = i.min(RANGES.len() - 1);
    }

    /// One refresh, throttled to ~1 Hz, from the latest snapshot only. Called
    /// on every Vitals paint, so every device records, not just the selected one.
    pub fn refresh(&mut self, snap: &Snapshot) {
        let now = Instant::now();
        let due = self.last.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(950));
        if !due {
            return;
        }
        // seconds missed since the last refresh (Vitals or window hidden): keep
        // the local rings on wall-clock time by padding them with gaps
        let gap = self.last.map_or(0, |t| (now.duration_since(t).as_secs_f32().round() as usize).saturating_sub(1)).min(RING);
        self.last = Some(now);
        if gap > 0 {
            for r in self.ring.values_mut() {
                for _ in 0..gap {
                    push_ring(r, f32::NAN);
                }
            }
        }

        let (state, cpu) = match snap.cpu_read {
            Some(CpuRead::Ok(t)) => (CpuState::Ok, Some(t)),
            Some(CpuRead::NotInstalled) => (CpuState::NotInstalled, None),
            Some(CpuRead::NeedsAdmin) => (CpuState::NeedsAdmin, None),
            Some(CpuRead::Failed) => (CpuState::Failed, None),
            None => (CpuState::Unknown, None),
        };
        self.cpu_state = state;
        self.cpu = cpu;
        self.cpu_stale = snap.cpu_temp_stale;
        // a stale value is shown (dimmed) but not recorded: the graph gets a gap
        let good = cpu.filter(|_| !snap.cpu_temp_stale);
        self.record("cpu", good, now);
        self.track("cpu", Kind::Cpu, good);

        for (i, g) in snap.gpu_raw.iter().enumerate() {
            self.record(&format!("gpu{i}.temp"), g.temp, now);
            self.record(&format!("gpu{i}.fan"), g.fan_rpm.map(|f| f as f32), now);
            self.record(&format!("gpu{i}.pow"), g.power_pct, now);
            self.track(&format!("gpu{i}.temp"), Kind::Gpu, g.temp);
        }
        // storage temperatures come from the background snapshot (refreshed
        // every 15 s while Vitals is shown); keyed by the drive's title, stable within a session
        for d in &snap.disks {
            let good = d.temp.filter(|_| !d.temp_stale);
            self.record(&d.title, good, now);
            self.track(&d.title, disk_kind(d), good);
        }
    }

    fn record(&mut self, key: &str, v: Option<f32>, now: Instant) {
        let ring = match self.ring.get_mut(key) {
            Some(r) => r,
            None => self.ring.entry(key.to_string()).or_default(),
        };
        push_ring(ring, v.unwrap_or(f32::NAN));
        if v.is_some() {
            match self.last_ok.get_mut(key) {
                Some(t) => *t = now,
                None => {
                    self.last_ok.insert(key.to_string(), now);
                }
            }
        }
    }

    fn track(&mut self, key: &str, kind: Kind, v: Option<f32>) {
        let prev = self.st.get(key).copied().unwrap_or(St::Ok);
        self.st.insert(key.to_string(), state(kind, v, prev));
    }

    /// WARM / HOT state of a temperature channel (device-list dots, header stats).
    pub fn dev_state(&self, key: &str) -> St {
        self.st.get(key).copied().unwrap_or(St::Ok)
    }

    /// Key of a GPU's temperature channel.
    pub fn gpu_key(i: usize) -> String {
        format!("gpu{i}.temp")
    }

    /// CPU package temperature could not be read for a known reason (header shows "—").
    pub fn cpu_missing(&self) -> bool {
        matches!(self.cpu_state, CpuState::NotInstalled | CpuState::NeedsAdmin | CpuState::Failed) || (self.cpu_state == CpuState::Ok && self.cpu_stale)
    }

    /// The last `n` seconds of one channel, oldest first, left-padded with
    /// gaps so the time axis is always the full window.
    fn series(&self, s: &Sensor, n: usize) -> Vec<f32> {
        let from_hist = s.hist.as_deref().and_then(|k| crate::history::tail(k, n)).filter(|v| !v.is_empty());
        let mut v = match from_hist {
            Some(h) => h,
            None => self.ring.get(&s.key).map(|r| r.iter().skip(r.len().saturating_sub(n)).copied().collect()).unwrap_or_default(),
        };
        if v.len() > n {
            v.drain(..v.len() - n);
        }
        if v.len() < n {
            let mut p = vec![f32::NAN; n - v.len()];
            p.extend(v);
            v = p;
        }
        v
    }
}

fn push_ring(r: &mut VecDeque<f32>, v: f32) {
    if r.len() >= RING {
        r.pop_front();
    }
    r.push_back(v);
}

/// Largest finite value per bucket so spikes survive thinning; an all-gap
/// bucket stays a gap. Returns the values and the bucket size in samples.
fn thin(v: &[f32], target: usize) -> (Vec<f32>, usize) {
    if v.len() <= target || target == 0 {
        return (v.to_vec(), 1);
    }
    let step = v.len().div_ceil(target);
    (v.chunks(step).map(|c| if c.iter().any(|x| !x.is_finite()) { f32::NAN } else { c.iter().copied().fold(f32::NAN, f32::max) }).collect(), step)
}

fn ago(sec: usize) -> String {
    if sec <= 1 {
        "now".into()
    } else if sec < 90 {
        format!("{sec} s ago")
    } else {
        let (m, r) = (sec / 60, sec % 60);
        if m < 10 && r > 0 {
            format!("{m} m {r} s ago")
        } else {
            format!("{m} min ago")
        }
    }
}

fn span_s(sec: u64) -> String {
    if sec < 90 {
        format!("{sec} s")
    } else if sec < 3600 {
        format!("{} min", sec / 60)
    } else {
        format!("{} h", sec / 3600)
    }
}

// ---------- channels of one device ----------

struct Sensor {
    key: String,
    /// sampler history series to use for the graph, when one exists
    hist: Option<String>,
    /// drive letter(s), drawn mono before the name
    letter: String,
    name: String,
    /// bus tag (drives) or a short qualifier
    sub: String,
    unit: &'static str,
    dec: usize,
    kind: Kind,
    value: Option<f32>,
    /// `value` is old (its refresh hangs or keeps failing): drawn dimmed
    stale: bool,
}

fn channels(v: &SensorsView, s: &Snapshot, dev: SDev) -> Vec<Sensor> {
    let mut out = Vec::new();
    match dev {
        SDev::Cpu => {
            // the Package row shows while readings work, and stays (stale, "—") after a failure
            if v.cpu_state == CpuState::Ok || (v.cpu_state == CpuState::Failed && v.last_ok.contains_key("cpu")) || v.cpu_state == CpuState::Unknown {
                out.push(Sensor {
                    key: "cpu".into(),
                    hist: Some("cput".into()),
                    letter: String::new(),
                    name: "Package".into(),
                    sub: "Tctl / Tdie".into(),
                    unit: "°C",
                    dec: 1,
                    kind: Kind::Cpu,
                    value: v.cpu,
                    stale: v.cpu_stale,
                });
            }
        }
        SDev::Gpu(i) => {
            if let Some(gpu) = s.gpu_raw.get(i) {
                let mut add = |key: &str, hist: Option<String>, name: &str, sub: &str, unit: &'static str, dec: usize, kind: Kind, val: Option<f32>| {
                    let key = format!("gpu{i}.{key}");
                    // a channel that has never reported is not a row
                    if val.is_some() || v.last_ok.contains_key(&key) {
                        out.push(Sensor { key, hist, letter: String::new(), name: name.into(), sub: sub.into(), unit, dec, kind, value: val, stale: false });
                    }
                };
                add("temp", Some(format!("gput{i}")), "Temperature", "core", "°C", 1, Kind::Gpu, gpu.temp);
                add("fan", None, "Fan", "speed", "RPM", 0, Kind::Fan, gpu.fan_rpm.map(|f| f as f32));
                add("pow", None, "Power", "of board limit", "%", 0, Kind::Pow, gpu.power_pct);
            }
        }
        SDev::Disk(i) => {
            if let Some(d) = s.disks.get(i) {
                if let Some(t) = d.temp {
                    let (letter, model) = d.title.split_once(" · ").unwrap_or(("", &d.title));
                    let bus = d.sub.split(" · ").next().unwrap_or("");
                    let ty = if disk_kind(d) == Kind::Hdd { "HDD" } else { "SSD" };
                    out.push(Sensor {
                        key: d.title.clone(),
                        hist: None,
                        letter: letter.into(),
                        name: model.into(),
                        sub: if bus.contains(ty) { bus.into() } else { format!("{bus} · {ty}") },
                        unit: "°C",
                        dec: 1,
                        kind: disk_kind(d),
                        value: Some(t),
                        stale: d.temp_stale,
                    });
                }
            }
        }
    }
    out
}

/// Channels Blink cannot read for this device ("Not detected" strip).
fn missing(s: &Snapshot, dev: SDev) -> Vec<(String, String)> {
    match dev {
        SDev::Cpu => vec![
            ("Motherboard · VRM · fans".into(), "ITE IT87 Super I/O: PawnIO has no IT87 module yet (LpcIO only)".into()),
            ("DIMM temperature".into(), "needs SMBus (SmbusPIIX4) + SPD hub scan: not implemented".into()),
        ],
        SDev::Gpu(_) => Vec::new(),
        SDev::Disk(_) => s
            .disks
            .iter()
            .filter(|d| d.temp.is_none())
            .map(|d| {
                let (letter, model) = d.title.split_once(" · ").unwrap_or(("", &d.title));
                (format!("{letter} {model}").trim().to_string(), "drive reports no temperature".to_string())
            })
            .collect(),
    }
}

// ---------- layout helpers ----------

/// Detail-column geometry the panel and foot lay out in.
#[derive(Clone, Copy)]
pub struct Geo {
    pub x: f32,
    pub w: f32,
    pub narrow: bool,
}

/// Push a hit region clipped to the scroll viewport (nothing when off-screen).
fn hit(app: &mut App, view: Rect, r: Rect, id: usize) {
    let x0 = r.x.max(view.x);
    let y0 = r.y.max(view.y);
    let x1 = r.r().min(view.r());
    let y1 = r.b().min(view.b());
    if x1 > x0 && y1 > y0 {
        app.hits.push((Rect::new(x0, y0, x1 - x0, y1 - y0), Hit::Sensor(id)));
    }
}

fn fmt_s(s: &Sensor, v: Option<f32>) -> String {
    match v {
        Some(v) if s.kind == Kind::Fan => group_num(v.round().max(0.0) as u64),
        Some(v) => format!("{v:.*}", s.dec),
        None => "—".into(),
    }
}

/// Splits `s` into at most two lines that fit `w` (the second may still ellipsise).
fn wrap2(g: &mut Gfx, s: &str, font: Font, w: f32) -> (String, Option<String>) {
    if g.measure(s, font) <= w {
        return (s.to_string(), None);
    }
    let words: Vec<&str> = s.split(' ').collect();
    let mut line = String::new();
    for (i, word) in words.iter().enumerate() {
        let next = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if g.measure(&next, font) > w && !line.is_empty() {
            return (line, Some(words[i..].join(" ")));
        }
        line = next;
    }
    (line, None)
}

/// Row column positions: (x, w) per column inside the 14 px panel padding.
struct Cols {
    name: (f32, f32),
    spark: (f32, f32),
    range: Option<(f32, f32)>,
    val: (f32, f32),
    tag: (f32, f32),
    mm: Option<(f32, f32)>,
}

fn cols(x: f32, w: f32) -> Cols {
    let (x0, iw) = (x + 14.0, w - 28.0);
    let gap = 12.0;
    // mock: 176 / flex / 140 / 84 / 50 / 74. Narrow detail columns drop RANGE, then MIN–MAX.
    let (range, mm) = if iw >= 704.0 {
        (Some(140.0), Some(74.0))
    } else if iw >= 552.0 {
        (None, Some(74.0))
    } else {
        (None, None)
    };
    let (name_w, val_w, tag_w) = (176.0f32.min((iw - 230.0).max(60.0)), 84.0, 50.0);
    let n = 4 + range.is_some() as usize + mm.is_some() as usize;
    let fixed = name_w + val_w + tag_w + range.unwrap_or(0.0) + mm.unwrap_or(0.0) + gap * (n - 1) as f32;
    let spark_w = (iw - fixed).max(60.0);
    let mut cx = x0;
    let mut take = |cw: f32| {
        let c = (cx, cw);
        cx += cw + gap;
        c
    };
    let name = take(name_w);
    let spark = take(spark_w);
    let range = range.map(&mut take);
    let val = take(val_w);
    let tag = take(tag_w);
    let mm = mm.map(take);
    Cols { name, spark, range, val, tag, mm }
}

/// WARM / HOT tag, right edge at `right`, centred on `cy`.
fn tag(g: &mut Gfx, right: f32, cy: f32, st: St) {
    let (t, bg, ln, tx) = if st == St::Hot { ("HOT", HOT_BG, HOT_LN, HOT) } else { ("WARM", WARN_BG, WARN_LN, WARN_TX) };
    let tf = f(10.5).b().m();
    let w = g.measure(t, tf) + 12.0;
    let r = Rect::new(right - w, cy - 9.0, w, 18.0);
    g.round(r, 4.0, bg);
    g.round_stroke(r, 4.0, ln, 1.0);
    g.text(t, r, tf.c(), tx);
}

/// Track, window min–max band, warn/crit ticks, current-value marker.
fn range_bar(g: &mut Gfx, r: Rect, kind: Kind, mm: Option<Mm>, v: Option<f32>) {
    let (lo, mut hi) = kind.scale();
    if kind == Kind::Fan {
        hi = hi.max(mm.map_or(0.0, |m| m.max * 1.1));
    }
    let p = |x: f32| r.x + ((x - lo) / (hi - lo)).clamp(0.0, 1.0) * r.w;
    g.round(Rect::new(r.x, r.y + 4.0, r.w, 6.0), 3.0, TRACK);
    if let Some(m) = mm {
        let (a, b) = (p(m.min), p(m.max));
        g.round(Rect::new(a, r.y + 4.0, (b - a).max(r.w * 0.008), 6.0), 2.0, BAND);
    }
    if let Some((w, c)) = kind.limits() {
        g.round(Rect::new(p(w) - 1.0, r.y + 1.0, 2.0, 12.0), 1.0, WARN);
        g.round(Rect::new(p(c) - 1.0, r.y + 1.0, 2.0, 12.0), 1.0, HOT);
    }
    if let Some(v) = v {
        g.round(Rect::new(p(v) - 2.5, r.y + 2.0, 3.0, 10.0), 1.0, TX1);
    }
}

/// Diagonal hatch + dashed right edge: the part of a window nothing was recorded for.
fn hatch(g: &mut Gfx, r: Rect, step: f32) {
    if r.w < 2.0 {
        return;
    }
    let step = step.max((r.w + r.h) / 140.0);
    g.clip(r);
    let mut x = r.x - r.h;
    while x < r.r() {
        g.line(x, r.b(), x + r.h, r.y, TRACK, 1.0);
        x += step;
    }
    g.unclip();
    let mut y = r.y;
    while y < r.b() {
        g.fill(Rect::new(r.r() - 1.0, y, 1.0, 3.0f32.min(r.b() - y)), LN);
        y += 6.0;
    }
}

/// Number of leading gap samples (the part of the window that was never recorded).
fn lead_gap(v: &[f32]) -> usize {
    v.iter().position(|x| x.is_finite()).unwrap_or(v.len())
}

fn dev_color(dev: SDev) -> u32 {
    match dev {
        SDev::Cpu => C_CPU,
        SDev::Gpu(_) => C_GPU,
        SDev::Disk(_) => C_DSK,
    }
}

// ---------- panel ----------

const ROW_H: f32 = 40.0;

/// The Sensors panel at `y` in the detail column. `view` is the scroll
/// viewport (hit regions are clipped to it). Returns the bottom edge.
pub fn paint_panel(app: &mut App, g: &mut Gfx, s: &Snapshot, geo: Geo, y: f32, view: Rect, dev: SDev) -> f32 {
    let rows = channels(&app.sensors, s, dev);
    let meta = match dev {
        SDev::Cpu => "CPU package · PawnIO · history 1 h from the sampler".to_string(),
        SDev::Gpu(i) => format!("GPU {i} · D3DKMT · temperature: sampler 1 h · fan, power: recorded while on screen"),
        SDev::Disk(i) => {
            let t = s.disks.get(i).map(|d| d.title.replace(" · ", " ")).unwrap_or_default();
            format!("{t} · storage temperature query · recorded while on screen")
        }
    };
    let x = geo.x;
    let w = geo.w;

    if rows.is_empty() {
        let r = Rect::new(x, y, w, 33.0 + 44.0 + 1.0);
        title_row(app, g, view, r, &meta, false);
        let (m1, m2) = match (dev, app.sensors.cpu_state) {
            (SDev::Cpu, CpuState::NeedsAdmin) => ("Package —", "no reading: needs administrator. See the notice below."),
            (SDev::Cpu, CpuState::NotInstalled) => ("Package —", "no reading: PawnIO not installed. See the notice below."),
            (SDev::Cpu, _) => ("Package —", "no reading: the CPU sensor read failed. See the notice below."),
            (SDev::Gpu(_), _) => ("Temperature —", "the driver reports no temperature, fan or power through D3DKMT."),
            (SDev::Disk(_), _) => ("Temperature —", "drive reports no temperature. Listed under Not detected."),
        };
        let cy = r.y + 33.0 + 22.0;
        g.circle(x + 14.0 + 3.5, cy, 3.5, TX4);
        g.circle(x + 14.0 + 3.5, cy, 2.0, PAN);
        let mf = f(13.0).b().m();
        let mw = g.measure(m1, mf);
        g.text(m1, Rect::new(x + 14.0 + 17.0, cy - 12.0, mw + 2.0, 24.0), mf, TX3);
        g.text(m2, Rect::new(x + 14.0 + 17.0 + mw + 10.0, cy - 12.0, (w - 14.0 - 17.0 - mw - 10.0 - 14.0).max(0.0), 24.0), f(12.0), TX3);
        return r.b();
    }

    let sel = if matches!(dev, SDev::Gpu(_)) { app.sensors.ch.min(rows.len() - 1) } else { 0 };
    let n = RANGES[app.sensors.range].0;
    let sers: Vec<Vec<f32>> = rows.iter().map(|r| app.sensors.series(r, n)).collect();
    let mms: Vec<Option<Mm>> = sers.iter().map(|v| window_mm(v)).collect();

    let plot_h = if rows.len() > 1 { 100.0 } else { 92.0 };
    let sg_h = plot_h + 35.0;
    let h = 33.0 + sg_h + 24.0 + rows.len() as f32 * ROW_H + 1.0;
    let r = Rect::new(x, y, w, h);
    title_row(app, g, view, r, &meta, true);

    let gy = r.y + 33.0;
    paint_graph(app, g, view, geo, r, gy, plot_h, &rows[sel], &sers[sel], mms[sel], dev);

    // column header
    let c = cols(x, w);
    let hy = gy + sg_h;
    g.fill(Rect::new(x + 1.0, hy, w - 2.0, 1.0), SEP);
    g.fill(Rect::new(x + 1.0, hy + 23.0, w - 2.0, 1.0), SEP);
    let hf = f(10.5).b();
    g.text("SENSOR", Rect::new(c.name.0, hy, c.name.1, 24.0), hf, TX4);
    g.text(RANGES[app.sensors.range].2, Rect::new(c.spark.0, hy, c.spark.1, 24.0), hf, TX4);
    if let Some(rc) = c.range {
        g.text("RANGE", Rect::new(rc.0, hy, rc.1, 24.0), hf, TX4);
    }
    g.text("NOW", Rect::new(c.val.0, hy, c.val.1, 24.0), hf.r(), TX4);
    if let Some(mc) = c.mm {
        g.text("MIN–MAX", Rect::new(mc.0, hy, mc.1, 24.0), hf.r(), TX4);
    }

    let pick = rows.len() > 1;
    let mut ry = hy + 24.0;
    for (i, row) in rows.iter().enumerate() {
        let rr = Rect::new(x, ry, w, ROW_H);
        let last = i + 1 == rows.len();
        paint_row(app, g, view, rr, &c, row, &sers[i], mms[i], i, i == sel, pick, last);
        ry += ROW_H;
    }
    r.b()
}

/// Panel frame + title row: "SENSORS", meta, range chips.
fn title_row(app: &mut App, g: &mut Gfx, view: Rect, r: Rect, meta: &str, chips: bool) {
    panel(g, r);
    let tw = g.measure("SENSORS", f(11.0).b());
    g.text("SENSORS", Rect::new(r.x + 13.0, r.y, tw + 2.0, 32.0), f(11.0).b(), TX2);
    g.fill(Rect::new(r.x + 1.0, r.y + 32.0, r.w - 2.0, 1.0), SEP);
    let mut right = r.r() - 12.0;
    if chips {
        let cf = f(12.0);
        let ws: Vec<f32> = RANGES.iter().map(|x| g.measure(x.1, cf) + 20.0).collect();
        let total: f32 = ws.iter().sum::<f32>() + 4.0 * (ws.len() - 1) as f32;
        let mut x = right - total;
        for (i, (rg, cw)) in RANGES.iter().zip(&ws).enumerate() {
            let cr = Rect::new(x, r.y + 5.0, *cw, 22.0);
            let on = i == app.sensors.range;
            let hov = app.hover == Some(Hit::Sensor(H_RANGE + i));
            if on {
                g.round(cr, 11.0, PAN3);
            } else if hov {
                g.round(cr, 11.0, PAN2);
            }
            g.round_stroke(cr, 11.0, if on { CHIP_ON_LN } else { LN }, 1.0);
            g.text(rg.1, cr, cf.c(), if on { TX1 } else { TX2 });
            hit(app, view, cr, H_RANGE + i);
            x += cw + 4.0;
        }
        right -= total + 12.0;
    }
    let mx = r.x + 13.0 + tw + 9.0;
    g.text(meta, Rect::new(mx, r.y, (right - mx).max(0.0), 32.0), f(11.5), TX3);
}

/// y-label column, plot with gridlines / WARM-HOT lines / hover readout, and
/// the Now / Min / Max legend. Returns nothing: height is `plot_h + 35`.
#[allow(clippy::too_many_arguments)]
fn paint_graph(app: &mut App, g: &mut Gfx, view: Rect, geo: Geo, r: Rect, gy: f32, plot_h: f32, s: &Sensor, ser: &[f32], mm: Option<Mm>, dev: SDev) {
    let leg_w = if geo.narrow { 104.0 } else { 128.0 };
    let lab_x = r.x + 14.0;
    let plot = Rect::new(lab_x + 40.0 + 10.0, gy + 10.0, (r.r() - 14.0 - leg_w - 10.0) - (lab_x + 50.0), plot_h);
    let leg_x = r.r() - 14.0 - leg_w;
    let col = dev_color(dev);

    let (lo, mut hi) = s.kind.scale();
    if s.kind == Kind::Fan {
        let top = mm.map_or(0.0, |m| m.max);
        hi = hi.max((top / 1000.0).ceil() * 1000.0);
    }
    let yv = |v: f32| plot.b() - (((v - lo) / (hi - lo)).clamp(0.0, 1.0) * (plot.h - 1.0) + 0.5);

    // gridlines + labels
    let step = s.kind.step();
    let mut t = (lo / step).ceil() * step;
    while t <= hi + 0.01 {
        let ty = yv(t).round();
        g.fill(Rect::new(plot.x, ty, plot.w, 1.0), SEP);
        let lab = if s.kind == Kind::Fan { group_num(t as u64) } else { format!("{t:.0}") };
        g.text(&lab, Rect::new(lab_x, ty - 7.0, 40.0, 14.0), f(10.0).m().r(), TX4);
        t += step;
    }

    // thinned window
    let (vals, bucket) = thin(ser, ((plot.w / 1.5) as usize).max(8));
    let slots = vals.len();
    let gap = lead_gap(&vals);
    if gap > 0 {
        let gw = if gap >= slots { plot.w } else { gap as f32 / (slots - 1).max(1) as f32 * plot.w };
        let gr = Rect::new(plot.x, plot.y, gw.min(plot.w), plot.h);
        hatch(g, gr, 6.0);
        if gw > 230.0 {
            let msg = "not recorded";
            let mw = g.measure(msg, f(11.0)) + 12.0;
            let pr = Rect::new(gr.x + (gr.w - mw) / 2.0, plot.y + (plot.h - 18.0) / 2.0, mw, 18.0);
            g.round(pr, 3.0, PAN);
            g.text(msg, pr, f(11.0).c(), TX4);
        }
    }

    // WARM / HOT dashed lines (labels drawn after the line so they stay readable)
    let lim = s.kind.limits();
    if let Some((w, c)) = lim {
        g.dash_h(plot.x, plot.r(), yv(w).round(), 3.0, 3.0, WARN_LN, 1.0);
        g.dash_h(plot.x, plot.r(), yv(c).round(), 3.0, 3.0, HOT_LN, 1.0);
    }
    g.series(plot, &vals, lo, hi, col, 0.16, 1.5, false);
    if let Some((w, c)) = lim {
        let lf = f(10.0).m();
        for (v, txt, color, above) in [(w, format!("WARM {w:.0}"), WARN, false), (c, format!("HOT {c:.0}"), HOT, true)] {
            let lw = g.measure(&txt, lf) + 6.0;
            let ly = yv(v).round();
            let lr = Rect::new(plot.r() - 2.0 - lw, if above { ly - 12.0 } else { ly }, lw, 12.0);
            g.round(lr, 2.0, PAN);
            g.text(&txt, lr, lf.c(), color);
        }
    }

    // axis labels
    g.text(&format!("−{}", RANGES[app.sensors.range].3), Rect::new(plot.x, plot.b() + 4.0, 80.0, 12.0), f(10.0).m(), TX4);
    g.text("now", Rect::new(plot.r() - 40.0, plot.b() + 4.0, 40.0, 12.0), f(10.0).m().r(), TX4);

    // legend: Now / Min / Max (+ thresholds)
    let dim = s.value.is_none() || s.stale;
    let unit = if s.unit == "%" { " %" } else { &format!(" {}", s.unit) };
    let rows: [(u32, &str, String, bool); 3] = [
        (col, "Now", format!("{}{unit}", fmt_s(s, s.value)), false),
        (SW_MM, "Min", mm.map(|m| fmt_s(s, Some(m.min))).unwrap_or_else(|| "—".into()), true),
        (SW_MM, "Max", mm.map(|m| fmt_s(s, Some(m.max))).unwrap_or_else(|| "—".into()), true),
    ];
    let mut ly = plot.y;
    for (c, label, value, sm) in rows {
        g.round(Rect::new(leg_x, ly + 6.0, 8.0, 8.0), 2.0, c);
        g.text(label, Rect::new(leg_x + 14.0, ly, 40.0, 20.0), f(11.0), TX3);
        let vf = if sm { f(11.5).m().r() } else { f(13.0).m().b().r() };
        let vc = if sm { TX2 } else if dim { TX3 } else { TX1 };
        g.text(&value, Rect::new(leg_x + 40.0, ly, leg_w - 40.0, 20.0), vf, vc);
        ly += 22.0;
    }
    if let Some((w, c)) = lim {
        let lf = f(10.5);
        let pre = "warm / hot at ";
        let pw = g.measure(pre, lf);
        g.text(pre, Rect::new(leg_x, plot.b() - 14.0, pw + 2.0, 14.0), lf, TX4);
        g.text(&format!("{w:.0} / {c:.0}"), Rect::new(leg_x + pw, plot.b() - 14.0, leg_w - pw, 14.0), f(10.5).m(), TX3);
    }

    // hover crosshair + readout
    hit(app, view, plot, H_PLOT);
    let (mx, my) = (app.mx, app.my);
    if slots > 1 && mx >= plot.x && mx < plot.r() && my >= plot.y && my < plot.b() && my >= view.y && my < view.b() {
        let i = (((mx - plot.x) / plot.w) * (slots - 1) as f32).round().clamp(0.0, (slots - 1) as f32) as usize;
        if let Some(v) = vals.get(i).copied().filter(|v| v.is_finite()) {
            let x = (plot.x + i as f32 / (slots - 1) as f32 * plot.w).round();
            g.fill_a(Rect::new(x, plot.y, 1.0, plot.h), TX3, 0.9);
            let val = format!("{} {}", fmt_s(s, Some(v)), s.unit);
            let when = format!(" · {}", ago((slots - 1 - i) * bucket));
            let rf = f(11.0).m();
            let (vw, ww) = (g.measure(&val, rf), g.measure(&when, rf));
            let bw = vw + ww + 14.0;
            let flip = x > plot.x + plot.w * 0.6;
            let bx = if flip { x - 6.0 - bw } else { x + 6.0 };
            let b = Rect::new(bx, plot.y - 1.0, bw, 18.0);
            g.round(b, 4.0, PAN3);
            g.round_stroke(b, 4.0, LN, 1.0);
            g.text(&val, Rect::new(b.x + 7.0, b.y, vw + 2.0, b.h), rf, TX1);
            g.text(&when, Rect::new(b.x + 7.0 + vw, b.y, ww + 2.0, b.h), rf, TX3);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_row(app: &mut App, g: &mut Gfx, view: Rect, rr: Rect, c: &Cols, s: &Sensor, ser: &[f32], mm: Option<Mm>, idx: usize, sel: bool, pick: bool, last: bool) {
    let hovered = pick && app.hover == Some(Hit::Sensor(H_ROW + idx));
    // zebra on the 1st / 3rd row like the mock (its header counts as a sibling)
    let bg = if sel {
        Some(ACC_BG)
    } else if hovered {
        Some(PAN2)
    } else if idx.is_multiple_of(2) {
        Some(ZEBRA)
    } else {
        None
    };
    if let Some(bgc) = bg {
        let fr = Rect::new(rr.x + 1.0, rr.y, rr.w - 2.0, rr.h);
        if last {
            g.round(fr, 6.0, bgc);
            g.fill(Rect::new(fr.x, fr.y, fr.w, 8.0), bgc);
        } else {
            g.fill(fr, bgc);
        }
    }
    if sel {
        g.round(Rect::new(rr.x + 1.0, rr.y + 9.0, 2.0, rr.h - 18.0), 1.0, ACC);
    }
    if pick {
        hit(app, view, rr, H_ROW + idx);
    }

    // name cell: drive letter mono + model, bus / qualifier underneath
    let mut nx = c.name.0;
    let end = c.name.0 + c.name.1;
    if !s.letter.is_empty() {
        let lf = f(11.5).m();
        let lw = g.measure(&s.letter, lf);
        g.text(&s.letter, Rect::new(nx, rr.y + 4.0, (end - nx).max(0.0), 17.0), lf, TX3);
        nx += lw + 8.0;
    }
    g.text(&s.name, Rect::new(nx, rr.y + 4.0, (end - nx).max(0.0), 17.0), f(12.5), TX1);
    g.text(&s.sub, Rect::new(c.name.0, rr.y + 21.0, c.name.1, 15.0), f(11.0), if sel { TX2 } else { TX3 });

    // spark: auto scale over the window, flat drive stays flat
    let sp = Rect::new(c.spark.0, rr.y + (rr.h - 26.0) / 2.0, c.spark.1, 26.0);
    let (vals, _) = thin(ser, ((sp.w / 1.5) as usize).max(8));
    let slots = vals.len();
    let col = if matches!(s.kind, Kind::Fan | Kind::Pow) { SPARK } else if s.kind == Kind::Cpu { C_CPU } else if s.kind == Kind::Gpu { C_GPU } else { C_DSK };
    match mm {
        Some(m) => {
            let gap = lead_gap(&vals);
            if gap > 0 {
                let gw = if gap >= slots { sp.w } else { gap as f32 / (slots - 1).max(1) as f32 * sp.w };
                hatch(g, Rect::new(sp.x, sp.y, gw.min(sp.w), sp.h), 5.0);
            }
            let pad = ((m.max - m.min) * 0.25).max(1.0);
            g.series(sp, &vals, m.min - pad, m.max + pad, col, 0.0, 1.2, false);
        }
        None => g.dash_h(sp.x, sp.r(), sp.b() - 0.5, 2.0, 2.0, TX4, 1.0),
    }

    if let Some(rc) = c.range {
        range_bar(g, Rect::new(rc.0, rr.y + (rr.h - 14.0) / 2.0, rc.1, 14.0), s.kind, mm, s.value.filter(|_| !s.stale));
    }

    // value + unit, right-aligned
    let uf = f(11.0);
    let uw = g.measure(s.unit, uf);
    let vr = c.val.0 + c.val.1;
    g.text(s.unit, Rect::new(vr - uw - 1.0, rr.y + 1.0, uw + 2.0, rr.h), uf, TX3);
    let vcol = if s.value.is_some() && !s.stale { TX1 } else { TX3 };
    g.text(&fmt_s(s, s.value), Rect::new(c.val.0 - 10.0, rr.y, c.val.1 + 10.0 - uw - 3.0, rr.h), f(13.0).b().m().r(), vcol);

    let st = if s.value.is_some() && !s.stale { app.sensors.dev_state(&s.key) } else { St::Ok };
    if st != St::Ok {
        tag(g, c.tag.0 + c.tag.1, rr.y + rr.h / 2.0, st);
    }

    // min–max, or how long the channel has been silent
    if let Some(mc) = c.mm {
        let silent = if s.value.is_none() || s.stale { app.sensors.last_ok.get(&s.key).map(|t| t.elapsed().as_secs()) } else { None };
        let (txt, tc) = match (silent, mm) {
            (Some(sec), _) if s.stale => (format!("stale · {}", span_s(sec)), WARN),
            (Some(sec), _) => (format!("no reading · {}", span_s(sec)), WARN),
            (None, Some(m)) => (format!("{}–{}", fmt_s(s, Some(m.min)), fmt_s(s, Some(m.max))), if sel { TX2 } else { TX3 }),
            _ => (String::new(), TX3),
        };
        if !txt.is_empty() {
            g.text(&txt, Rect::new(mc.0 - 50.0, rr.y, mc.1 + 50.0, rr.h), f(11.0).m().r(), tc);
        }
    }
}

// ---------- docked foot ----------

/// (glyph, title, text, code line, icon-tile tick colour, button (label, hit id))
struct Banner {
    title: &'static str,
    l1: String,
    l2: Option<String>,
    code: Option<&'static str>,
    tick: u32,
    btn: Option<(&'static str, usize, f32)>,
    h: f32,
}

/// The PawnIO notice (CPU only) and the "Not detected" strip, docked under the
/// scrolling detail column. `h` is 0 when there is nothing to dock.
pub struct Foot {
    banner: Option<Banner>,
    nd: Vec<(String, String)>,
    pub h: f32,
}

pub fn foot(app: &App, g: &mut Gfx, s: &Snapshot, w: f32, dev: SDev) -> Foot {
    let nd = missing(s, dev);
    let action = if dev == SDev::Cpu {
        match app.sensors.cpu_state {
            CpuState::NeedsAdmin => Some((
                "CPU temperature needs administrator",
                "Reading the thermal register needs an elevated process. Nothing elevates silently and there is no logon task. GPU and drive temperatures keep working.",
                Some("UAC prompt, then the new Hayate waits for this one to exit (--wait-pid)"),
                WARN,
                Some(("Restart as admin", H_ADMIN)),
            )),
            CpuState::NotInstalled => Some((
                "PawnIO driver not installed",
                "Hayate reads the CPU package temperature through the PawnIO driver and never installs or starts it on its own. GPU and drive temperatures keep working.",
                Some(if crate::osutil::cold_started() { "Run Hayate normally to install PawnIO" } else { "winget install -e --id namazso.PawnIO · UAC prompt" }),
                WARN,
                if crate::osutil::cold_started() { None } else { Some(("Install PawnIO", H_INSTALL)) },
            )),
            CpuState::Failed => Some((
                "CPU sensor read failed",
                "PawnIO returned an error for SMN 0x59800 (another tool may hold Access_PCI). Retrying periodically.",
                None,
                WARN,
                None,
            )),
            _ => None,
        }
    } else {
        None
    };
    let banner = action.map(|(title, text, code, tick, btn)| {
        let btn = btn.map(|(label, id)| (label, id, g.measure(label, f(12.0).b()) + 28.0));
        let text_w = (w - 2.0 - 16.0 - 32.0 - 12.0 - 12.0 - 14.0 - btn.map_or(0.0, |b| b.2 + 12.0)).max(80.0);
        let (l1, l2) = wrap2(g, text, f(11.5), text_w);
        let lines = 1 + l2.is_some() as usize;
        let body = 17.0 + 1.0 + 16.0 * lines as f32 + if code.is_some() { 1.0 + 15.0 } else { 0.0 };
        Banner { title, l1, l2, code, tick, btn, h: body.max(32.0) + 18.0 + 2.0 }
    });
    let mut h = 0.0;
    if let Some(b) = &banner {
        h += b.h;
    }
    if !nd.is_empty() {
        if h > 0.0 {
            h += 8.0;
        }
        h += nd_h(app, nd.len());
    }
    Foot { banner, nd, h }
}

fn nd_h(app: &App, n: usize) -> f32 {
    2.0 + 30.0 + if app.sensors.nd_open { 26.0 * n as f32 } else { 0.0 }
}

/// Paint the foot into `r` (bottom of the detail column).
pub fn paint_foot(app: &mut App, g: &mut Gfx, ft: &Foot, r: Rect) {
    app.hits.push((r, Hit::Sensor(H_FOOT)));
    let mut y = r.y;
    if let Some(b) = &ft.banner {
        let br = Rect::new(r.x, y, r.w, b.h);
        panel(g, br);
        g.round(Rect::new(br.x, br.y + 8.0, 2.0, br.h - 16.0), 1.0, b.tick);
        let ic = Rect::new(br.x + 16.0, br.y + (br.h - 32.0) / 2.0, 32.0, 32.0);
        g.round(ic, 8.0, PAN2);
        g.round_stroke(ic, 8.0, LN, 1.0);
        g.text("\u{E9CA}", ic, f(15.0).icon().c(), TX3);
        let tx = ic.r() + 12.0;
        let tw = (br.r() - 14.0 - 1.0 - b.btn.map_or(0.0, |x| x.2 + 12.0) - tx).max(40.0);
        let lines = 1 + b.l2.is_some() as usize;
        let body = 17.0 + 1.0 + 16.0 * lines as f32 + if b.code.is_some() { 1.0 + 15.0 } else { 0.0 };
        let mut ty = br.y + (br.h - body) / 2.0;
        g.text(b.title, Rect::new(tx, ty, tw, 17.0), f(12.5).b(), TX1);
        ty += 18.0;
        g.text(&b.l1, Rect::new(tx, ty, tw, 16.0), f(11.5), TX3);
        ty += 16.0;
        if let Some(l2) = &b.l2 {
            g.text(l2, Rect::new(tx, ty, tw, 16.0), f(11.5), TX3);
            ty += 16.0;
        }
        if let Some(code) = b.code {
            g.text(code, Rect::new(tx, ty + 1.0, tw, 15.0), f(11.0).m(), TX2);
        }
        if let Some((label, id, bw)) = b.btn {
            let bt = Rect::new(br.r() - 14.0 - 1.0 - bw, br.y + (br.h - 28.0) / 2.0, bw, 28.0);
            let hov = app.hover == Some(Hit::Sensor(id));
            let fill = if hov { BTN_HOVER } else { ACC };
            g.round(bt, 6.0, fill);
            g.text(label, bt, f(12.0).b().c(), BG);
            app.hits.push((bt, Hit::Sensor(id)));
        }
        y += b.h + 8.0;
    }
    if !ft.nd.is_empty() {
        let open = app.sensors.nd_open;
        let pr = Rect::new(r.x, y, r.w, nd_h(app, ft.nd.len()));
        panel(g, pr);
        let strip = Rect::new(pr.x + 1.0, pr.y + 1.0, pr.w - 2.0, 30.0);
        if app.hover == Some(Hit::Sensor(H_ND)) {
            g.clip(strip);
            g.round(pr.inset(1.0, 1.0), 6.0, PAN2);
            g.unclip();
        }
        let tf = f(11.0).b();
        let tw = g.measure("NOT DETECTED", tf);
        g.text("NOT DETECTED", Rect::new(pr.x + 13.0, strip.y, tw + 2.0, 30.0), tf, TX2);
        let cnt = format!("{} channel{}", ft.nd.len(), if ft.nd.len() > 1 { "s" } else { "" });
        g.text(&cnt, Rect::new(pr.x + 13.0 + tw + 8.0, strip.y, 120.0, 30.0), f(11.0).m(), TX3);
        g.text(if open { "\u{E70E}" } else { "\u{E70D}" }, Rect::new(pr.r() - 12.0 - 12.0, strip.y, 12.0, 30.0), f(8.0).icon().c(), TX3);
        app.hits.push((strip, Hit::Sensor(H_ND)));
        if open {
            let mut ry = strip.b();
            for (i, (name, why)) in ft.nd.iter().enumerate() {
                let rr = Rect::new(pr.x + 1.0, ry, pr.w - 2.0, 26.0);
                if !i.is_multiple_of(2) {
                    g.fill(rr, ZEBRA);
                }
                g.fill(Rect::new(rr.x, rr.y, rr.w, 1.0), SEP);
                g.text(name, Rect::new(pr.x + 14.0, ry, 230.0, 26.0), f(12.0), TX1);
                g.text(why, Rect::new(pr.x + 14.0 + 230.0 + 14.0, ry, (pr.r() - 14.0 - (pr.x + 258.0)).max(0.0), 26.0), f(11.5).m(), TX3);
                ry += 26.0;
            }
        }
    }
}

// ---------- input ----------

pub fn click(app: &mut App, id: usize) -> Action {
    match id {
        H_INSTALL => Action::InstallPawnIO,
        H_ADMIN => Action::RestartAdmin,
        i if (H_RANGE..H_RANGE + RANGES.len()).contains(&i) => {
            app.sensors.set_range(i - H_RANGE);
            Action::Repaint
        }
        H_ND => {
            app.sensors.nd_open = !app.sensors.nd_open;
            Action::Repaint
        }
        i if i >= H_ROW => {
            app.sensors.ch = i - H_ROW;
            Action::Repaint
        }
        _ => Action::None,
    }
}

/// Mouse move over the graph: repaint when the crosshair moved (2 px steps).
pub fn hover_moved(app: &mut App, h: Option<Hit>, x: f32) -> bool {
    if h == Some(Hit::Sensor(H_PLOT)) && (x - app.sensors.hover_x).abs() >= 2.0 {
        app.sensors.hover_x = x;
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_preserves_time_away_and_stale_gaps() {
        let mut v = SensorsView::default();
        let mut snap = Snapshot::default();
        snap.disks.push(crate::sample::Part {
            title: "C: · SSD".into(), value: 0.0, unit: "MB/s", max: 1.0,
            sub: "NVMe".into(), temp: Some(40.0), temp_stale: false, hist: vec![],
        });
        v.refresh(&snap);
        v.last = Some(Instant::now() - Duration::from_secs(5));
        v.refresh(&snap);
        let r = &v.ring["C: · SSD"];
        assert_eq!(r.len(), 6);
        assert_eq!(r[0], 40.0);
        assert!(r.iter().skip(1).take(4).all(|x| x.is_nan()));
        assert_eq!(r[5], 40.0);
        snap.disks[0].temp_stale = true;
        v.last = Some(Instant::now() - Duration::from_secs(1));
        v.refresh(&snap);
        assert!(v.ring["C: · SSD"].back().unwrap().is_nan());
    }

    #[test]
    fn fresh_launch_opens_on_one_minute_and_keeps_the_users_range() {
        assert_eq!(RANGES[DEFAULT_RANGE].1, "1m", "a fresh launch has no hour of history yet");
        assert_eq!(SensorsView::default().range, 0);
        let snap = Snapshot::default();
        for pick in [1, 2] {
            let mut v = SensorsView::default();
            v.set_range(pick);
            // new data arriving never switches the range back or forward
            for _ in 0..3 {
                v.last = Some(Instant::now() - Duration::from_secs(2));
                v.refresh(&snap);
            }
            assert_eq!(v.range, pick);
        }
        let mut v = SensorsView::default();
        v.set_range(99);
        assert_eq!(v.range, RANGES.len() - 1, "an out-of-range id never indexes past RANGES");
    }

    #[test]
    fn state_thresholds_and_hysteresis() {
        assert_eq!(state(Kind::Cpu, Some(79.9), St::Ok), St::Ok);
        assert_eq!(state(Kind::Cpu, Some(80.0), St::Ok), St::Warm);
        assert_eq!(state(Kind::Cpu, Some(90.0), St::Ok), St::Hot);
        // on the way down: stays HOT within 1 °C of crit, WARM within 1 °C of warn
        assert_eq!(state(Kind::Cpu, Some(89.2), St::Hot), St::Hot);
        assert_eq!(state(Kind::Cpu, Some(88.9), St::Hot), St::Warm);
        assert_eq!(state(Kind::Cpu, Some(79.5), St::Warm), St::Warm);
        assert_eq!(state(Kind::Cpu, Some(78.9), St::Warm), St::Ok);
        assert_eq!(state(Kind::Hdd, Some(56.0), St::Ok), St::Hot);
        assert_eq!(state(Kind::Fan, Some(9000.0), St::Ok), St::Ok);
        assert_eq!(state(Kind::Gpu, None, St::Hot), St::Ok);
    }

    #[test]
    fn thin_keeps_peaks_and_gaps() {
        assert!(thin(&[1.0, f32::NAN, 2.0, 3.0], 2).0[0].is_nan());
        let v = [1.0, 5.0, f32::NAN, f32::NAN, 2.0, 3.0];
        let (t, step) = thin(&v, 3);
        assert_eq!(step, 2);
        assert_eq!(t[0], 5.0);
        assert!(t[1].is_nan());
        assert_eq!(t[2], 3.0);
    }

    #[test]
    fn ago_text() {
        assert_eq!(ago(0), "now");
        assert_eq!(ago(40), "40 s ago");
        assert_eq!(ago(134), "2 m 14 s ago");
        assert_eq!(ago(600), "10 min ago");
    }

    #[test]
    fn lead_gap_counts_unrecorded_prefix() {
        assert_eq!(lead_gap(&[f32::NAN, f32::NAN, 1.0, f32::NAN, 2.0]), 2);
        assert_eq!(lead_gap(&[1.0, 2.0]), 0);
        assert_eq!(lead_gap(&[f32::NAN; 3]), 3);
    }

    #[test]
    fn window_mm_skips_gaps() {
        let m = window_mm(&[f32::NAN, 40.0, f32::NAN, 55.5, 47.0]).unwrap();
        assert_eq!((m.min, m.max), (40.0, 55.5));
        assert!(window_mm(&[f32::NAN; 4]).is_none());
    }

    #[test]
    fn columns_fit_the_mock_widths() {
        // wide detail column: 176 / flex / 140 / 84 / 50 / 74 inside the 14 px padding
        let c = cols(0.0, 760.0);
        assert_eq!(c.name.1, 176.0);
        assert_eq!(c.range.unwrap().1, 140.0);
        let m = c.mm.unwrap();
        assert!((m.0 + m.1 - (760.0 - 14.0)).abs() < 0.01);
        // narrow: RANGE goes first, then MIN–MAX
        assert!(cols(0.0, 640.0).range.is_none() && cols(0.0, 640.0).mm.is_some());
        assert!(cols(0.0, 520.0).mm.is_none());
    }
}
