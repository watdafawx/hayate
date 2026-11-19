//! Hardware facts for the Vitals detail footers: what Task Manager's
//! Performance tab lists next to its graphs (cores, caches, base clock, RAM
//! speed and form factor, GPU driver, disk type...).
//!
//! Static facts are read once at startup on the sampler thread (`read_static`,
//! `Gpus::load`, `disk::enumerate`) and cached. The only live reading is the
//! CPU clock (`clock_now`), polled on the I/O worker while the CPU detail is
//! shown. Everything parsed from firmware or the registry is untrusted: every
//! offset is bounds checked, every number is range checked, every string is
//! stripped of control characters and capped. A fact that cannot be read stays
//! `None` / empty and the UI says "Unavailable"; nothing is guessed.
//!
//! Sources (see docs/hardware-parity.md for the full table):
//! - sockets / cores / caches / NUMA / groups: GetLogicalProcessorInformationEx(RelationAll)
//! - base clock: CallNtPowerInformation(ProcessorInformation).MaxMhz, else HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\N\~MHz
//! - current clock: PDH "Processor Information" `% Processor Performance` x `Processor Frequency`
//! - virtualization: IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED)
//! - RAM modules: SMBIOS type 17 (GetSystemFirmwareTable "RSMB")
//! - GPU driver: HKLM class key {4d36e968-...} matched by PCI vendor/device id
//!   (D3DKMT physical adapter ids), WDDM model and PCI location from D3DKMT

use std::collections::HashSet;
use windows::core::*;
use windows::Win32::System::Performance::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::{IsProcessorFeaturePresent, PF_VIRT_FIRMWARE_ENABLED};

use super::sys::{affinity_threads, records};

// Windows SDK pdh.h; omitted by the windows crate's generated constants.
const PDH_FMT_NOCAP100: PDH_FMT = PDH_FMT(0x0000_8000);

// ---------- CPU topology ----------

/// Total size of the caches of one level. Shared caches are counted once.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CacheTotal {
    pub bytes: u64,
    /// distinct cache instances
    pub count: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuTopo {
    /// RelationProcessorPackage records (0: not reported)
    pub packages: u32,
    /// RelationProcessorCore records
    pub cores: u32,
    /// distinct logical processors (group * 64 + bit) named by the core records
    pub logical: u32,
    /// active processor groups
    pub groups: u32,
    pub numa: u32,
    /// cores per efficiency class, highest class (fastest cores) first; more
    /// than one entry only on hybrid CPUs
    pub classes: Vec<(u8, u32)>,
    /// levels 1..=4 (index 0 = L1): data + instruction + unified of each
    pub caches: [CacheTotal; 4],
}

/// Parse a SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX buffer into counts.
/// Layouts: PROCESSOR_RELATIONSHIP (core @9 EfficiencyClass, @30 GroupCount,
/// @32 masks), CACHE_RELATIONSHIP (@8 Level, @12 CacheSize, @16 Type, @38
/// GroupCount, @40 masks), GROUP_RELATIONSHIP (@10 ActiveGroupCount).
pub fn parse_cpu_topology(b: &[u8]) -> CpuTopo {
    let mut t = CpuTopo::default();
    let mut threads: HashSet<usize> = HashSet::new();
    let mut seen_cache: HashSet<(u8, u32, Vec<(u16, u64)>)> = HashSet::new();
    let rd16 = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let rd32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    for (rel, i, size) in records(b) {
        match rel {
            // RelationProcessorCore
            0 if size >= 48 => {
                t.cores += 1;
                let mut th = Vec::new();
                affinity_threads(&b[..i + size], i + 32, rd16(i + 30).max(1) as usize, &mut th);
                threads.extend(th);
                let class = b[i + 9];
                match t.classes.iter_mut().find(|(c, _)| *c == class) {
                    Some((_, n)) => *n += 1,
                    None => t.classes.push((class, 1)),
                }
            }
            // RelationNumaNode
            1 => t.numa += 1,
            // RelationCache
            2 if size >= 56 => {
                let level = b[i + 8];
                if !(1..=4).contains(&level) {
                    continue;
                }
                let kb = rd32(i + 12) as u64;
                let ty = rd32(i + 16);
                let n = rd16(i + 38).max(1) as usize;
                // the masks identify the instance: a record repeated for each
                // sharing core would otherwise count one cache many times
                let masks: Vec<(u16, u64)> = (0..n)
                    .map_while(|k| {
                        let o = i + 40 + k * 16;
                        (o + 10 <= i + size).then(|| (u16::from_le_bytes([b[o + 8], b[o + 9]]), u64::from_le_bytes(b[o..o + 8].try_into().unwrap())))
                    })
                    .collect();
                if !seen_cache.insert((level, ty, masks)) {
                    continue;
                }
                let c = &mut t.caches[level as usize - 1];
                c.bytes += kb;
                c.count += 1;
            }
            // RelationProcessorPackage
            3 => t.packages += 1,
            // RelationGroup
            4 if size >= 12 => t.groups = t.groups.max(rd16(i + 10) as u32),
            _ => {}
        }
    }
    t.logical = threads.len() as u32;
    t.classes.sort_by_key(|(c, _)| std::cmp::Reverse(*c));
    t
}

/// "512 KB", "1.5 MB", "96 MB": a cache total.
pub fn fmt_cache(c: CacheTotal) -> Option<String> {
    (c.bytes > 0).then(|| fmt_kb_mb(c.bytes / 1024))
}
fn fmt_kb_mb(kb: u64) -> String {
    if kb >= 1024 {
        let mb = kb as f64 / 1024.0;
        let s = format!("{mb:.1}");
        format!("{} MB", s.trim_end_matches(".0"))
    } else {
        format!("{kb} KB")
    }
}

/// Everything static about the CPU beyond name / thread count.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuHw {
    pub topo: CpuTopo,
    /// lowest / highest nominal ("base") MHz over all logical processors
    pub base_mhz: Option<(u32, u32)>,
    /// firmware virtualization enabled (None: not reported)
    pub virt: Option<bool>,
}

/// "8 P + 16 E" on hybrid CPUs, empty otherwise.
pub fn hybrid_split(classes: &[(u8, u32)]) -> String {
    if classes.len() < 2 {
        return String::new();
    }
    let n = classes.len();
    classes
        .iter()
        .enumerate()
        .map(|(i, (_, c))| format!("{c} {}", if i == 0 { "P".to_string() } else if i == n - 1 { "E".to_string() } else { "M".to_string() }))
        .collect::<Vec<_>>()
        .join(" + ")
}

// ---------- base clock, virtualization ----------

fn wz(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn reg_dword(key: &str, val: &str) -> Option<u32> {
    let (k, v) = (wz(key), wz(val));
    let (mut d, mut len) = (0u32, 4u32);
    let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, PCWSTR(k.as_ptr()), PCWSTR(v.as_ptr()), RRF_RT_REG_DWORD, None, Some(&mut d as *mut u32 as *mut _), Some(&mut len)) };
    r.is_ok().then_some(d)
}

fn reg_string(key: &str, val: &str) -> Option<String> {
    let (k, v) = (wz(key), wz(val));
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, PCWSTR(k.as_ptr()), PCWSTR(v.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len)) };
    if r.is_err() {
        return None;
    }
    let n = (len as usize / 2).min(buf.len());
    let s = &buf[..n];
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    Some(String::from_utf16_lossy(&s[..end]))
}

/// First `max` subkey names of an HKLM key.
fn reg_subkeys(key: &str, max: u32) -> Vec<String> {
    let k = wz(key);
    let mut out = Vec::new();
    unsafe {
        let mut hk = HKEY::default();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(k.as_ptr()), None, KEY_READ, &mut hk).is_err() {
            return out;
        }
        for i in 0..max {
            let mut name = [0u16; 128];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(hk, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None).is_err() {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..(len as usize).min(name.len())]));
        }
        let _ = RegCloseKey(hk);
    }
    out
}

/// Plausible CPU clock, MHz.
fn sane_mhz(v: f64) -> bool {
    v.is_finite() && (100.0..=20_000.0).contains(&v)
}

/// (min, max) of the usable readings.
pub fn mhz_range(v: &[u32]) -> Option<(u32, u32)> {
    let ok: Vec<u32> = v.iter().copied().filter(|&m| sane_mhz(m as f64)).collect();
    Some((*ok.iter().min()?, *ok.iter().max()?))
}

/// Base clock, MHz: the rated non-boost frequency Windows keeps for each
/// logical processor (`PROCESSOR_POWER_INFORMATION.MaxMhz`, the same nominal
/// value as the `Processor Frequency` counter). On hybrid CPUs the cores
/// differ, hence a range. When the call fails, the `~MHz` value the OS
/// measured at boot (a few MHz off the nominal one) stands in; it is still a
/// base clock, never the live one.
pub fn base_mhz() -> Option<(u32, u32)> {
    nominal_mhz().or_else(|| {
        let base = "HARDWARE\\DESCRIPTION\\System\\CentralProcessor";
        let v: Vec<u32> = reg_subkeys(base, 512).iter().filter_map(|k| reg_dword(&format!("{base}\\{k}"), "~MHz")).collect();
        mhz_range(&v)
    })
}

fn nominal_mhz() -> Option<(u32, u32)> {
    use windows::Win32::System::Power::{CallNtPowerInformation, ProcessorInformation, PROCESSOR_POWER_INFORMATION};
    const N: usize = 1024;
    let mut buf = vec![PROCESSOR_POWER_INFORMATION::default(); N];
    let st = unsafe { CallNtPowerInformation(ProcessorInformation, None, 0, Some(buf.as_mut_ptr() as *mut _), (N * std::mem::size_of::<PROCESSOR_POWER_INFORMATION>()) as u32) };
    if st.0 != 0 {
        return None;
    }
    // entries past the last processor stay zero and fall out of the sanity range
    mhz_range(&buf.iter().map(|p| p.MaxMhz).collect::<Vec<_>>())
}

/// Virtualization enabled in firmware (Task Manager's "Virtualization").
pub fn virtualization() -> Option<bool> {
    Some(unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) }.as_bool())
}

/// "4.52" (GHz, two decimals) from MHz.
pub fn fmt_ghz(mhz: f64) -> String {
    format!("{:.2}", mhz / 1000.0)
}

/// "3.70 GHz" or "3.70 - 4.20 GHz" for a base range.
pub fn fmt_base(r: (u32, u32)) -> String {
    if r.0 == r.1 {
        format!("{} GHz", fmt_ghz(r.0 as f64))
    } else {
        format!("{}–{} GHz", fmt_ghz(r.0 as f64), fmt_ghz(r.1 as f64))
    }
}

// ---------- live CPU clock ----------

/// Current clock, MHz: each logical processor's nominal frequency times its
/// `% Processor Performance` (what Task Manager's "Speed" is made of; the
/// percentage goes past 100 while boosting).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CpuClock {
    /// mean over the logical processors that reported
    pub avg_mhz: f32,
    /// fastest logical processor
    pub max_mhz: f32,
    pub cores: u32,
}

/// `pairs` = (nominal MHz, % performance) per logical processor. A processor
/// with an implausible frequency or a non-positive / absurd percentage gave
/// no reading and is left out, so a counter that failed never drags the mean
/// to zero or substitutes the base clock.
pub fn clock_from(pairs: &[(f64, f64)]) -> Option<CpuClock> {
    let v: Vec<f64> = pairs.iter().filter(|(f, p)| sane_mhz(*f) && p.is_finite() && *p > 0.0 && *p <= 1000.0).map(|(f, p)| f * p / 100.0).collect();
    if v.is_empty() {
        return None;
    }
    Some(CpuClock { avg_mhz: (v.iter().sum::<f64>() / v.len() as f64) as f32, max_mhz: v.iter().copied().fold(0.0, f64::max) as f32, cores: v.len() as u32 })
}

/// "0,12" (group, processor) is a logical processor; "_Total" / "0,_Total" are not.
fn is_cpu_instance(name: &str) -> bool {
    match name.split_once(',') {
        Some((g, n)) => !g.is_empty() && !n.is_empty() && g.bytes().all(|b| b.is_ascii_digit()) && n.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

struct Query {
    q: PDH_HQUERY,
    perf: PDH_HCOUNTER,
    freq: PDH_HCOUNTER,
    buf: Vec<u64>,
    last: std::time::Instant,
}
// PDH handles are plain tokens used by one thread at a time (guarded by the static mutex)
unsafe impl Send for Query {}

impl Drop for Query {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.q);
        }
    }
}

impl Query {
    fn open() -> Option<Query> {
        unsafe {
            let mut q = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut q) != 0 {
                return None;
            }
            let (mut perf, mut freq) = (PDH_HCOUNTER::default(), PDH_HCOUNTER::default());
            let ok = PdhAddEnglishCounterW(q, w!("\\Processor Information(*)\\% Processor Performance"), 0, &mut perf) == 0
                && PdhAddEnglishCounterW(q, w!("\\Processor Information(*)\\Processor Frequency"), 0, &mut freq) == 0;
            if !ok {
                let _ = PdhCloseQuery(q);
                return None;
            }
            // the first collection only sets the baseline the rate counters need
            let _ = PdhCollectQueryData(q);
            Some(Query { q, perf, freq, buf: Vec::new(), last: std::time::Instant::now() })
        }
    }

    /// Per instance (name, value) of one counter, valid readings only.
    fn array(&mut self, c: PDH_HCOUNTER) -> Vec<(String, f64)> {
        unsafe {
            let (mut size, mut count) = (0u32, 0u32);
            let st = PdhGetFormattedCounterArrayW(c, PDH_FMT(PDH_FMT_DOUBLE.0 | PDH_FMT_NOCAP100.0), &mut size, &mut count, None);
            if st != PDH_MORE_DATA || size == 0 || size > 4 << 20 {
                return Vec::new();
            }
            self.buf.resize(size as usize / 8 + 2, 0);
            let base = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            let st = PdhGetFormattedCounterArrayW(c, PDH_FMT(PDH_FMT_DOUBLE.0 | PDH_FMT_NOCAP100.0), &mut size, &mut count, Some(base));
            if st != 0 {
                return Vec::new();
            }
            let (lo, hi) = (self.buf.as_ptr() as usize, self.buf.as_ptr() as usize + self.buf.len() * 8);
            let max_items = (size as usize / std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>()).min(count as usize);
            let items = std::slice::from_raw_parts(base, max_items);
            items
                .iter()
                .filter(|it| it.FmtValue.CStatus == PDH_CSTATUS_VALID_DATA || it.FmtValue.CStatus == PDH_CSTATUS_NEW_DATA)
                .filter_map(|it| {
                    // the name points into the buffer PDH just filled: check, then read at most 32 units
                    let p = it.szName.0 as usize;
                    if p < lo || p + 2 > hi {
                        return None;
                    }
                    let room = ((hi - p) / 2).min(32);
                    let s = std::slice::from_raw_parts(it.szName.0, room);
                    let n = s.iter().position(|&c| c == 0)?;
                    Some((String::from_utf16_lossy(&s[..n]), it.FmtValue.Anonymous.doubleValue))
                })
                .collect()
        }
    }

    fn read(&mut self) -> Option<CpuClock> {
        let stale = self.last.elapsed() > std::time::Duration::from_secs(4);
        self.last = std::time::Instant::now();
        unsafe {
            if PdhCollectQueryData(self.q) != 0 {
                return None;
            }
        }
        if stale {
            return None; // this collection establishes a fresh baseline after inactivity
        }
        let (perf, freq) = (self.perf, self.freq);
        let p = self.array(perf);
        let f = self.array(freq);
        let pairs: Vec<(f64, f64)> = p.iter().filter(|(n, _)| is_cpu_instance(n)).filter_map(|(n, pv)| f.iter().find(|(m, _)| m == n).map(|(_, fv)| (*fv, *pv))).collect();
        clock_from(&pairs)
    }
}

static QUERY: std::sync::Mutex<Option<Query>> = std::sync::Mutex::new(None);

/// The CPU clock right now (None until PDH has two samples, or when the
/// performance counters are unavailable). Called from the I/O worker.
pub fn clock_now() -> Option<CpuClock> {
    let mut g = QUERY.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        *g = Query::open();
    }
    g.as_mut()?.read()
}

// ---------- RAM modules (SMBIOS type 17) ----------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dimm {
    /// "DIMM A1", "ChannelA-DIMM0": the board's slot name
    pub locator: String,
    /// bank locator ("P0 CHANNEL A"): tells apart modules whose locators repeat per channel
    pub bank: String,
    /// 0 = empty slot
    pub size_mb: u64,
    /// Firmware reports a module present even when its capacity is unknown.
    pub populated: bool,
    /// "DDR5", or "RAM" when the type code is not one we name
    pub kind: &'static str,
    /// "DIMM", "SODIMM", "Row of chips"... empty when unknown
    pub form: &'static str,
    /// configured speed, MT/s (0 = not reported)
    pub cfg_mts: u32,
    /// rated (maximum) speed of the module, MT/s (0 = not reported)
    pub rated_mts: u32,
    pub maker: String,
    pub part: String,
}

fn mem_kind(code: u8) -> &'static str {
    match code {
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => "RAM",
    }
}

fn form_factor(code: u8) -> &'static str {
    match code {
        0x03 => "SIMM",
        0x09 => "DIMM",
        0x0B => "Row of chips",
        0x0C => "RIMM",
        0x0D => "SODIMM",
        0x0E => "SRIMM",
        0x0F => "FB-DIMM",
        0x10 => "Die",
        _ => "",
    }
}

/// A firmware string, safe to show: no control characters, capped, and the
/// placeholders boards ship instead of data removed.
pub fn clean_string(b: &[u8]) -> String {
    clean_text(b, 40)
}

/// `clean_string` with a length cap of `cap` characters.
pub fn clean_text(b: &[u8], cap: usize) -> String {
    let s: String = String::from_utf8_lossy(b).chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s: String = s.chars().take(cap).collect();
    let low = s.to_ascii_lowercase();
    let junk = ["not specified", "to be filled by o.e.m.", "unknown", "undefined", "none", "n/a", "default string", "not available", "empty", "unspecified"];
    if s.is_empty() || junk.contains(&low.as_str()) || s.bytes().all(|c| c == b'0' || c == b'-' || c == b'?') {
        String::new()
    } else {
        s
    }
}

/// Strings of an SMBIOS structure's string set starting at `start`, and where
/// the next structure begins.
fn string_set(data: &[u8], start: usize) -> (Vec<&[u8]>, usize) {
    let mut v = Vec::new();
    let mut j = start;
    loop {
        if j >= data.len() {
            return (v, data.len());
        }
        if data[j] == 0 {
            // an empty set is two NULs, a set ends with one extra NUL
            return (v.clone(), j + if v.is_empty() { 2 } else { 1 });
        }
        let end = data[j..].iter().position(|&c| c == 0).map(|p| j + p).unwrap_or(data.len());
        v.push(&data[j..end]);
        j = end + 1;
    }
}

/// Plausible module speed, MT/s (0 = unknown).
fn sane_mts(v: u64) -> u32 {
    if (100..=20_000).contains(&v) {
        v as u32
    } else {
        0
    }
}

/// Memory devices (type 17) of a RawSMBIOSData buffer, one per slot.
pub fn parse_smbios(raw: &[u8]) -> Vec<Dimm> {
    let mut out = Vec::new();
    if raw.len() < 8 {
        return out;
    }
    let declared = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let data = &raw[8..(8 + declared).min(raw.len())];
    let mut i = 0;
    let mut structs = 0;
    while i + 4 <= data.len() && structs < 8192 && out.len() < 128 {
        structs += 1;
        let t = data[i];
        let len = data[i + 1] as usize;
        if len < 4 || i + len > data.len() {
            break;
        }
        let s = &data[i..i + len];
        let (strs, next) = string_set(data, i + len);
        // SMBIOS 2.3 records (27 bytes) carry size, form, type and rated speed;
        // the configured speed (2.7) and extended size / speeds (3.x) come later
        if t == 17 && len >= 0x1B {
            let text = |off: usize| -> String {
                match s.get(off).copied() {
                    Some(n) if n >= 1 => strs.get(n as usize - 1).map(|b| clean_string(b)).unwrap_or_default(),
                    _ => String::new(),
                }
            };
            let sz = u16::from_le_bytes([s[0x0C], s[0x0D]]);
            let size_mb = match sz {
                0 | 0xFFFF => 0,
                0x7FFF if len >= 0x20 => (u32::from_le_bytes([s[0x1C], s[0x1D], s[0x1E], s[0x1F]]) & 0x7FFF_FFFF) as u64,
                0x7FFF => 0,
                v if v & 0x8000 != 0 => (v & 0x7FFF) as u64 / 1024,
                v => v as u64,
            };
            let rated = u16::from_le_bytes([s[0x15], s[0x16]]) as u64;
            let rated = if rated == 0xFFFF && len >= 0x58 { u32::from_le_bytes([s[0x54], s[0x55], s[0x56], s[0x57]]) as u64 } else { rated };
            let cfg = if len >= 0x22 { u16::from_le_bytes([s[0x20], s[0x21]]) as u64 } else { 0 };
            let cfg = if cfg == 0xFFFF && len >= 0x5C { u32::from_le_bytes([s[0x58], s[0x59], s[0x5A], s[0x5B]]) as u64 } else { cfg };
            let populated = sz != 0;
            out.push(Dimm {
                locator: text(0x10),
                bank: text(0x11),
                size_mb,
                populated,
                kind: if populated { mem_kind(s[0x12]) } else { "" },
                form: form_factor(s[0x0E]),
                // an empty slot keeps no speed: nothing runs there
                cfg_mts: if populated { sane_mts(cfg) } else { 0 },
                rated_mts: if populated { sane_mts(rated) } else { 0 },
                maker: if populated { text(0x17) } else { String::new() },
                part: if populated { text(0x1A) } else { String::new() },
            });
        }
        if t == 127 {
            break;
        }
        i = next;
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RamSummary {
    /// "DDR5-6000 · 2×32 GB" (the device list subtitle)
    pub desc: String,
    pub populated: u32,
    pub slots: u32,
    /// configured (min, max) MT/s over populated modules, (0, 0) = none reported
    pub cfg: (u32, u32),
    /// rated (min, max) MT/s
    pub rated: (u32, u32),
    /// "DIMM", or "DIMM / SODIMM" when the board mixes them; empty = unknown
    pub form: String,
    /// "DDR5", "DDR4 / DDR5"...; empty = no module
    pub kind: String,
}

fn range(v: impl Iterator<Item = u32>) -> (u32, u32) {
    let v: Vec<u32> = v.filter(|&x| x > 0).collect();
    (v.iter().copied().min().unwrap_or(0), v.iter().copied().max().unwrap_or(0))
}

fn gb_text(mb: u64) -> String {
    if mb >= 1024 && mb % 1024 == 0 {
        format!("{} GB", mb / 1024)
    } else if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

pub fn ram_summary(d: &[Dimm]) -> RamSummary {
    let pop: Vec<&Dimm> = d.iter().filter(|x| x.populated).collect();
    let mut r = RamSummary { populated: pop.len() as u32, slots: d.len() as u32, ..Default::default() };
    if pop.is_empty() {
        return r;
    }
    r.cfg = range(pop.iter().map(|x| x.cfg_mts));
    r.rated = range(pop.iter().map(|x| x.rated_mts));
    let uniq = |f: &dyn Fn(&Dimm) -> &'static str| {
        let mut v: Vec<&'static str> = Vec::new();
        for x in &pop {
            let k = f(x);
            if !k.is_empty() && !v.contains(&k) {
                v.push(k);
            }
        }
        v.join(" / ")
    };
    r.kind = uniq(&|x| x.kind);
    r.form = uniq(&|x| x.form);
    // module groups by size, in slot order: "2×8 GB + 2×16 GB"
    let mut groups: Vec<(u64, u32)> = Vec::new();
    for x in &pop {
        match groups.iter_mut().find(|g| g.0 == x.size_mb) {
            Some(g) => g.1 += 1,
            None => groups.push((x.size_mb, 1)),
        }
    }
    let layout = groups.iter().map(|(mb, n)| format!("{n}×{}", if *mb == 0 { "unknown size".into() } else { gb_text(*mb) })).collect::<Vec<_>>().join(" + ");
    // a speed in the name only when every module runs at the same one
    let name = if r.cfg.0 > 0 && r.cfg.0 == r.cfg.1 && !r.kind.contains('/') { format!("{}-{}", r.kind, r.cfg.1) } else { r.kind.clone() };
    r.desc = format!("{name} · {layout}");
    r
}

/// Label and value for the RAM speed: the configured speed when SMBIOS gives
/// one (a range when modules differ), else the rated speed, labelled as such.
pub fn speed_text(r: &RamSummary) -> Option<(&'static str, String)> {
    let fmt = |(lo, hi): (u32, u32)| if lo == hi { format!("{hi} MT/s") } else { format!("{lo}–{hi} MT/s") };
    if r.cfg.1 > 0 {
        Some(("Speed", fmt(r.cfg)))
    } else if r.rated.1 > 0 {
        Some(("Speed (rated)", fmt(r.rated)))
    } else {
        None
    }
}

/// One module for the footer: "A1 · 16 GB DDR4 · 3200 MT/s · Maker Part".
/// `qualify`: prefix the bank, for boards whose locators repeat ("DIMM 0" per channel).
pub fn dimm_line(d: &Dimm, qualify: bool) -> (String, String) {
    let label = match (d.locator.is_empty(), qualify && !d.bank.is_empty()) {
        (true, true) => d.bank.clone(),
        (true, false) => "Slot".to_string(),
        (false, true) => format!("{} {}", d.bank, d.locator),
        (false, false) => d.locator.clone(),
    };
    if !d.populated {
        return (label, "Empty".into());
    }
    let mut v = format!("{} {}", if d.size_mb == 0 { "Unknown size".into() } else { gb_text(d.size_mb) }, d.kind);
    if d.cfg_mts > 0 {
        v += &format!(" · {} MT/s", d.cfg_mts);
    } else if d.rated_mts > 0 {
        v += &format!(" · {} MT/s rated", d.rated_mts);
    }
    let who = [d.maker.as_str(), d.part.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
    if !who.is_empty() {
        v += &format!(" · {who}");
    }
    (label, v)
}

// ---------- GPU driver ----------

/// What the footer shows about one adapter, read once at load.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuFacts {
    /// the adapter string as the driver reports it
    pub full_name: String,
    pub driver_version: Option<String>,
    /// YYYY-MM-DD
    pub driver_date: Option<String>,
    /// "WDDM 3.1"
    pub wddm: Option<String>,
    /// "PCI bus 1, device 0, function 0"
    pub location: Option<String>,
    /// PCI vendor:device, "10DE:2206"
    pub pci_id: Option<String>,
}

/// 0x0001_0002_0003_0004 -> "1.2.3.4" (D3DKMT_UMD_DRIVER_VERSION).
pub fn fmt_umd_version(v: i64) -> Option<String> {
    if v <= 0 {
        return None;
    }
    let v = v as u64;
    Some(format!("{}.{}.{}.{}", v >> 48, (v >> 32) & 0xffff, (v >> 16) & 0xffff, v & 0xffff))
}

/// KMT_DRIVERVERSION (1000 = 1.0 ... 3100 = 3.1, 3200 = 3.2) -> "WDDM 3.1".
pub fn wddm_name(v: i32) -> Option<String> {
    (1000..10_000).contains(&v).then(|| format!("WDDM {}.{}", v / 1000, (v % 1000) / 100))
}

/// "10-27-2023" (the class key's DriverDate, month first) -> "2023-10-27".
pub fn parse_driver_date(s: &str) -> Option<String> {
    let mut it = s.trim().split(['-', '/']);
    let (m, d, y): (u32, u32, u32) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1990..=2100).contains(&y) {
        return None;
    }
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// Driver version / date from class-key entries (matching id, version, date)
/// of the adapter with this PCI id. Several matches (identical cards) must
/// agree; disagreement or no match gives nothing rather than a guess.
pub fn pick_driver(entries: &[(String, String, String)], vendor: u32, device: u32) -> Option<(String, Option<String>)> {
    let key = format!("ven_{vendor:04x}&dev_{device:04x}");
    let hits: Vec<&(String, String, String)> = entries.iter().filter(|e| e.0.to_ascii_lowercase().contains(&key) && !e.1.is_empty()).collect();
    let first = hits.first()?;
    if hits.iter().any(|h| h.1 != first.1) {
        return None;
    }
    let date = if hits.iter().all(|h| h.2 == first.2) { parse_driver_date(&first.2) } else { None };
    Some((clean_string(first.1.as_bytes()), date))
}

/// Display adapter class key entries: (MatchingDeviceId, DriverVersion, DriverDate).
pub fn class_entries() -> Vec<(String, String, String)> {
    let base = "SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}";
    reg_subkeys(base, 64)
        .iter()
        .filter(|k| k.len() == 4 && k.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|k| {
            let key = format!("{base}\\{k}");
            Some((reg_string(&key, "MatchingDeviceId")?, reg_string(&key, "DriverVersion").unwrap_or_default(), reg_string(&key, "DriverDate").unwrap_or_default()))
        })
        .collect()
}

pub fn fmt_location(bus: u32, dev: u32, func: u32) -> Option<String> {
    (bus < 256 && dev < 32 && func < 8).then(|| format!("PCI bus {bus}, device {dev}, function {func}"))
}

// ---------- disk ----------

/// "SSD", "HDD" or "Unknown": rotation from the seek-penalty flag; an NVMe
/// drive is flash even when the flag is missing.
pub fn disk_media(bus: &str, rotational: Option<bool>) -> &'static str {
    match rotational {
        Some(true) => "HDD",
        Some(false) => "SSD",
        None if bus == "NVMe" => "SSD",
        None => "Unknown",
    }
}

/// "NVMe", "SATA", "USB" from the bus label (which may carry the media word).
pub fn disk_bus(bus: &str) -> &str {
    bus.trim_end_matches(" HDD").trim_end_matches(" SSD")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_rec(class: u8, group: u16, mask: u64) -> Vec<u8> {
        let mut r = vec![0u8; 48];
        r[0..4].copy_from_slice(&0u32.to_le_bytes());
        r[4..8].copy_from_slice(&48u32.to_le_bytes());
        r[9] = class;
        r[30..32].copy_from_slice(&1u16.to_le_bytes());
        r[32..40].copy_from_slice(&mask.to_le_bytes());
        r[40..42].copy_from_slice(&group.to_le_bytes());
        r
    }
    fn simple_rec(rel: u32, size: usize) -> Vec<u8> {
        let mut r = vec![0u8; size];
        r[0..4].copy_from_slice(&rel.to_le_bytes());
        r[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        r
    }
    fn cache_rec(level: u8, kb: u32, ty: u32, group: u16, mask: u64) -> Vec<u8> {
        let mut r = simple_rec(2, 56);
        r[8] = level;
        r[12..16].copy_from_slice(&(kb * 1024).to_le_bytes());
        r[16..20].copy_from_slice(&ty.to_le_bytes());
        r[38..40].copy_from_slice(&1u16.to_le_bytes());
        r[40..48].copy_from_slice(&mask.to_le_bytes());
        r[48..50].copy_from_slice(&group.to_le_bytes());
        r
    }

    #[test]
    fn counts_sockets_cores_threads_and_shares_caches_once() {
        let mut b = Vec::new();
        b.extend(simple_rec(3, 48)); // package 0
        b.extend(simple_rec(3, 48)); // package 1
        // 4 SMT cores of 2 threads in group 0, 2 more in group 1
        for c in 0..4 {
            b.extend(core_rec(0, 0, 0b11 << (2 * c)));
        }
        b.extend(core_rec(0, 1, 0b11));
        b.extend(core_rec(0, 1, 0b1100));
        // per-core L1d + L1i + L2, one L3 shared by the group-0 cores
        for c in 0..4 {
            let m = 0b11u64 << (2 * c);
            b.extend(cache_rec(1, 32, 2, 0, m));
            b.extend(cache_rec(1, 32, 1, 0, m));
            b.extend(cache_rec(2, 1024, 0, 0, m));
        }
        b.extend(cache_rec(3, 32 * 1024, 0, 0, 0xff));
        b.extend(cache_rec(3, 32 * 1024, 0, 0, 0xff)); // same instance repeated: once
        b.extend(simple_rec(1, 48)); // numa 0
        let mut g = simple_rec(4, 80);
        g[10..12].copy_from_slice(&2u16.to_le_bytes());
        b.extend(g);
        let t = parse_cpu_topology(&b);
        assert_eq!((t.packages, t.cores, t.logical, t.groups, t.numa), (2, 6, 12, 2, 1));
        assert_eq!(t.caches[0], CacheTotal { bytes: 8 * 32 * 1024, count: 8 });
        assert_eq!(t.caches[1], CacheTotal { bytes: 4 * 1024 * 1024, count: 4 });
        assert_eq!(t.caches[2], CacheTotal { bytes: 32 * 1024 * 1024, count: 1 }, "shared L3 counted once");
        assert_eq!(t.caches[3], CacheTotal::default());
        assert_eq!(fmt_cache(t.caches[2]).as_deref(), Some("32 MB"));
        assert_eq!(fmt_cache(t.caches[0]).as_deref(), Some("256 KB"));
        assert_eq!(fmt_cache(CacheTotal::default()), None);
    }

    #[test]
    fn hybrid_cores_split_by_class_fastest_first() {
        let mut b = Vec::new();
        for c in 0..2 {
            b.extend(core_rec(1, 0, 0b11 << (2 * c)));
        }
        for c in 0..4 {
            b.extend(core_rec(0, 0, 1 << (4 + c)));
        }
        let t = parse_cpu_topology(&b);
        assert_eq!(t.classes, vec![(1, 2), (0, 4)]);
        assert_eq!(hybrid_split(&t.classes), "2 P + 4 E");
        assert_eq!(hybrid_split(&[(0, 8)]), "");
        assert_eq!(t.logical, 8);
    }

    #[test]
    fn hostile_topology_buffers_do_not_panic() {
        assert_eq!(parse_cpu_topology(&[]), CpuTopo::default());
        let mut b = core_rec(0, 0, 1);
        b[4..8].copy_from_slice(&0xffff_fff0u32.to_le_bytes()); // size runs off the end
        assert_eq!(parse_cpu_topology(&b).cores, 0);
        let mut c = cache_rec(9, 32, 0, 0, 1); // level out of range
        assert_eq!(parse_cpu_topology(&c).caches, [CacheTotal::default(); 4]);
        c[8] = 3;
        c[38..40].copy_from_slice(&u16::MAX.to_le_bytes()); // absurd mask count: bounded by the buffer
        assert_eq!(parse_cpu_topology(&c).caches[2].count, 1);
        assert_eq!(parse_cpu_topology(&c[..30]), CpuTopo::default());
        let mut core = core_rec(0, 0, 1);
        core[30..32].copy_from_slice(&2u16.to_le_bytes());
        let expected = parse_cpu_topology(&core);
        core.extend(cache_rec(3, 32, 0, 0, 1));
        assert_eq!(parse_cpu_topology(&core).logical, expected.logical, "a core mask must not read the following record");
    }

    #[test]
    fn base_clock_range_ignores_garbage() {
        assert_eq!(mhz_range(&[4300, 4300]), Some((4300, 4300)));
        assert_eq!(mhz_range(&[0, 2100, 3400, 99_999]), Some((2100, 3400)));
        assert_eq!(mhz_range(&[0]), None);
        assert_eq!(mhz_range(&[]), None);
        assert_eq!(fmt_base((3700, 3700)), "3.70 GHz");
        assert_eq!(fmt_base((2100, 3400)), "2.10–3.40 GHz");
        assert_eq!(fmt_ghz(4521.6), "4.52");
    }

    #[test]
    fn current_clock_is_frequency_times_performance_and_never_the_base() {
        // boosting: 4300 MHz nominal at 118 %
        let c = clock_from(&[(4300.0, 118.0), (4300.0, 100.0)]).unwrap();
        assert!((c.avg_mhz - 4687.0).abs() < 1.0, "{}", c.avg_mhz);
        assert!((c.max_mhz - 5074.0).abs() < 1.0);
        assert_eq!(c.cores, 2);
        // below nominal while idle
        assert!(clock_from(&[(4300.0, 40.0)]).unwrap().avg_mhz < 4300.0);
        // no performance reading: no clock, not the base
        assert_eq!(clock_from(&[(4300.0, 0.0)]), None);
        assert_eq!(clock_from(&[(4300.0, f64::NAN)]), None);
        assert_eq!(clock_from(&[(0.0, 100.0)]), None);
        assert_eq!(clock_from(&[]), None);
        // one dead processor does not pull the mean
        let c = clock_from(&[(3000.0, 100.0), (3000.0, 0.0)]).unwrap();
        assert_eq!((c.avg_mhz, c.cores), (3000.0, 1));
    }

    #[test]
    fn cpu_instances_are_numeric_pairs() {
        assert!(is_cpu_instance("0,12"));
        assert!(is_cpu_instance("1,0"));
        assert!(!is_cpu_instance("_Total"));
        assert!(!is_cpu_instance("0,_Total"));
        assert!(!is_cpu_instance(",1"));
        assert!(!is_cpu_instance("a,1"));
    }

    /// One type 17 record: `strings` are the string set (1-based indexes).
    #[allow(clippy::too_many_arguments)]
    fn dimm_rec(size: u16, ext: u32, form: u8, kind: u8, rated: u16, cfg: u16, len: usize, strings: &[&str]) -> Vec<u8> {
        let mut r = vec![0u8; len];
        r[0] = 17;
        r[1] = len as u8;
        r[0x0C..0x0E].copy_from_slice(&size.to_le_bytes());
        r[0x0E] = form;
        r[0x10] = 1; // locator
        r[0x12] = kind;
        r[0x15..0x17].copy_from_slice(&rated.to_le_bytes());
        r[0x17] = 2; // maker
        r[0x1A] = 3; // part
        if len >= 0x20 {
            r[0x1C..0x20].copy_from_slice(&ext.to_le_bytes());
        }
        if len >= 0x22 {
            r[0x20..0x22].copy_from_slice(&cfg.to_le_bytes());
        }
        for s in strings {
            r.extend(s.as_bytes());
            r.push(0);
        }
        if strings.is_empty() {
            r.push(0);
        }
        r.push(0);
        r
    }
    fn raw(structs: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = structs.iter().flatten().copied().chain([127, 4, 0, 0, 0, 0]).collect();
        let mut v = vec![0u8, 3, 3, 0];
        v.extend((body.len() as u32).to_le_bytes());
        v.extend(body);
        v
    }

    #[test]
    fn homogeneous_ddr5_pair() {
        // 32 GB does not fit the 15-bit MB field: 0x7FFF, size in the extended field
        let m = dimm_rec(0x7FFF, 32768, 9, 0x22, 6400, 6000, 0x28, &["DIMM A1", "Corsair", "CMK64GX5M2B6000C30"]);
        let r = raw(&[m.clone(), m.clone(), dimm_rec(0, 0, 9, 0, 0, 0, 0x28, &["DIMM B1"]), dimm_rec(0, 0, 9, 0, 0, 0, 0x28, &["DIMM B2"])]);
        let d = parse_smbios(&r);
        assert_eq!(d.len(), 4);
        assert_eq!((d[0].size_mb, d[0].kind, d[0].form, d[0].cfg_mts, d[0].rated_mts), (32768, "DDR5", "DIMM", 6000, 6400));
        assert_eq!((d[0].locator.as_str(), d[0].maker.as_str()), ("DIMM A1", "Corsair"));
        assert_eq!(d[2].size_mb, 0);
        let s = ram_summary(&d);
        assert_eq!((s.populated, s.slots, s.cfg, s.rated), (2, 4, (6000, 6000), (6400, 6400)));
        assert_eq!(s.desc, "DDR5-6000 · 2×32 GB");
        assert_eq!(speed_text(&s), Some(("Speed", "6000 MT/s".into())));
        assert_eq!(s.form, "DIMM");
        assert_eq!(dimm_line(&d[2], false), ("DIMM B1".to_string(), "Empty".to_string()));
        assert_eq!(dimm_line(&d[0], false), ("DIMM A1".to_string(), "32 GB DDR5 · 6000 MT/s · Corsair CMK64GX5M2B6000C30".to_string()));
    }

    #[test]
    fn mixed_modules_are_not_collapsed() {
        let a = dimm_rec(8192, 0, 9, 0x1A, 3200, 2666, 0x28, &["A1", "X", "P1"]);
        let b = dimm_rec(16384, 0, 0x0D, 0x1A, 3200, 3200, 0x28, &["B1", "Y", "P2"]);
        let s = ram_summary(&parse_smbios(&raw(&[a.clone(), a, b.clone(), b])));
        assert_eq!(s.desc, "DDR4 · 2×8 GB + 2×16 GB", "no single speed in the name when speeds differ");
        assert_eq!(s.cfg, (2666, 3200));
        assert_eq!(speed_text(&s), Some(("Speed", "2666–3200 MT/s".into())));
        assert_eq!(s.form, "DIMM / SODIMM");
    }

    #[test]
    fn unknown_and_extended_speeds_and_sizes() {
        // 2.3-style short record: no configured speed -> rated, labelled
        let short = dimm_rec(4096, 0, 9, 0x18, 1600, 0, 0x1C, &["A1", "", ""]);
        let s = ram_summary(&parse_smbios(&raw(&[short])));
        assert_eq!(s.cfg, (0, 0));
        assert_eq!(speed_text(&s), Some(("Speed (rated)", "1600 MT/s".into())));
        // extended size (32 GB module reported as 0x7FFF + MB) and a nonsense speed
        let big = dimm_rec(0x7FFF, 65536, 9, 0x22, 0, 40_000, 0x28, &["A1"]);
        let d = parse_smbios(&raw(&[big]));
        assert_eq!((d[0].size_mb, d[0].cfg_mts), (65536, 0));
        assert_eq!(speed_text(&ram_summary(&d)), None, "no speed reported: no speed shown");
        // KB-granular size field (bit 15)
        let kb = dimm_rec(0x8000 | 2048, 0, 9, 0x22, 4800, 4800, 0x28, &["A1"]);
        assert_eq!(parse_smbios(&raw(&[kb]))[0].size_mb, 2);
        // Unknown capacity still occupies a slot and retains the reported module facts.
        let unknown = parse_smbios(&raw(&[dimm_rec(0xFFFF, 0, 9, 0x22, 4800, 4800, 0x28, &[])]));
        assert_eq!(unknown[0].size_mb, 0);
        assert!(unknown[0].populated);
        let summary = ram_summary(&unknown);
        assert_eq!(summary.populated, 1);
        assert_eq!(summary.cfg, (4800, 4800));
        assert!(dimm_line(&unknown[0], false).1.starts_with("Unknown size"));
        // no module at all
        assert_eq!(ram_summary(&[]).desc, "");
        assert_eq!(ram_summary(&[]).slots, 0);
    }

    #[test]
    fn hostile_smbios_tables_do_not_panic() {
        assert!(parse_smbios(&[]).is_empty());
        assert!(parse_smbios(&[0; 7]).is_empty());
        let mut r = raw(&[dimm_rec(8192, 0, 9, 0x22, 4800, 4800, 0x28, &["A1", "M", "P"])]);
        r.truncate(r.len() - 20);
        let _ = parse_smbios(&r);
        // length byte of 0 would never advance
        let mut z = vec![0u8, 3, 3, 0, 8, 0, 0, 0];
        z.extend([17, 0, 0, 0, 0, 0, 0, 0]);
        assert!(parse_smbios(&z).is_empty());
        // declared length larger than the buffer
        let mut big = raw(&[dimm_rec(8192, 0, 9, 0x22, 4800, 4800, 0x28, &["A1"])]);
        big[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_smbios(&big).len(), 1);
        // a string index past the set, and one with control characters
        let mut m = dimm_rec(8192, 0, 9, 0x22, 4800, 4800, 0x28, &["A\u{7}1"]);
        m[0x17] = 9;
        let d = parse_smbios(&raw(&[m]));
        assert_eq!((d[0].locator.as_str(), d[0].maker.as_str()), ("A 1", ""));
    }

    #[test]
    fn repeated_locators_are_qualified_by_bank() {
        let d = |bank: &str, loc: &str| Dimm { locator: loc.into(), bank: bank.into(), size_mb: 16384, populated: true, kind: "DDR4", form: "DIMM", cfg_mts: 3600, rated_mts: 3600, maker: String::new(), part: String::new() };
        assert_eq!(dimm_line(&d("P0 CHANNEL A", "DIMM 0"), true).0, "P0 CHANNEL A DIMM 0");
        assert_eq!(dimm_line(&d("P0 CHANNEL A", "DIMM 0"), false).0, "DIMM 0");
        assert_eq!(dimm_line(&d("", "DIMM 0"), true).0, "DIMM 0", "no bank string: the locator alone");
        assert_eq!(dimm_line(&d("BANK 1", ""), true).0, "BANK 1");
        assert_eq!(dimm_line(&d("", ""), true).0, "Slot");
    }

    #[test]
    fn firmware_strings_are_cleaned() {
        assert_eq!(clean_string(b"  Corsair \x01 RAM "), "Corsair RAM");
        assert_eq!(clean_string(b"To Be Filled By O.E.M."), "");
        assert_eq!(clean_string(b"Unknown"), "");
        assert_eq!(clean_string(b"0000"), "");
        assert_eq!(clean_string(&[b'x'; 200]).len(), 40);
    }

    #[test]
    fn gpu_driver_facts() {
        assert_eq!(fmt_umd_version(0x001F_0000_000F_11F9).as_deref(), Some("31.0.15.4601"));
        assert_eq!(fmt_umd_version(0), None);
        assert_eq!(fmt_umd_version(-1), None);
        assert_eq!(wddm_name(3100).as_deref(), Some("WDDM 3.1"));
        assert_eq!(wddm_name(2700).as_deref(), Some("WDDM 2.7"));
        assert_eq!(wddm_name(0), None);
        assert_eq!(parse_driver_date("10-27-2023").as_deref(), Some("2023-10-27"));
        assert_eq!(parse_driver_date("1-5-2024").as_deref(), Some("2024-01-05"));
        assert_eq!(parse_driver_date("13-1-2024"), None);
        assert_eq!(parse_driver_date("garbage"), None);
        assert_eq!(parse_driver_date("1-2-3-4"), None);
        assert_eq!(fmt_location(1, 0, 0).as_deref(), Some("PCI bus 1, device 0, function 0"));
        assert_eq!(fmt_location(999, 0, 0), None);
    }

    #[test]
    fn driver_matches_by_pci_id_and_refuses_conflicts() {
        let e = |id: &str, v: &str, d: &str| (id.to_string(), v.to_string(), d.to_string());
        let list = vec![e("PCI\\VEN_10DE&DEV_2206&SUBSYS_1", "31.0.15.4601", "10-27-2023"), e("PCI\\VEN_8086&DEV_4680", "30.0.101.1", "1-2-2023")];
        assert_eq!(pick_driver(&list, 0x10DE, 0x2206), Some(("31.0.15.4601".into(), Some("2023-10-27".into()))));
        assert_eq!(pick_driver(&list, 0x10DE, 0x9999), None, "no entry: nothing");
        // two identical cards, same driver: fine; different drivers: ambiguous
        let twin = vec![list[0].clone(), list[0].clone()];
        assert!(pick_driver(&twin, 0x10DE, 0x2206).is_some());
        let clash = vec![list[0].clone(), e("PCI\\VEN_10DE&DEV_2206", "30.0.1.1", "1-1-2022")];
        assert_eq!(pick_driver(&clash, 0x10DE, 0x2206), None);
        // a bad date leaves the version
        let nodate = vec![e("PCI\\VEN_10DE&DEV_2206", "1.2.3.4", "x")];
        assert_eq!(pick_driver(&nodate, 0x10DE, 0x2206), Some(("1.2.3.4".into(), None)));
    }

    #[test]
    fn disk_media_and_bus() {
        assert_eq!(disk_media("NVMe", None), "SSD");
        assert_eq!(disk_media("SATA", Some(true)), "HDD");
        assert_eq!(disk_media("SATA", Some(false)), "SSD");
        assert_eq!(disk_media("USB", None), "Unknown", "a USB bridge that hides rotation is not guessed");
        assert_eq!(disk_media("NVMe", Some(true)), "HDD");
        assert_eq!(disk_bus("SATA HDD"), "SATA");
        assert_eq!(disk_bus("NVMe"), "NVMe");
    }
}
