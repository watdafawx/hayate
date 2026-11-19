//! Background sampler. Runs on its own thread, publishes an immutable
//! `Snapshot` once per tick and pokes the window to repaint.
//! While the window is hidden it only samples the cheap system-wide numbers
//! (for history); the process walk runs only while visible.

pub mod disk;
pub mod gpu;
pub mod gpuproc;
pub mod hw;
pub mod meta;
pub mod net;
pub mod baseline;
pub mod nethealth;
pub mod nt;
pub mod probe;
pub mod ring;
pub mod sys;
pub mod who;
pub mod workload;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use crate::{history, leak};
use windows::core::BOOL;
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const HIST: usize = 60;
pub const WM_APP_DATA: u32 = WM_APP + 1;
pub const WM_APP_TRAYTICK: u32 = WM_APP + 6;

pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: Arc<str>,
    pub create: i64,
    pub cpu: f32,
    pub mem: u64,
    pub io_bps: f64,
    pub threads: u32,
    pub handles: u32,
    pub session: u32,
    pub windowed: bool,
    pub meta: Arc<meta::Meta>,
    /// busiest GPU engine's utilisation, 0..100 (only while `PROC_GPU`)
    pub gpu: f32,
    pub gpu_engine: Option<Arc<str>>,
}

/// Set by the UI while the Processes tab shows a GPU column; the sampler
/// queries per-process GPU only then (and only while the window is shown).
pub static PROC_GPU: AtomicBool = AtomicBool::new(false);
/// Set by the UI while Vitals shows the CPU detail (its Sensors panel): CPU temperature every second.
pub static SENSORS_TAB: AtomicBool = AtomicBool::new(false);
/// The window is minimized: shown as far as the sampler is concerned, but nothing is on screen, so
/// the opt-in network probes do not send (`set_minimized`).
static MINIMIZED: AtomicBool = AtomicBool::new(false);

pub fn set_minimized(v: bool) {
    MINIMIZED.store(v, Ordering::Relaxed);
    if v {
        probe::release();
    }
}

/// Mirror of `Shared::visible` for code without the `Shared` (stall log context).
pub static VISIBLE: AtomicBool = AtomicBool::new(false);

/// Which Vitals device the UI shows (`VIT_*`), 0 when Vitals is not on
/// screen. Detail reads that cost a syscall (memory lists, volume space,
/// adapter addresses, socket counts) run only for the selected device.
pub static VIT_SEL: AtomicU8 = AtomicU8::new(0);
pub const VIT_CPU: u8 = 1;
pub const VIT_MEM: u8 = 2;
pub const VIT_GPU: u8 = 3;
pub const VIT_DISK: u8 = 4;
pub const VIT_NET: u8 = 5;
/// LUID of the adapter whose Network detail is shown (set by the UI with `VIT_SEL`).
pub static VIT_NIC: AtomicU64 = AtomicU64::new(0);

/// Vitals detail data, built only while the window is shown. Every series
/// is `HIST` long, oldest first, NaN = no sample (see `ring`).
#[derive(Default)]
pub struct Vit {
    /// per logical processor load %, `cores.len()` rows x HIST, row-major
    pub cores: Vec<f32>,
    /// commit charge, GB
    pub commit: Vec<f32>,
    /// physical memory lists (only while Memory is selected)
    pub lists: Option<sys::MemLists>,
    /// Memory Compression working set, bytes
    pub compressed: Option<u64>,
    /// aligned with `Snapshot.disks`
    pub disks: Vec<VDisk>,
    /// aligned with `Snapshot.nics` (connected adapters)
    pub nics: Vec<VNic>,
    /// hardware adapters left out because they are not connected
    pub nics_hidden: usize,
    /// aligned with `Snapshot.gpus` / `gpu_raw`
    pub gpus: Vec<VGpu>,
    /// every adapter's addresses (only while a NIC is selected)
    pub addrs: Vec<net::Addr>,
    pub conns: Option<net::Conns>,
    /// passive health counters (only while a NIC is selected; empty until two reads are in)
    pub health: nethealth::Health,
    /// opt-in probe results for the selected adapter (empty unless probes are on and running)
    pub probe: probe::View,
    /// per-process traffic, all adapters (ETW; only while Network is selected)
    pub net_top: crate::netetw::Feed,
    /// 60 s history of `net_top`'s LAN / WAN / All totals (only while Network is selected)
    pub net_scope: crate::netscope::Series,
}

pub struct VDisk {
    pub num: u32,
    pub model: String,
    pub bus: &'static str,
    pub rotational: Option<bool>,
    pub firmware: String,
    pub letters: String,
    pub size: u64,
    pub read_bps: f64,
    pub write_bps: f64,
    pub active: f32,
    pub resp_ms: Option<f32>,
    pub queue: u32,
    pub system: bool,
    pub pagefile: bool,
    pub temp: Option<f32>,
    /// volume space (only while a disk is selected; empty until the worker answers)
    pub vols: Vec<disk::Volume>,
    /// MB/s
    pub read: Vec<f32>,
    pub write: Vec<f32>,
    /// active time %
    pub act: Vec<f32>,
    /// average response, ms
    pub resp: Vec<f32>,
    pub qlen: Vec<f32>,
}

pub struct VNic {
    pub luid: u64,
    pub name: String,
    pub desc: String,
    pub kind: &'static str,
    pub link_bps: u64,
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_total: u64,
    pub tx_total: u64,
    /// Mb/s
    pub rx: Vec<f32>,
    pub tx: Vec<f32>,
}

pub struct VGpu {
    pub util: Vec<f32>,
    /// dedicated memory in use, % of total
    pub vram: Vec<f32>,
    /// MHz
    pub clock: Vec<f32>,
    /// engine groups ("3D", "Copy", ...) and their load, `names.len()` rows x HIST
    pub names: Vec<Arc<str>>,
    pub eng: Vec<f32>,
}

/// 60 s rings behind `Vit`. They keep advancing while hidden (from numbers
/// the hidden tick reads anyway), so the timelines have no hole after a show.
#[derive(Default)]
struct DiskRings {
    read: ring::Ring,
    write: ring::Ring,
    act: ring::Ring,
    resp: ring::Ring,
    qlen: ring::Ring,
}
#[derive(Default)]
struct GpuRings {
    vram: ring::Ring,
    clock: ring::Ring,
    names: Vec<Arc<str>>,
    eng: ring::Rings,
}

/// When the sampler posted the data (or tray-tick) message the UI has not
/// handled yet (`timing::now_us`, 0 = none in the queue). The sampler posts
/// only when none is pending: a busy UI thread finds one message, not a
/// backlog of them, and paints the newest snapshot once.
pub static DATA_POSTED: AtomicU64 = AtomicU64::new(0);
pub static TRAY_POSTED: AtomicU64 = AtomicU64::new(0);
/// Ticks folded into the pending message (for the `stall: ui-queue` line).
pub static COALESCED: AtomicU32 = AtomicU32::new(0);

/// Post `msg` unless one is still queued. A message pending for > 5 s (lost,
/// or the window was recreated) is posted again.
fn post_coalesced(hwnd: isize, msg: u32, slot: &AtomicU64) {
    let now = crate::timing::now_us();
    let prev = slot.load(Ordering::SeqCst);
    if prev != 0 && now.saturating_sub(prev) < 5_000_000 {
        COALESCED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    slot.store(now, Ordering::SeqCst);
    let ok = unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(0)).is_ok() };
    if !ok {
        slot.store(0, Ordering::SeqCst);
    }
}

#[derive(Clone)]
pub struct Part {
    pub title: String,
    pub value: f64,
    pub unit: &'static str,
    pub max: f32,
    pub sub: String,
    pub temp: Option<f32>,
    /// `temp` is an old value: its refresh hangs or keeps failing (bgio::STALE)
    pub temp_stale: bool,
    pub hist: Vec<f32>,
}

#[derive(Default)]
pub struct Snapshot {
    pub seq: u64,
    pub st: Arc<sys::Static>,
    pub cpu: f32,
    pub cores: Vec<f32>,
    pub cpu_hist: Vec<f32>,
    pub mem: sys::Mem,
    pub mem_hist: Vec<f32>,
    pub gpus: Vec<Part>,
    pub disks: Vec<Part>,
    pub nics: Vec<Part>,
    pub procs: Vec<Proc>,
    pub nprocs: usize,
    pub threads: u32,
    pub handles: u32,
    pub uptime: u64,
    pub sample_ms: f32,
    /// current CPU clock (PDH), only while Vitals shows the CPU; None = no reading
    pub cpu_clock: Option<hw::CpuClock>,
    /// CPU package temperature (PawnIO), only while shown and elevated
    pub cpu_temp: Option<f32>,
    /// last PawnIO answer (None = not asked yet) and whether it is stale
    pub cpu_read: Option<crate::sensors::CpuRead>,
    pub cpu_temp_stale: bool,
    /// raw GPU readings (temperature, fan, power) for the Vitals Sensors panel
    pub gpu_raw: Vec<gpu::Gpu>,
    /// every rate on Vitals (CPU, per-process CPU, GPU, disks, NICs) comes from a real delta
    pub ready: bool,
    /// Vitals detail (empty while hidden)
    pub vit: Vit,
}

pub struct Shared {
    pub snap: Mutex<Arc<Snapshot>>,
    pub visible: AtomicBool,
    pub hwnd: AtomicIsize,
    wake: (Mutex<bool>, Condvar),
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Shared {
            snap: Mutex::new(Arc::new(Snapshot::default())),
            visible: AtomicBool::new(false),
            hwnd: AtomicIsize::new(0),
            wake: (Mutex::new(false), Condvar::new()),
        })
    }
    pub fn get(&self) -> Arc<Snapshot> {
        self.snap.lock().unwrap().clone()
    }
    pub fn set_visible(&self, v: bool) {
        let was = self.visible.swap(v, Ordering::SeqCst);
        VISIBLE.store(v, Ordering::Relaxed);
        if !v {
            // hidden or minimized: the opt-in probes stop sending now, not at the next tick
            probe::release();
        }
        if v && !was {
            let (m, c) = &self.wake;
            *m.lock().unwrap() = true;
            c.notify_one();
        }
    }
}

fn push(h: &mut VecDeque<f32>, v: f32) {
    if h.len() >= HIST {
        h.pop_front();
    }
    h.push_back(v);
}
fn hist_vec(h: &VecDeque<f32>) -> Vec<f32> {
    let mut v = vec![0.0; HIST - h.len()];
    v.extend(h.iter().copied());
    v
}

struct Prev {
    cpu: i64,
    io: i64,
}

pub fn spawn(shared: Arc<Shared>) {
    crate::bgio::start();
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || run(shared))
        .unwrap();
}

/// Per-tick phase timings: always taken (one clock read per phase, for the
/// `stall: sampler` line), logged every tick with `BLINK_TIMING=1`
/// (`tick:` lines in blink.log: wall time, tick gap, then ms per phase).
struct Phases {
    on: bool,
    last: Instant,
    v: [(&'static str, f32); 12],
    n: usize,
}
impl Phases {
    fn new(on: bool, t0: Instant) -> Self {
        Phases { on, last: t0, v: [("", 0.0); 12], n: 0 }
    }
    fn lap(&mut self, what: &'static str) {
        let now = Instant::now();
        if self.n < self.v.len() {
            self.v[self.n] = (what, now.duration_since(self.last).as_secs_f32() * 1000.0);
            self.n += 1;
        }
        self.last = now;
    }
    fn list(&self) -> String {
        self.v[..self.n].iter().map(|(w, ms)| format!("{w} {ms:.1}")).collect::<Vec<_>>().join(" ")
    }
    fn slowest(&self) -> (&'static str, f32) {
        self.v[..self.n].iter().copied().fold(("-", 0.0), |a, b| if b.1 > a.1 { b } else { a })
    }
    fn log(&self, seq: u64, gap: f64, sample_ms: f32, visible: bool) {
        if !self.on {
            return;
        }
        crate::osutil::log(&format!("tick: {seq} at {:.3} s gap {:.3} s sample {sample_ms:.1} ms vis={} | {}", crate::timing::ms() / 1000.0, gap, visible as u8, self.list()));
    }
}

/// CPU time the sampler thread has used, ms.
fn own_cpu_ms() -> f64 {
    crate::timing::thread_cpu_ms(unsafe { windows::Win32::System::Threading::GetCurrentThread() })
}

/// Time `f` as a startup milestone while `on` (first ticks of a launch).
fn tm<R>(on: bool, what: impl FnOnce() -> String, f: impl FnOnce() -> R) -> R {
    if on {
        crate::timing::time(&what(), f)
    } else {
        f()
    }
}

fn run(shared: Arc<Shared>) {
    crate::timing::mark("sampler thread start");
    // probes: vitals.ini (one tiny file read; off unless the user turned them on)
    probe::init();
    let st = Arc::new(crate::timing::time("read_static (cpu name, smbios)", sys::read_static));
    let n = st.ncpu.max(1);
    let mut nbuf: Vec<u64> = vec![0; 64 * 1024];
    let mut cbuf: Vec<u64> = Vec::new();
    let mut last_cpu = nt::cpu_times(n, &mut cbuf);
    let mut prev: HashMap<(u32, i64), Prev> = HashMap::new();
    let mut last_proc_t: Option<Instant> = None;
    let mut metas = meta::MetaCache::default();
    let mut disks = crate::timing::time("disk enumerate", disk::enumerate);
    let mut net = net::NetState::default();
    let mut gpus = crate::timing::time("gpu load (D3DKMT)", gpu::Gpus::load);
    let mut gpu_primed = false;
    let mut gpu_h: Vec<VecDeque<f32>> = Vec::new();
    let mut cpu_h = VecDeque::new();
    let mut mem_h = VecDeque::new();
    let mut disk_h: HashMap<u32, VecDeque<f32>> = HashMap::new();
    let mut nic_h: HashMap<u64, VecDeque<f32>> = HashMap::new();
    // Vitals 60 s rings (bounded: see `ring`)
    let mut core_r = ring::Rings::new(n);
    let mut commit_r = ring::Ring::default();
    let mut disk_r: HashMap<u32, DiskRings> = HashMap::new();
    let mut nic_r: HashMap<u64, (ring::Ring, ring::Ring)> = HashMap::new();
    let mut gpu_r: Vec<GpuRings> = Vec::new();
    let addr_cell: crate::bgio::Cell<Vec<net::Addr>> = crate::bgio::Cell::default();
    let conn_cell: crate::bgio::Cell<net::Conns> = crate::bgio::Cell::default();
    let health_cell: crate::bgio::Cell<nethealth::Raw> = crate::bgio::Cell::default();
    let mut health_hist = nethealth::Hist::default();
    // per-process network (ETW) clock and PID -> create time scratch map
    let mono0 = Instant::now();
    let mut net_ids: HashMap<u32, i64> = HashMap::new();
    let mut net_hist = crate::netscope::ScopeHist::default();
    let mut seq = 0u64;
    let mut quick = false;
    // a visible tick with real deltas was committed (startup / after showing: tick fast until then)
    let mut was_ready = false;
    let mut fast_left = 10u32; // cap on 200 ms ticks per show, should a delta never become valid
    let mut last_tick: Option<Instant> = None;
    let cpu_cell: crate::bgio::Cell<crate::sensors::CpuRead> = crate::bgio::Cell::default();
    let clock_cell: crate::bgio::Cell<hw::CpuClock> = crate::bgio::Cell::default();
    let mut leaks = leak::Tracker::default();
    let mut last_leak: Option<Instant> = None;
    // charts and the 1 h history advance once per second of real time, also
    // while fast (200 ms) ticks run: those only refresh the numbers. (Every
    // fast tick used to add a chart sample, so after a stall the charts ran
    // at 5x speed for a few seconds.)
    let mut last_chart: Option<Instant> = None;
    // The sampler is what makes the window show anything new: on a busy
    // machine it must not queue behind normal-priority work (Task Manager
    // runs at high priority for the same reason). It uses ~10 ms of CPU a
    // second, so above-normal costs other programs nothing measurable.
    unsafe {
        use windows::Win32::System::Threading::*;
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }

    // hidden: GPU/NIC are only needed for the History tab (and the tray tooltip),
    // so refresh them every 3rd tick, and "light" (idle GPU engines and
    // NIC re-enumeration skipped most of the time, see gpu.rs / net.rs)
    let mut last_gpu: Vec<gpu::Gpu> = Vec::new();
    let mut last_nic: Vec<net::Nic> = Vec::new();
    let mut proc_gpu: Option<gpuproc::ProcGpu> = None;
    let mut proc_gpu_failed = false;
    loop {
        let t0 = Instant::now();
        let visible = shared.visible.load(Ordering::Relaxed);
        if visible {
            // this tick serves the show request: a stale wake must not trigger
            // an immediate second tick with ~0 ms deltas
            *shared.wake.0.lock().unwrap() = false;
        } else {
            was_ready = false;
            fast_left = 10;
        }
        let tick_dt = last_tick.map(|t| t0.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        last_tick = Some(t0);
        seq += 1;
        // startup timing: the first few ticks, until real data is on screen
        let tl = seq <= 4 && crate::timing::active();
        let mut ph = Phases::new(crate::timing::detail(), t0);
        let cpu0 = own_cpu_ms();
        if tl {
            crate::timing::mark(&format!("tick{seq} start (visible={visible})"));
        }

        // CPU per logical processor
        let now_cpu = nt::cpu_times(n, &mut cbuf);
        let cores: Vec<f32> = now_cpu
            .iter()
            .zip(&last_cpu)
            .map(|(a, b)| {
                let total = (a.kernel - b.kernel) + (a.user - b.user);
                let idle = a.idle - b.idle;
                if total <= 0 {
                    0.0
                } else {
                    ((total - idle) as f32 / total as f32 * 100.0).clamp(0.0, 100.0)
                }
            })
            .collect();
        last_cpu = now_cpu;
        let cpu = cores.iter().sum::<f32>() / n as f32;
        let chart = last_chart.is_none_or(|t| t0.duration_since(t) >= Duration::from_millis(900));
        if chart {
            last_chart = Some(t0);
            push(&mut cpu_h, cpu);
            core_r.push(&cores);
        }
        let sel = if visible { VIT_SEL.load(Ordering::Relaxed) } else { 0 };
        // CPU package temperature: whatever the I/O worker last read (never waits)
        let cpu_r = cpu_cell.read();
        let cpu_temp = match cpu_r.val {
            Some(crate::sensors::CpuRead::Ok(t)) => Some(t),
            _ => None,
        };
        // current CPU clock: only a reading from the last few seconds counts
        let cpu_clock = {
            let r = clock_cell.read();
            r.val.filter(|_| !r.stale && r.age.is_some_and(|a| a < Duration::from_secs(4)))
        };
        ph.lap("cpu");

        let mem = tm(tl, || format!("tick{seq} memory"), sys::mem);
        ph.lap("mem");

        if chart {
            push(&mut mem_h, (mem.total - mem.avail) as f32 / (1u64 << 30) as f32);
            commit_r.push(mem.commit as f32 / (1u64 << 30) as f32);
        }
        // one NtQuerySystemInformation (~10 us), only while Memory is on screen
        let lists = if sel == VIT_MEM { sys::mem_lists() } else { None };

        // the NVMe temperature query costs ~35 ms for all drives: slow-moving, so every 15 s and only while shown
        tm(tl, || format!("tick{seq} disks"), || disk::update(&mut disks));
        ph.lap("disks");
        let disk_parts: Vec<Part> = disks
            .iter()
            .map(|d| {
                let t = d.temp.read();
                let h = disk_h.entry(d.num).or_default();
                let mbps = (d.read_bps + d.write_bps) / 1e6;
                if chart {
                    push(h, mbps as f32);
                    let r = disk_r.entry(d.num).or_default();
                    r.read.push((d.read_bps / 1e6) as f32);
                    r.write.push((d.write_bps / 1e6) as f32);
                    r.act.push(d.active);
                    r.resp.push(d.resp_ms.unwrap_or(0.0));
                    r.qlen.push(d.queue as f32);
                }
                let name = if d.letters.is_empty() { format!("Disk {}", d.num) } else { d.letters.clone() };
                Part {
                    title: format!("{name} · {}", if d.model.is_empty() { "Disk" } else { &d.model }),
                    value: mbps,
                    unit: "MB/s",
                    max: h.iter().copied().fold(10.0, f32::max),
                    sub: format!(
                        "{} · {:.0}% busy · R {:.1} W {:.1} MB/s · {}",
                        d.bus,
                        d.active,
                        d.read_bps / 1e6,
                        d.write_bps / 1e6,
                        fmt_size(d.size)
                    ),
                    temp: t.val.map(|t| t.c),
                    temp_stale: t.stale,
                    hist: hist_vec(h),
                }
            })
            .collect();

        let gpu_ready = gpu_primed;
        if visible || seq % 3 == 0 || last_gpu.is_empty() {
            last_gpu = tm(tl, || format!("tick{seq} gpu (D3DKMT)"), || gpus.sample(!visible));
            gpu_primed = true;
        }
        ph.lap("gpu");
        let gpu_parts: Vec<Part> = last_gpu
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, g)| {
                if gpu_h.len() <= i {
                    gpu_h.push(VecDeque::new());
                }
                if chart {
                    push(&mut gpu_h[i], g.util);
                    if gpu_r.len() <= i {
                        gpu_r.push(GpuRings::default());
                    }
                    let r = &mut gpu_r[i];
                    let opt = |v: Option<f32>| v.unwrap_or(f32::NAN);
                    r.vram.push(if g.vram_total > 0 { g.vram_used as f32 / g.vram_total as f32 * 100.0 } else { f32::NAN });
                    r.clock.push(opt(g.clock_mhz.map(|c| c as f32)));
                    // engine names come with full samples only; light (hidden) ones leave a gap
                    if !g.engines.is_empty() && (r.names.len() != g.engines.len() || r.names.iter().zip(&g.engines).any(|(a, b)| *a != b.0)) {
                        r.names = g.engines.iter().map(|e| e.0.clone()).collect();
                        r.eng = ring::Rings::new(r.names.len());
                    }
                    let col: Vec<f32> = g.engines.iter().map(|e| e.1).collect();
                    r.eng.push(&col);
                }
                let mut sub = format!("VRAM {:.1}/{:.0} GB", g.vram_used as f64 / (1u64 << 30) as f64, g.vram_total as f64 / (1u64 << 30) as f64);
                if let Some(c) = g.clock_mhz {
                    sub += &format!(" · {:.2} GHz", c as f32 / 1000.0);
                }
                if let Some(p) = g.power_pct {
                    sub += &format!(" · power {:.0}%", p);
                }
                Part { title: format!("GPU · {}", g.name), value: g.util as f64, unit: "%", max: 100.0, sub, temp: g.temp, temp_stale: false, hist: hist_vec(&gpu_h[i]) }
            })
            .collect();

        if visible || seq % 3 == 0 || last_nic.is_empty() {
            last_nic = tm(tl, || format!("tick{seq} nics"), || net.sample(!visible));
            // link flaps come from the list just read: no extra API call, also while hidden
            health_hist.observe_links(last_nic.iter().map(|c| (c.luid, c.up)));
        }
        ph.lap("nic");
        // only connected adapters: unplugged / disabled NICs are noise on Vitals
        let nic_parts: Vec<Part> = last_nic
            .iter()
            .filter(|c| c.up)
            .cloned()
            .map(|c| {
                let h = nic_h.entry(c.luid).or_default();
                let mbps = (c.rx_bps + c.tx_bps) / 1e6;
                if chart {
                    push(h, mbps as f32);
                    let r = nic_r.entry(c.luid).or_default();
                    r.0.push((c.rx_bps / 1e6) as f32);
                    r.1.push((c.tx_bps / 1e6) as f32);
                }
                Part {
                    title: c.name.clone(),
                    value: mbps,
                    unit: "Mb/s",
                    max: h.iter().copied().fold(1.0, f32::max),
                    sub: if c.up {
                        format!("↓ {:.1}  ↑ {:.1} Mb/s · {} link", c.rx_bps / 1e6, c.tx_bps / 1e6, fmt_link(c.link_bps))
                    } else {
                        format!("not connected · {}", c.desc)
                    },
                    temp: None,
                    temp_stale: false,
                    hist: hist_vec(h),
                }
            })
            .collect();

        // 1 h history for the History tab
        if chart {
            use crate::ui::{C_CPU, C_DSK, C_GPU, C_MEM, C_NET};
            let gb = |b: u64| b as f32 / (1u64 << 30) as f32;
            history::record("cpu", "CPU", "%", C_CPU, Some(100.0), cpu);
            history::record("mem", "Memory in use", "GB", C_MEM, Some(gb(mem.total)), gb(mem.total - mem.avail));
            let cp = if mem.commit_limit > 0 { mem.commit as f32 / mem.commit_limit as f32 * 100.0 } else { 0.0 };
            if let Some(t) = cpu_temp.filter(|_| !cpu_r.stale) {
                history::record("cput", "CPU temperature", "°C", C_CPU, None, t);
            } else {
                history::temperature_gap("cput");
            }
            history::record("commit", "Commit charge", "%", C_MEM, Some(100.0), cp);
            for (i, g) in gpu_parts.iter().enumerate() {
                history::record(&format!("gpu{i}"), &g.title, "%", C_GPU, Some(100.0), g.value as f32);
                if let Some(t) = g.temp {
                    history::record(&format!("gput{i}"), &format!("{} · temperature", g.title), "°C", C_GPU, None, t);
                } else {
                    history::temperature_gap(&format!("gput{i}"));
                }
            }
            for (i, d) in disk_parts.iter().enumerate() {
                history::record(&format!("disk{i}"), &d.title, "MB/s", C_DSK, None, d.value as f32);
            }
            for (i, n) in nic_parts.iter().enumerate() {
                history::record(&format!("nic{i}"), &n.title, "Mb/s", C_NET, None, n.value as f32);
            }
        }

        ph.lap("hist");
        // processes: only while visible
        let mut nbuf_fresh = false;
        let mut procs = Vec::new();
        let (mut threads, mut handles) = (0u32, 0u32);
        let old = shared.get();
        let mut nprocs = old.nprocs;
        let mut proc_dt = 0.0f64;
        let mut compressed = None;
        // the very first tick primes the per-process baseline even before the window
        // is shown, so the first shown tick already has real per-process CPU
        if visible || seq == 1 {
            let now = Instant::now();
            let dt = last_proc_t.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
            proc_dt = dt;
            last_proc_t = Some(now);
            if tm(tl, || format!("tick{seq} process query"), || nt::query(nt::SYSTEM_PROCESS_INFORMATION, &mut nbuf)) {
                nbuf_fresh = true;
                let pt0 = Instant::now();
                ph.lap("procq");
                metas.poll();
                let mut misses: Vec<(u32, i64)> = Vec::new();
                let windowed = if visible { window_pids() } else { HashSet::new() };
                ph.lap("wins");
                let mut alive = HashSet::new();
                let mut next_prev = HashMap::with_capacity(prev.len());
                let base = nbuf.as_ptr() as *const u8;
                let mut off = 0usize;
                loop {
                    let p = unsafe { &*(base.add(off) as *const nt::Spi) };
                    let pid = p.pid as u32;
                    let name: Arc<str> = if pid == 0 {
                        "System Idle Process".into()
                    } else if p.name.buf.is_null() {
                        "System".into()
                    } else {
                        let s = unsafe { std::slice::from_raw_parts(p.name.buf, p.name.len as usize / 2) };
                        String::from_utf16_lossy(s).into()
                    };
                    let key = (pid, p.create);
                    let cpu_t = p.user + p.kernel;
                    let io_t = p.read_bytes + p.write_bytes + p.other_bytes;
                    let (cpu, io) = match prev.get(&key) {
                        Some(pv) if dt > 0.0 => (
                            ((cpu_t - pv.cpu) as f64 / (dt * 1e7 * n as f64) * 100.0) as f32,
                            (io_t - pv.io).max(0) as f64 / dt,
                        ),
                        _ => (0.0, 0.0),
                    };
                    next_prev.insert(key, Prev { cpu: cpu_t, io: io_t });
                    threads += p.nthreads;
                    handles += p.handles;
                    alive.insert(key);
                    if visible && compressed.is_none() && &*name == "Memory Compression" {
                        compressed = Some(p.ws as u64);
                    }
                    if pid != 0 && visible {
                        procs.push(Proc {
                            pid,
                            ppid: p.ppid as u32,
                            name,
                            create: p.create,
                            cpu: cpu.clamp(0.0, 100.0),
                            mem: p.ws_private as u64,
                            io_bps: io,
                            threads: p.nthreads,
                            handles: p.handles,
                            session: p.session,
                            windowed: windowed.contains(&pid),
                            meta: match metas.lookup(pid, p.create) {
                                Some(m) => m,
                                None => {
                                    misses.push(key);
                                    metas.placeholder()
                                }
                            },
                            gpu: 0.0,
                            gpu_engine: None,
                        });
                    }
                    if p.next == 0 {
                        break;
                    }
                    off += p.next as usize;
                }
                prev = next_prev;
                // details of new processes come from the I/O worker (placeholders until then)
                metas.fetch(misses);
                metas.retain(&alive);
                if visible {
                    nprocs = procs.len();
                }
                if tl {
                    crate::timing::mark(&format!("tick{seq} process walk+meta ({:.1} ms, {nprocs} procs)", pt0.elapsed().as_secs_f32() * 1000.0));
                }
            }
        } else {
            last_proc_t = None;
            prev.clear();
        }
        ph.lap("procs");
        // per-process GPU: only while the Processes tab shows a GPU column
        if visible && PROC_GPU.load(Ordering::Relaxed) {
            if proc_gpu.is_none() && !proc_gpu_failed {
                proc_gpu = gpuproc::ProcGpu::open(gpus.engine_labels());
                proc_gpu_failed = proc_gpu.is_none();
            }
            if let (Some(pg), true) = (proc_gpu.as_mut(), nbuf_fresh) {
                pg.sample(&mut procs);
            }
        } else {
            proc_gpu = None;
        }

        ph.lap("procgpu");
        // memory leaks / pressure: every 30 s, shown or hidden
        if last_leak.map(|t| t0.duration_since(t).as_secs() >= leak::step_s()).unwrap_or(true) {
            last_leak = Some(t0);
            if nbuf_fresh || nt::query(nt::SYSTEM_PROCESS_INFORMATION, &mut nbuf) {
                let samples = commit_samples(&nbuf);
                let m = leak::MemInfo { total: mem.total, avail: mem.avail, commit: mem.commit, commit_limit: mem.commit_limit };
                for a in leaks.feed(&samples, m, Instant::now()) {
                    leak_toast(a);
                }
            }
        }

        ph.lap("leak");
        // rings of vanished disks / adapters
        if chart && seq.is_multiple_of(60) {
            disk_r.retain(|k, _| disks.iter().any(|d| d.num == *k));
            disk_h.retain(|k, _| disks.iter().any(|d| d.num == *k));
            nic_r.retain(|k, _| last_nic.iter().any(|c| c.luid == *k));
            nic_h.retain(|k, _| last_nic.iter().any(|c| c.luid == *k));
        }
        // per-process traffic (ETW session only while Vitals > Network is the visible detail)
        let net_top = {
            let want = visible && sel == VIT_NET;
            let mut c = crate::netetw::global().lock().unwrap();
            if c.wants_ids(want) {
                net_ids.clear();
                net_ids.extend(procs.iter().map(|p| (p.pid, p.create)));
            }
            let now_ms = t0.duration_since(mono0).as_millis() as u64;
            let feed = c.tick(now_ms, want, &net_ids);
            #[cfg(debug_assertions)]
            let feed = crate::netscope::synthetic(now_ms, feed, &net_ids);
            // the scope history follows sampler time (same clock as the interval ends), not paints
            net_hist.observe(now_ms, &feed);
            feed
        };
        ph.lap("net top");
        let vit = if visible {
            let disks_v = disks
                .iter()
                .map(|d| {
                    let r = disk_r.entry(d.num).or_default();
                    VDisk {
                        num: d.num,
                        model: d.model.clone(),
                        bus: d.bus,
                        rotational: d.rotational,
                        firmware: d.firmware.clone(),
                        letters: d.letters.clone(),
                        size: d.size,
                        read_bps: d.read_bps,
                        write_bps: d.write_bps,
                        active: d.active,
                        resp_ms: d.resp_ms,
                        queue: d.queue,
                        system: d.system,
                        pagefile: d.pagefile,
                        temp: d.temp.read().val.map(|t| t.c),
                        vols: if sel == VIT_DISK { d.vols.read().val.unwrap_or_default() } else { Vec::new() },
                        read: r.read.to_vec(),
                        write: r.write.to_vec(),
                        act: r.act.to_vec(),
                        resp: r.resp.to_vec(),
                        qlen: r.qlen.to_vec(),
                    }
                })
                .collect();
            let nics_v = last_nic
                .iter()
                .filter(|c| c.up)
                .map(|c| {
                    let r = nic_r.entry(c.luid).or_default();
                    VNic {
                        luid: c.luid,
                        name: c.name.clone(),
                        desc: c.desc.clone(),
                        kind: c.kind,
                        link_bps: c.link_bps,
                        rx_bps: c.rx_bps,
                        tx_bps: c.tx_bps,
                        rx_total: c.rx_total,
                        tx_total: c.tx_total,
                        rx: r.0.to_vec(),
                        tx: r.1.to_vec(),
                    }
                })
                .collect();
            let gpus_v = (0..last_gpu.len())
                .map(|i| {
                    let util = gpu_h.get(i).map(|h| {
                        let mut v = vec![f32::NAN; HIST - h.len()];
                        v.extend(h.iter().copied());
                        v
                    });
                    match gpu_r.get(i) {
                        Some(r) => VGpu {
                            util: util.unwrap_or_default(),
                            vram: r.vram.to_vec(),
                            clock: r.clock.to_vec(),
                            names: r.names.clone(),
                            eng: r.eng.to_vec(),
                        },
                        None => VGpu { util: util.unwrap_or_default(), vram: vec![], clock: vec![], names: vec![], eng: vec![] },
                    }
                })
                .collect();
            Vit {
                cores: core_r.to_vec(),
                commit: commit_r.to_vec(),
                lists,
                compressed,
                disks: disks_v,
                nics: nics_v,
                nics_hidden: last_nic.iter().filter(|c| !c.up && c.kind != "virtual").count(),
                gpus: gpus_v,
                addrs: if sel == VIT_NET { addr_cell.read().val.unwrap_or_default() } else { Vec::new() },
                conns: if sel == VIT_NET { conn_cell.read().val } else { None },
                health: if sel == VIT_NET { health_hist.health() } else { Default::default() },
                probe: if sel == VIT_NET && probe::enabled() { probe::view(VIT_NIC.load(Ordering::Relaxed)) } else { Default::default() },
                net_top,
                net_scope: if sel == VIT_NET { net_hist.series() } else { Default::default() },
            }
        } else {
            Vit::default()
        };
        let snap = Snapshot {
            seq,
            st: st.clone(),
            cpu,
            cores,
            cpu_hist: hist_vec(&cpu_h),
            mem,
            mem_hist: hist_vec(&mem_h),
            gpus: gpu_parts,
            disks: disk_parts,
            nics: nic_parts,
            nprocs,
            procs,
            threads: if visible { threads } else { old.threads },
            handles: if visible { handles } else { old.handles },
            uptime: sys::uptime_s(),
            sample_ms: t0.elapsed().as_secs_f32() * 1000.0,
            cpu_clock,
            cpu_temp,
            cpu_read: cpu_r.val,
            cpu_temp_stale: cpu_r.stale,
            gpu_raw: last_gpu.clone(),
            ready: visible && last_proc_t.is_some() && proc_dt >= 0.1 && tick_dt >= 0.1 && gpu_ready,
            vit,
        };
        let ready = snap.ready;
        let sample_ms = snap.sample_ms;
        crate::timing::CTX_CPU10.store((cpu * 10.0) as u32, Ordering::Relaxed);
        if visible {
            crate::timing::CTX_PROCS.store(snap.nprocs as u32, Ordering::Relaxed);
        }
        *shared.snap.lock().unwrap() = Arc::new(snap);
        if tl {
            crate::timing::mark(&format!("tick{seq} commit (ready={ready}, {:.1} ms)", t0.elapsed().as_secs_f32() * 1000.0));
        }
        // shown: full data message each tick; hidden: a tray-only tick every 2nd second
        if visible || seq % 2 == 0 {
            let h = shared.hwnd.load(Ordering::Relaxed);
            if h != 0 {
                if visible {
                    post_coalesced(h, WM_APP_DATA, &DATA_POSTED);
                } else {
                    post_coalesced(h, WM_APP_TRAYTICK, &TRAY_POSTED);
                }
            }
        }

        // Reads that ask hardware or drivers run on the I/O worker; here they are
        // only requested (deduplicated, never waited for). Their values show from
        // the next tick on.
        crate::bgio::watchdog();
        crate::timing::ui_watchdog();
        // CPU package temperature: every second on the Vitals CPU detail, every 2nd while
        // shown, every 5th hidden (tray tooltip); without the driver or admin rights
        // only rarely
        {
            use crate::sensors::CpuRead;
            let every = match cpu_r.val {
                None | Some(CpuRead::Ok(_)) if !visible => 5,
                None | Some(CpuRead::Ok(_)) if SENSORS_TAB.load(Ordering::Relaxed) => 1,
                None | Some(CpuRead::Ok(_)) => 2,
                Some(CpuRead::Failed) => 5,
                Some(_) => 30,
            };
            // a little slack: the last read finished a few ms after the previous tick
            cpu_cell.refresh("cpu temperature", Duration::from_millis(every * 1000 - 300), || Some(crate::sensors::cpu_temp()));
        }
        // drive temperatures: slow-moving, so every 15 s and only while shown
        if visible && ready {
            disk::request_temps(&disks, Duration::from_millis(14_700));
        }
        // Vitals detail reads for the selected device, on the worker
        match sel {
            VIT_CPU => {
                clock_cell.refresh("cpu clock", Duration::from_millis(900), hw::clock_now);
            }
            VIT_DISK => disk::request_volumes(&disks, Duration::from_millis(4_700)),
            VIT_NET => {
                addr_cell.refresh("adapter addresses", Duration::from_millis(9_700), net::addresses);
                conn_cell.refresh("socket counts", Duration::from_millis(1_900), net::connections);
                // passive health counters: one read a second, only on this detail
                let up: Vec<&net::Nic> = last_nic.iter().filter(|c| c.up).collect();
                let luids: Vec<u64> = up.iter().map(|c| c.luid).collect();
                let wireless: Vec<u64> = up.iter().filter(|c| c.kind == "wireless").map(|c| c.luid).collect();
                health_cell.refresh("net health", Duration::from_millis(900), move || nethealth::read(&luids, &wireless));
                if let Some(raw) = health_cell.get() {
                    health_hist.feed(raw);
                }
                // opt-in probes: lease the probe thread for this adapter while this detail is shown
                // (`sel` is 0 whenever the window is hidden), nothing otherwise
                let want = if probe::enabled() && !MINIMIZED.load(Ordering::Relaxed) {
                    let luid = VIT_NIC.load(Ordering::Relaxed);
                    let nic = last_nic.iter().find(|c| c.luid == luid && c.up);
                    let st = probe::settings();
                    // the adapter the OS would send the internet target through (a VPN or another NIC may own the route)
                    let routed = nic.and_then(|_| st.internet).and_then(probe::route_luid);
                    nic.and_then(|n| addr_cell.get().unwrap_or_default().into_iter().find(|a| a.luid == luid).map(|a| (n.kind, a))).and_then(|(kind, a)| probe::cfg_for(luid, &a.ipv4, &a.gateway, &a.dns, &st, kind, routed))
                } else {
                    None
                };
                match want {
                    Some(cfg) => probe::lease(cfg),
                    None => probe::release(),
                }
            }
            _ => {
                health_hist.idle();
                probe::release();
            }
        }
        ph.lap("post");
        ph.log(seq, tick_dt, sample_ms, visible);
        let body = t0.elapsed();
        let body_cpu = own_cpu_ms() - cpu0;

        // wait for next tick, or wake early when the window is shown;
        // until the first real numbers are on screen, tick again soon
        // (also while a bulk process-details lookup runs, so descriptions show up soon)
        let fast = visible && fast_left > 0 && ((!ready && !was_ready) || metas.busy());
        fast_left -= fast as u32;
        let wait = if (quick && !ready) || fast { Duration::from_millis(200) } else { Duration::from_secs(1) };
        was_ready |= ready;
        quick = false;
        let (m, c) = &shared.wake;
        let g = m.lock().unwrap();
        let (mut g, _) = c.wait_timeout_while(g, wait.saturating_sub(t0.elapsed()), |woken| !*woken).unwrap();
        let period = t0.elapsed();
        // (minutes: the machine slept, not a stall)
        if !*g && period > Duration::from_millis(1500) && period < Duration::from_secs(60) {
            // the timed wait ends at t0 + wait (at once when the body ran longer)
            let late = period.saturating_sub(wait.max(body));
            let (sp, sp_ms) = ph.slowest();
            let body_ms = body.as_secs_f32() * 1000.0;
            let why = if late > Duration::from_millis(300) {
                " (sampler thread not scheduled: CPU starved)"
            } else if body_ms > 500.0 && (body_cpu as f32) < body_ms * 0.3 {
                " (body mostly waiting: a blocked call, or starved)"
            } else {
                ""
            };
            crate::timing::stall(crate::timing::Stall::Sampler, period.as_secs_f32() * 1000.0, || {
                format!(
                    "sampler: tick {seq} came round after {:.2} s (planned {} ms): body {body_ms:.0} ms (sampler CPU {body_cpu:.0} ms; slowest {sp} {sp_ms:.0} ms), wait woke {} ms late{why} | {}",
                    period.as_secs_f32(),
                    wait.as_millis(),
                    late.as_millis(),
                    ph.list()
                )
            });
        }
        if *g {
            *g = false;
            quick = true; // first full sample has no CPU deltas; take the next one soon
            drop(g);
            // shown right after a tick: leave >= 150 ms so the deltas mean something
            let since = t0.elapsed();
            if since < Duration::from_millis(150) {
                std::thread::sleep(Duration::from_millis(150) - since);
            }
        }
    }
}

/// Private commit of every process from a SystemProcessInformation buffer.
fn commit_samples(nbuf: &[u64]) -> Vec<leak::Sample> {
    let mut v = Vec::new();
    let base = nbuf.as_ptr() as *const u8;
    let mut off = 0usize;
    loop {
        let p = unsafe { &*(base.add(off) as *const nt::Spi) };
        if p.pid != 0 && !p.name.buf.is_null() {
            let s = unsafe { std::slice::from_raw_parts(p.name.buf, p.name.len as usize / 2) };
            v.push(leak::Sample { pid: p.pid as u32, create: p.create, name: String::from_utf16_lossy(s), commit: p.private_pages as u64 });
        }
        if p.next == 0 {
            break;
        }
        off += p.next as usize;
    }
    v
}

fn fmt_mb(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
}

/// Turn a leak/pressure verdict into a Watch event + toast.
fn leak_toast(a: leak::Alert) {
    let Some(f) = crate::features::get() else { return };
    match a {
        leak::Alert::Leak(s) => {
            let title = format!("Memory leak? {}", s.name);
            let text = format!(
                "{} now, up from {} in {} min (+{}/h). Restart it if it keeps growing.",
                fmt_mb(s.now_mb as f64),
                fmt_mb(s.start_mb as f64),
                s.minutes,
                fmt_mb(s.rate_mb_h as f64)
            );
            f.watch.memory_alert(true, &s.name, &title, &text);
        }
        leak::Alert::Pressure { free_mb, commit_pct, top } => {
            let top = top.iter().map(|(n, mb)| format!("{} {}", n, fmt_mb(*mb as f64))).collect::<Vec<_>>().join(", ");
            let text = format!("{} free, commit {}%. Biggest: {}", fmt_mb(free_mb as f64), commit_pct, top);
            f.watch.memory_alert(false, "Memory", "Memory almost full", &text);
        }
    }
}

fn window_pids() -> HashSet<u32> {
    let mut set: HashSet<u32> = HashSet::new();
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        let set = &mut *(l.0 as *mut HashSet<u32>);
        if IsWindowVisible(h).as_bool() && GetWindow(h, GW_OWNER).map(|o| o.is_invalid()).unwrap_or(true) {
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            // InternalGetWindowText reads the title win32k keeps: it never
            // sends WM_GETTEXTLENGTH, which waits for the window's thread
            // (seconds, on a busy machine or with one hung window)
            let mut t = [0u16; 2];
            if ex & WS_EX_TOOLWINDOW.0 == 0 && InternalGetWindowText(h, &mut t) > 0 {
                let mut cloaked = 0u32;
                let _ = windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
                    h,
                    windows::Win32::Graphics::Dwm::DWMWA_CLOAKED,
                    &mut cloaked as *mut u32 as *mut _,
                    4,
                );
                if cloaked == 0 {
                    let mut pid = 0u32;
                    GetWindowThreadProcessId(h, Some(&mut pid));
                    set.insert(pid);
                }
            }
        }
        TRUE
    }
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut set as *mut _ as isize));
    }
    set
}

pub fn fmt_size(b: u64) -> String {
    let g = b as f64 / 1e9;
    if g >= 1000.0 {
        format!("{:.1} TB", g / 1000.0)
    } else {
        format!("{:.0} GB", g)
    }
}
fn fmt_link(bps: u64) -> String {
    if bps >= 1_000_000_000 {
        format!("{:.1} Gb/s", bps as f64 / 1e9).replace(".0 ", " ")
    } else {
        format!("{} Mb/s", bps / 1_000_000)
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

    fn thread_cpu_us() -> f64 {
        let (mut a, mut b, mut k, mut u) = Default::default();
        unsafe {
            let _ = GetThreadTimes(GetCurrentThread(), &mut a, &mut b, &mut k, &mut u);
        }
        let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) as f64 / 10.0;
        f(k) + f(u)
    }

    fn run(what: &str, n: usize, mut f: impl FnMut()) {
        f();
        let c0 = thread_cpu_us();
        let w0 = Instant::now();
        for _ in 0..n {
            f();
        }
        let cpu = (thread_cpu_us() - c0) / n as f64;
        let wall = w0.elapsed().as_secs_f64() * 1e6 / n as f64;
        eprintln!("{what:<32} cpu {cpu:>8.1} us  wall {wall:>8.1} us");
    }

    /// Per-step cost of a hidden tick: `cargo test hidden_tick_steps -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn hidden_tick_steps() {
        let n = 200;
        let mut cbuf = Vec::new();
        let mut nbuf: Vec<u64> = vec![0; 64 * 1024];
        nt::query(nt::SYSTEM_PROCESS_INFORMATION, &mut nbuf);
        eprintln!("procs: {}", commit_samples(&nbuf).len());
        run("cpu_times", n, || {
            std::hint::black_box(nt::cpu_times(32, &mut cbuf));
        });
        run("GetPerformanceInfo (sys::mem)", n, || {
            std::hint::black_box(sys::mem());
        });
        run("GlobalMemoryStatusEx", n, || {
            let mut m = windows::Win32::System::SystemInformation::MEMORYSTATUSEX { dwLength: 64, ..Default::default() };
            unsafe {
                let _ = windows::Win32::System::SystemInformation::GlobalMemoryStatusEx(&mut m);
            }
            std::hint::black_box(m);
        });
        run("NtQSI(SystemProcessInformation)", n, || {
            std::hint::black_box(nt::query(nt::SYSTEM_PROCESS_INFORMATION, &mut nbuf));
        });
        let mut bbuf: Vec<u64> = vec![0; 8 * 1024];
        let ok = nt::query(252, &mut bbuf);
        let mut cnt = 0;
        if ok {
            let base = bbuf.as_ptr() as *const u8;
            let mut off = 0usize;
            loop {
                let next = unsafe { *(base.add(off) as *const u32) };
                let pid = unsafe { *(base.add(off + 8) as *const usize) };
                let seq = unsafe { *(base.add(off + 24) as *const u64) };
                let len = unsafe { *(base.add(off + 32) as *const u16) };
                let nb = unsafe { *(base.add(off + 40) as *const *const u16) };
                if cnt < 5 && !nb.is_null() {
                    eprintln!("  {pid} seq {seq} {}", String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(nb, len as usize / 2) }));
                }
                cnt += 1;
                if next == 0 { break; }
                off += next as usize;
            }
        }
        eprintln!("class 252 ok={ok} entries={cnt}");
        run("NtQSI(252 basic process)", n, || {
            std::hint::black_box(nt::query(252, &mut bbuf));
        });
        run("commit_samples (leak walk)", n, || {
            std::hint::black_box(commit_samples(&nbuf));
        });
        let mut disks = disk::enumerate();
        run("disk::update", n, || disk::update(&mut disks));
        let mut gpus = gpu::Gpus::load();
        run("gpus.sample", n, || {
            std::hint::black_box(gpus.sample(false));
        });
        // light: let idle engines settle first, then steady-state cost (incl. every 10th full read)
        for _ in 0..3 {
            gpus.sample(true);
        }
        run("gpus.sample light (hidden)", n, || {
            std::hint::black_box(gpus.sample(true));
        });
        let mut net = net::NetState::default();
        run("net.sample (GetIfTable2)", n, || {
            std::hint::black_box(net.sample(false));
        });
        run("GetSystemTimes", n, || {
            let (mut a, mut b, mut c) = Default::default();
            unsafe {
                let _ = windows::Win32::System::Threading::GetSystemTimes(Some(&mut a), Some(&mut b), Some(&mut c));
            }
            std::hint::black_box((a, b, c));
        });
        run("net.sample light (hidden)", n, || {
            std::hint::black_box(net.sample(true));
        });
        let nics = net.sample(false);
        let luids: Vec<u64> = nics.iter().map(|n| n.luid).collect();
        eprintln!("nics kept: {}", luids.len());
        run("GetIfEntry2 x kept nics", n, || {
            use windows::Win32::NetworkManagement::IpHelper::*;
            for l in &luids {
                let mut r = MIB_IF_ROW2::default();
                r.InterfaceLuid.Value = *l;
                unsafe {
                    let _ = GetIfEntry2(&mut r);
                }
                std::hint::black_box(r.InOctets);
            }
        });
        gpus.bench_parts(n);
        run("uptime/history", n, || {
            history::record("cpu", "CPU", "%", 0, Some(100.0), 1.0);
        });
    }
}

