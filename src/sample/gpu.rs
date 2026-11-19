//! GPUs through D3DKMT, the kernel graphics statistics Task Manager uses.
//! Vendor neutral and needs no driver DLL (NVML alone cost ~20 MB of RAM).
//! Utilisation = busiest engine (node), like Task Manager's GPU %.

use std::sync::Arc;
use std::time::Instant;

use windows::Wdk::Graphics::Direct3D::*;
use windows::Win32::Foundation::LUID;

#[derive(Clone, Default)]
pub struct Gpu {
    pub name: String,
    pub util: f32,
    pub temp: Option<f32>,
    pub vram_used: u64,
    pub vram_total: u64,
    /// share of the board's max power, 0..100 (D3DKMT reports no watts)
    pub power_pct: Option<f32>,
    pub clock_mhz: Option<u32>,
    pub fan_rpm: Option<u32>,
    /// busiest node per engine group ("3D", "Copy", "Compute", ...), %;
    /// empty on a light sample (idle nodes were skipped)
    pub engines: Vec<(Arc<str>, f32)>,
    /// shared (system) memory resident / budget, bytes
    pub shared_used: u64,
    pub shared_total: u64,
    /// driver, location and model facts, read once at load
    pub facts: Arc<super::hw::GpuFacts>,
}

struct Adapter {
    h: u32,
    luid: LUID,
    name: String,
    nodes: u32,
    segments: u32,
    vram_total: u64,
    /// per node: running time and when it was read (nodes are not all read every sample)
    last_run: Vec<(i64, Option<Instant>)>,
    /// per node: samples in a row it was idle (light sampling skips idle nodes)
    idle: Vec<u8>,
    /// per node: "GPU <i> - <engine>" (index filled in after sorting)
    engines: Vec<Arc<str>>,
    /// engine groups (see `engine_group`) and each node's index into them
    groups: Vec<Arc<str>>,
    node_group: Vec<usize>,
    shared_total: u64,
    facts: Arc<super::hw::GpuFacts>,
}

/// Driver, WDDM model and PCI location of one adapter. Each query that fails
/// leaves its field empty; the registry's version/date are used only when the
/// adapter's PCI ids identify exactly one driver (see `hw::pick_driver`).
fn read_facts(h: u32, full_name: &str, entries: &[(String, String, String)]) -> super::hw::GpuFacts {
    use super::hw;
    let mut f = hw::GpuFacts { full_name: hw::clean_text(full_name.as_bytes(), 80), ..Default::default() };
    let mut ids = D3DKMT_QUERY_DEVICE_IDS::default();
    let mut driver = None;
    if query(h, KMTQAITYPE_PHYSICALADAPTERDEVICEIDS, &mut ids) && ids.DeviceIds.VendorID != 0 {
        let (v, d) = (ids.DeviceIds.VendorID, ids.DeviceIds.DeviceID);
        f.pci_id = Some(format!("{v:04X}:{d:04X}"));
        driver = hw::pick_driver(entries, v, d);
    }
    let mut umd = D3DKMT_UMD_DRIVER_VERSION::default();
    let umd = if query(h, KMTQAITYPE_UMD_DRIVER_VERSION, &mut umd) { hw::fmt_umd_version(umd.DriverVersion) } else { None };
    match driver {
        Some((v, date)) => {
            f.driver_version = Some(v);
            f.driver_date = date;
        }
        None => f.driver_version = umd,
    }
    let mut ver = D3DKMT_DRIVERVERSION(0);
    if query(h, KMTQAITYPE_DRIVERVERSION, &mut ver) {
        f.wddm = hw::wddm_name(ver.0);
    }
    let mut a = D3DKMT_ADAPTERADDRESS::default();
    if query(h, KMTQAITYPE_ADAPTERADDRESS, &mut a) {
        f.location = hw::fmt_location(a.BusNumber, a.DeviceNumber, a.FunctionNumber);
    }
    f
}

/// Engine group of a node's friendly name: "Compute_1" → "Compute",
/// "Video Decode" stays. Several nodes of one kind show as one engine.
pub fn engine_group(name: &str) -> &str {
    match name.rsplit_once('_') {
        Some((head, n)) if !head.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => name,
    }
}

/// Groups in display order: the usual Task Manager five first, then the
/// rest as found.
fn order_groups(names: &[&str]) -> (Vec<Arc<str>>, Vec<usize>) {
    const FIRST: [&str; 5] = ["3D", "Compute", "Copy", "Video Decode", "Video Encode"];
    let mut groups: Vec<&str> = Vec::new();
    for g in FIRST {
        if names.iter().any(|n| engine_group(n) == g) {
            groups.push(g);
        }
    }
    for n in names {
        let g = engine_group(n);
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    let idx = names.iter().map(|n| groups.iter().position(|g| *g == engine_group(n)).unwrap_or(0)).collect();
    (groups.into_iter().map(Arc::from).collect(), idx)
}

pub struct Gpus {
    adapters: Vec<Adapter>,
    /// light samples taken; every LIGHT_FULL-th one reads every node
    light_n: u32,
}

/// Light sampling (window hidden: tray tooltip and History) reads a node
/// only while it is busy: D3DKMTQueryStatistics costs 0.2-0.4 ms per node
/// (14-15 nodes on a GeForce, ~3-6 ms a sample), and most engines sit idle.
/// A node idle for IDLE_AFTER samples in a row is read again on every
/// LIGHT_FULL-th light sample (hidden GPU samples come every 3 s: 30 s), or
/// at once on a full sample. Its running time is cumulative, so the first
/// read after a skip still gives the right average over the gap.
const IDLE_AFTER: u8 = 2;
const LIGHT_FULL: u32 = 10;
/// below this a node counts as idle, %
const IDLE_PCT: f32 = 0.5;

fn query<T>(h: u32, ty: KMTQUERYADAPTERINFOTYPE, out: &mut T) -> bool {
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: h,
        Type: ty,
        pPrivateDriverData: out as *mut T as *mut _,
        PrivateDriverDataSize: std::mem::size_of::<T>() as u32,
    };
    unsafe { D3DKMTQueryAdapterInfo(&mut q).is_ok() }
}

fn stats(ty: D3DKMT_QUERYSTATISTICS_TYPE, luid: LUID, id: u32) -> Option<D3DKMT_QUERYSTATISTICS> {
    let mut s = D3DKMT_QUERYSTATISTICS::default();
    s.Type = ty;
    s.AdapterLuid = luid;
    // QuerySegment and QueryNode both start with the 32-bit id
    s.Anonymous.QuerySegment.SegmentId = id;
    unsafe { D3DKMTQueryStatistics(&s).is_ok().then_some(s) }
}

impl Gpus {
    pub fn load() -> Self {
        let mut adapters = Vec::new();
        let entries = super::hw::class_entries();
        unsafe {
            let mut e = D3DKMT_ENUMADAPTERS2::default();
            if D3DKMTEnumAdapters2(&mut e).is_err() || e.NumAdapters == 0 {
                return Gpus { adapters, light_n: 0 };
            }
            let mut list = vec![D3DKMT_ADAPTERINFO::default(); e.NumAdapters as usize];
            e.pAdapters = list.as_mut_ptr();
            if D3DKMTEnumAdapters2(&mut e).is_err() {
                return Gpus { adapters, light_n: 0 };
            }
            list.truncate(e.NumAdapters as usize);
            for a in list {
                let mut ty = D3DKMT_ADAPTERTYPE::default();
                if query(a.hAdapter, KMTQAITYPE_ADAPTERTYPE, &mut ty) {
                    let v = ty.Anonymous.Value;
                    let render = v & 1 != 0;
                    let software = v & (1 << 2) != 0;
                    if !render || software {
                        let _ = D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: a.hAdapter });
                        continue;
                    }
                }
                let mut reg = D3DKMT_ADAPTERREGISTRYINFO::default();
                query(a.hAdapter, KMTQAITYPE_ADAPTERREGISTRYINFO, &mut reg);
                let end = reg.AdapterString.iter().position(|&c| c == 0).unwrap_or(260);
                let full = String::from_utf16_lossy(&reg.AdapterString[..end]);
                let name = full.replace("NVIDIA GeForce ", "").replace("(R)", "").trim().to_string();
                let mut seg = D3DKMT_SEGMENTSIZEINFO::default();
                query(a.hAdapter, KMTQAITYPE_GETSEGMENTSIZE, &mut seg);
                let Some(s) = stats(D3DKMT_QUERYSTATISTICS_ADAPTER, a.AdapterLuid, 0) else { continue };
                let info = s.QueryResult.AdapterInformation;
                let engines = (0..info.NodeCount)
                    .map(|n| {
                        let mut md = D3DKMT_NODEMETADATA { NodeOrdinalAndAdapterIndex: n, ..Default::default() };
                        let name = if query(a.hAdapter, KMTQAITYPE_NODEMETADATA, &mut md) {
                            let f = { md.NodeData.FriendlyName };
                            let end = f.iter().position(|&c| c == 0).unwrap_or(f.len());
                            String::from_utf16_lossy(&f[..end])
                        } else {
                            String::new()
                        };
                        let ty = { md.NodeData.EngineType };
                        Arc::from(if name.is_empty() { engine_kind(ty).to_string() } else { name })
                    })
                    .collect::<Vec<Arc<str>>>();
                let (groups, node_group) = order_groups(&engines.iter().map(|e| &**e).collect::<Vec<_>>());
                adapters.push(Adapter {
                    h: a.hAdapter,
                    luid: a.AdapterLuid,
                    name: if name.is_empty() { "GPU".into() } else { name },
                    nodes: info.NodeCount,
                    segments: info.NbSegments,
                    vram_total: seg.DedicatedVideoMemorySize,
                    last_run: vec![(0, None); info.NodeCount as usize],
                    idle: vec![0; info.NodeCount as usize],
                    engines,
                    groups,
                    node_group,
                    shared_total: seg.SharedSystemMemorySize,
                    facts: Arc::new(read_facts(a.hAdapter, &full, &entries)),
                });
            }
        }
        // a discrete card first
        adapters.sort_by_key(|a| std::cmp::Reverse(a.vram_total));
        for (i, a) in adapters.iter_mut().enumerate() {
            for e in a.engines.iter_mut() {
                *e = Arc::from(format!("GPU {i} - {e}"));
            }
        }
        Gpus { adapters, light_n: 0 }
    }

    /// Engine labels per adapter, "GPU <i> - <engine>" by node ordinal,
    /// keyed by the adapter LUID (for the per-process engine column).
    pub fn engine_labels(&self) -> Vec<(LUID, Vec<Arc<str>>)> {
        self.adapters.iter().map(|a| (a.luid, a.engines.clone())).collect()
    }

    /// `light`: skip engines that have been idle (see IDLE_AFTER); the window
    /// is shown when it is false, and then every engine is read.
    pub fn sample(&mut self, light: bool) -> Vec<Gpu> {
        let now = Instant::now();
        let all = !light || self.light_n.is_multiple_of(LIGHT_FULL);
        if light {
            self.light_n = self.light_n.wrapping_add(1);
        }
        self.adapters
            .iter_mut()
            .map(|a| unsafe {
                // busiest engine since last sample
                let mut util = 0f32;
                let mut clock = None;
                // per engine group: only meaningful when every node was read
                let mut eng = if light { Vec::new() } else { vec![0f32; a.groups.len()] };
                for n in 0..a.nodes {
                    let i = n as usize;
                    // node 0 (3D, and the clock) is always read
                    if !all && n != 0 && a.idle[i] >= IDLE_AFTER {
                        continue;
                    }
                    let Some(s) = stats(D3DKMT_QUERYSTATISTICS_NODE, a.luid, n) else { continue };
                    let ni = s.QueryResult.NodeInformation;
                    let run = ni.GlobalInformation.RunningTime;
                    let (last, last_t) = a.last_run[i];
                    if let Some(t) = last_t {
                        let dt = now.duration_since(t).as_nanos() as f64 / 100.0; // 100 ns units
                        if dt > 0.0 {
                            let u = ((run - last) as f64 / dt * 100.0) as f32;
                            util = util.max(u);
                            if let Some(e) = a.node_group.get(i).and_then(|&g| eng.get_mut(g)) {
                                *e = e.max(u.clamp(0.0, 100.0));
                            }
                            a.idle[i] = if u < IDLE_PCT { a.idle[i].saturating_add(1) } else { 0 };
                        }
                    }
                    a.last_run[i] = (run, Some(now));
                    if n == 0 && ni.NodePerfData.Frequency > 0 {
                        clock = Some((ni.NodePerfData.Frequency / 1_000_000) as u32);
                    }
                }

                let (mut used, mut shared) = (0u64, 0u64);
                for sid in 0..a.segments {
                    if let Some(s) = stats(D3DKMT_QUERYSTATISTICS_SEGMENT, a.luid, sid) {
                        let si = s.QueryResult.SegmentInformation;
                        if si.Aperture == 0 {
                            used += si.BytesResident;
                        } else {
                            shared += si.BytesResident;
                        }
                    }
                }

                let mut pd = D3DKMT_ADAPTER_PERFDATA::default();
                let perf = query(a.h, KMTQAITYPE_ADAPTERPERFDATA, &mut pd);
                Gpu {
                    name: a.name.clone(),
                    util: util.clamp(0.0, 100.0),
                    temp: (perf && pd.Temperature > 0).then_some(pd.Temperature as f32 / 10.0),
                    vram_used: used,
                    vram_total: a.vram_total,
                    power_pct: (perf && pd.Power > 0).then_some(pd.Power as f32 / 10.0),
                    clock_mhz: clock,
                    fan_rpm: (perf && pd.FanRPM > 0).then_some(pd.FanRPM),
                    engines: a.groups.iter().cloned().zip(eng).collect(),
                    shared_used: shared,
                    shared_total: a.shared_total,
                    facts: a.facts.clone(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
impl Gpus {
    pub fn bench_parts(&self, n: usize) {
        for a in &self.adapters {
            eprintln!("adapter {} nodes {} segments {}", a.name, a.nodes, a.segments);
            let t = Instant::now();
            for _ in 0..n {
                for k in 0..a.nodes {
                    std::hint::black_box(stats(D3DKMT_QUERYSTATISTICS_NODE, a.luid, k));
                }
            }
            eprintln!("  nodes    {:>8.1} us", t.elapsed().as_secs_f64() * 1e6 / n as f64);
            let t = Instant::now();
            for _ in 0..n {
                for k in 0..a.segments {
                    std::hint::black_box(stats(D3DKMT_QUERYSTATISTICS_SEGMENT, a.luid, k));
                }
            }
            eprintln!("  segments {:>8.1} us", t.elapsed().as_secs_f64() * 1e6 / n as f64);
            let t = Instant::now();
            for _ in 0..n {
                let mut pd = D3DKMT_ADAPTER_PERFDATA::default();
                std::hint::black_box(query(a.h, KMTQAITYPE_ADAPTERPERFDATA, &mut pd));
            }
            eprintln!("  perfdata {:>8.1} us", t.elapsed().as_secs_f64() * 1e6 / n as f64);
        }
    }
}

fn engine_kind(t: DXGK_ENGINE_TYPE) -> &'static str {
    match t {
        DXGK_ENGINE_TYPE_3D => "3D",
        DXGK_ENGINE_TYPE_VIDEO_DECODE => "Video Decode",
        DXGK_ENGINE_TYPE_VIDEO_ENCODE => "Video Encode",
        DXGK_ENGINE_TYPE_VIDEO_PROCESSING => "Video Processing",
        DXGK_ENGINE_TYPE_SCENE_ASSEMBLY => "Scene Assembly",
        DXGK_ENGINE_TYPE_COPY => "Copy",
        DXGK_ENGINE_TYPE_OVERLAY => "Overlay",
        DXGK_ENGINE_TYPE_CRYPTO => "Crypto",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_engine_names() {
        assert_eq!(engine_group("Compute_1"), "Compute");
        assert_eq!(engine_group("Video Decode"), "Video Decode");
        assert_eq!(engine_group("JPEG_Decode_1"), "JPEG_Decode");
        assert_eq!(engine_group("_3"), "_3");
        assert_eq!(engine_group("Copy_"), "Copy_");
        let (g, idx) = order_groups(&["Copy", "3D", "Compute_0", "Security", "Compute_1", "Video Decode", "Graphics_1"]);
        let g: Vec<&str> = g.iter().map(|s| &**s).collect();
        assert_eq!(g, ["3D", "Compute", "Copy", "Video Decode", "Security", "Graphics"]);
        assert_eq!(idx, [2, 0, 1, 4, 1, 3, 5]);
    }
}
