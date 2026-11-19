//! Static system facts read once (CPU name, RAM layout from SMBIOS) and
//! cheap per-tick system counters (memory, uptime).

use windows::core::*;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::*;

#[derive(Clone, Default)]
pub struct Static {
    pub cpu_name: String,
    pub ncpu: usize,
    pub ram_desc: String, // e.g. "DDR5-6000 · 2×32 GB"
    /// RAM installed on the board (GetPhysicallyInstalledSystemMemory), bytes
    pub ram_installed: u64,
    /// thread groups that share an L3 (CCDs) or an efficiency class (P / E
    /// cores); one entry when the CPU has only one
    pub clusters: Vec<Cluster>,
    /// sockets, cores, caches, base clock, virtualization (see `hw`)
    pub cpu_hw: super::hw::CpuHw,
    /// every memory slot from SMBIOS type 17, empty ones included
    pub dimms: Vec<super::hw::Dimm>,
}

/// A group of logical processors shown as one graph line / heatmap block.
#[derive(Clone, Debug, PartialEq)]
pub struct Cluster {
    /// "CCD0", "P-cores", or "CPU" when there is only one
    pub name: String,
    /// "V-Cache" / "Frequency" on X3D parts with one V-Cache CCD, else empty
    pub role: String,
    /// logical processor indices (into `Snapshot.cores`), ascending
    pub threads: Vec<usize>,
}

pub fn read_static() -> Static {
    let mut si = SYSTEM_INFO::default();
    unsafe { GetSystemInfo(&mut si) };
    let cpu_name = reg_str(w!("HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0"), w!("ProcessorNameString"))
        .unwrap_or_else(|| "CPU".into())
        .trim()
        .to_string();
    let dimms = smbios_raw().map(|raw| super::hw::parse_smbios(&raw)).unwrap_or_default();
    let ram = super::hw::ram_summary(&dimms);
    let ram_desc = ram.desc;
    let ncpu = si.dwNumberOfProcessors as usize;
    let mut kb = 0u64;
    let ram_installed = if unsafe { GetPhysicallyInstalledSystemMemory(&mut kb) }.is_ok() { kb * 1024 } else { 0 };
    let ((l3, eff), topo) = topology().unwrap_or_default();
    let clusters = clusters(&l3, &eff, ncpu, &cpu_name);
    let cpu_hw = super::hw::CpuHw { topo, base_mhz: super::hw::base_mhz(), virt: super::hw::virtualization() };
    Static { cpu_name, ncpu, ram_desc, ram_installed, clusters, cpu_hw, dimms }
}

/// (L3 size in KB, threads) per L3 cache and (efficiency class, threads) per
/// class, from GetLogicalProcessorInformationEx(RelationAll).
type Topo = (Vec<(u32, Vec<usize>)>, Vec<(u8, Vec<usize>)>);

fn topology() -> Option<(Topo, super::hw::CpuTopo)> {
    let mut len = 0u32;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationAll, None, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u64; len as usize / 8 + 1];
        GetLogicalProcessorInformationEx(RelationAll, Some(buf.as_mut_ptr() as *mut _), &mut len).ok()?;
        let bytes = std::slice::from_raw_parts(buf.as_ptr() as *const u8, len as usize);
        Some((parse_topology(bytes), super::hw::parse_cpu_topology(bytes)))
    }
}

/// Walk SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX records as (relationship,
/// offset, size). Stops at the first malformed size, so a truncated or
/// hostile buffer ends the walk instead of running off the end.
pub(super) fn records(b: &[u8]) -> impl Iterator<Item = (u32, usize, usize)> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        if i + 8 > b.len() {
            return None;
        }
        let rel = u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let size = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        if size < 8 || i + size > b.len() {
            return None;
        }
        let at = i;
        i += size;
        Some((rel, at, size))
    })
}

/// Threads of a GROUP_AFFINITY array at `off` (group * 64 + bit).
pub(super) fn affinity_threads(b: &[u8], off: usize, count: usize, out: &mut Vec<usize>) {
    for k in 0..count {
        let o = off + k * 16;
        if o + 10 > b.len() {
            break;
        }
        let mask = u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let group = u16::from_le_bytes([b[o + 8], b[o + 9]]) as usize;
        out.extend((0..64).filter(|bit| mask & (1u64 << bit) != 0).map(|bit| group * 64 + bit));
    }
}

/// Parse a SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX buffer (see `topology`).
fn parse_topology(b: &[u8]) -> Topo {
    let (mut l3, mut eff): Topo = (Vec::new(), Vec::new());
    let rd16 = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let rd32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    for (rel, i, size) in records(b) {
        match rel {
            // RelationProcessorCore: EfficiencyClass @9, GroupCount @30, masks @32
            0 if size >= 48 => {
                let mut t = Vec::new();
                affinity_threads(b, i + 32, rd16(i + 30).max(1) as usize, &mut t);
                let class = b[i + 9];
                match eff.iter_mut().find(|(c, _)| *c == class) {
                    Some((_, v)) => v.extend(t),
                    None => eff.push((class, t)),
                }
            }
            // RelationCache: Level @8, CacheSize @12, GroupCount @38 (0 = 1), masks @40
            2 if size >= 56 && b[i + 8] == 3 => {
                let mut t = Vec::new();
                affinity_threads(b, i + 40, rd16(i + 38).max(1) as usize, &mut t);
                l3.push((rd32(i + 12) / 1024, t));
            }
            _ => {}
        }
    }
    (l3, eff)
}

/// CCDs when there is more than one L3, else P / E cores when there is more
/// than one efficiency class, else one "CPU" cluster with every thread.
fn clusters(l3: &[(u32, Vec<usize>)], eff: &[(u8, Vec<usize>)], ncpu: usize, cpu_name: &str) -> Vec<Cluster> {
    let clean = |v: &[usize]| {
        let mut v: Vec<usize> = v.iter().copied().filter(|&t| t < ncpu).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let l3: Vec<(u32, Vec<usize>)> = l3.iter().map(|(kb, t)| (*kb, clean(t))).filter(|(_, t)| !t.is_empty()).collect();
    if l3.len() >= 2 {
        let mut l3 = l3;
        l3.sort_by_key(|(_, t)| t[0]);
        let max = l3.iter().map(|(kb, _)| *kb).max().unwrap_or(0);
        let big = l3.iter().filter(|(kb, _)| *kb == max).count();
        // one CCD with the stacked cache: label both roles
        let vcache = big == 1 && cpu_name.contains("X3D");
        let pre = if cpu_name.contains("AMD") || cpu_name.contains("Ryzen") || cpu_name.contains("EPYC") { "CCD" } else { "L3 " };
        return l3
            .into_iter()
            .enumerate()
            .map(|(i, (kb, threads))| Cluster {
                name: format!("{pre}{i}"),
                role: if !vcache { String::new() } else if kb == max { "V-Cache".into() } else { "Frequency".into() },
                threads,
            })
            .collect();
    }
    let eff: Vec<(u8, Vec<usize>)> = eff.iter().map(|(c, t)| (*c, clean(t))).filter(|(_, t)| !t.is_empty()).collect();
    if eff.len() >= 2 {
        let mut eff = eff;
        // highest class = fastest cores, listed first
        eff.sort_by_key(|(c, _)| std::cmp::Reverse(*c));
        let n = eff.len();
        return eff
            .into_iter()
            .enumerate()
            .map(|(i, (_, threads))| Cluster {
                name: if i == 0 { "P-cores".into() } else if i == n - 1 { "E-cores".into() } else { format!("Class {}", n - 1 - i) },
                role: String::new(),
                threads,
            })
            .collect();
    }
    vec![Cluster { name: "CPU".into(), role: String::new(), threads: (0..ncpu).collect() }]
}

pub(super) fn reg_str(key: PCWSTR, val: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, key, val, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len)) };
    if r.is_err() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]))
}

/// The raw SMBIOS table (RawSMBIOSData: 8 byte header, then the structures).
fn smbios_raw() -> Option<Vec<u8>> {
    let sig = u32::from_be_bytes(*b"RSMB");
    let n = unsafe { GetSystemFirmwareTable(FIRMWARE_TABLE_PROVIDER(sig), 0, None) };
    if n == 0 {
        return None;
    }
    let mut buf = vec![0u8; n as usize];
    let got = unsafe { GetSystemFirmwareTable(FIRMWARE_TABLE_PROVIDER(sig), 0, Some(&mut buf)) };
    buf.truncate((got as usize).min(buf.len()));
    Some(buf)
}

#[derive(Clone, Copy, Default)]
pub struct Mem {
    pub total: u64,
    pub avail: u64,
    pub commit: u64,
    pub commit_limit: u64,
    pub cached: u64,
    pub paged_pool: u64,
    pub nonpaged_pool: u64,
}

/// Physical memory lists (SystemMemoryListInformation), bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct MemLists {
    /// free + zeroed
    pub free: u64,
    /// modified (incl. modified no-write): must be written before reuse
    pub modified: u64,
    /// standby, all priorities: cache, reusable at once
    pub standby: u64,
}

const SYSTEM_MEMORY_LIST_INFORMATION: u32 = 80;
/// SYSTEM_MEMORY_LIST_INFORMATION on 64-bit: 5 counts, 2 x 8 by priority, 1 count
const MEM_LIST_LEN: usize = 22 * 8;

/// Parse SYSTEM_MEMORY_LIST_INFORMATION (page counts, 64-bit layout).
pub fn parse_mem_lists(b: &[u8], page: u64) -> Option<MemLists> {
    if b.len() < MEM_LIST_LEN {
        return None;
    }
    let q = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    // ZeroPageCount, FreePageCount, ModifiedPageCount, ModifiedNoWritePageCount,
    // BadPageCount, PageCountByPriority[8] (standby), RepurposedPagesByPriority[8], ...
    let standby: u64 = (5..13).map(q).sum();
    Some(MemLists { free: (q(0) + q(1)) * page, modified: (q(2) + q(3)) * page, standby: standby * page })
}

/// Free / modified / standby lists. Windows 11 answers this unelevated
/// (checked on dev PC, 26300: status 0 from a standard token); older builds
/// want SeProfileSingleProcessPrivilege, which only an elevated token holds,
/// so a failed query is retried once with it enabled. Still failing: None,
/// and the composition view hides those fields. One syscall (~2 us), only
/// while the window is shown.
pub fn mem_lists() -> Option<MemLists> {
    use std::sync::atomic::{AtomicU8, Ordering};
    // 0 = untried, 1 = works, 2 = never works
    static STATE: AtomicU8 = AtomicU8::new(0);
    let query = || {
        let mut buf = [0u64; MEM_LIST_LEN / 8];
        let mut ret = 0u32;
        let st = unsafe { super::nt::NtQuerySystemInformation(SYSTEM_MEMORY_LIST_INFORMATION, buf.as_mut_ptr() as *mut _, MEM_LIST_LEN as u32, &mut ret) };
        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, MEM_LIST_LEN) };
        if st >= 0 { parse_mem_lists(bytes, 4096) } else { None }
    };
    match STATE.load(Ordering::Relaxed) {
        2 => None,
        1 => query(),
        _ => {
            let r = query().or_else(|| if crate::osutil::is_elevated() && enable_privilege(w!("SeProfileSingleProcessPrivilege")) { query() } else { None });
            STATE.store(if r.is_some() { 1 } else { 2 }, Ordering::Relaxed);
            r
        }
    }
}

/// Enable a privilege the token holds (disabled by default).
fn enable_privilege(name: PCWSTR) -> bool {
    use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LUID, WIN32_ERROR};
    use windows::Win32::Security::*;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        let mut luid = LUID::default();
        let mut ok = LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid).is_ok();
        if ok {
            let tp = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }] };
            // succeeds even when the privilege is not held: check the last error
            ok = AdjustTokenPrivileges(tok, false, Some(&tp), 0, None, None).is_ok() && GetLastError() == WIN32_ERROR(0);
        }
        let _ = CloseHandle(tok);
        ok
    }
}

pub fn mem() -> Mem {
    let mut pi = PERFORMANCE_INFORMATION { cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32, ..Default::default() };
    unsafe {
        let _ = GetPerformanceInfo(&mut pi, pi.cb);
    }
    let pg = pi.PageSize as u64;
    Mem {
        total: pi.PhysicalTotal as u64 * pg,
        avail: pi.PhysicalAvailable as u64 * pg,
        commit: pi.CommitTotal as u64 * pg,
        commit_limit: pi.CommitLimit as u64 * pg,
        cached: pi.SystemCache as u64 * pg,
        paged_pool: pi.KernelPaged as u64 * pg,
        nonpaged_pool: pi.KernelNonpaged as u64 * pg,
    }
}

pub fn uptime_s() -> u64 {
    unsafe { GetTickCount64() / 1000 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_rec(class: u8, mask: u64) -> Vec<u8> {
        let mut r = vec![0u8; 48];
        r[0..4].copy_from_slice(&0u32.to_le_bytes());
        r[4..8].copy_from_slice(&48u32.to_le_bytes());
        r[9] = class;
        r[30..32].copy_from_slice(&1u16.to_le_bytes());
        r[32..40].copy_from_slice(&mask.to_le_bytes());
        r
    }
    fn l3_rec(level: u8, kb: u32, mask: u64) -> Vec<u8> {
        let mut r = vec![0u8; 56];
        r[0..4].copy_from_slice(&2u32.to_le_bytes());
        r[4..8].copy_from_slice(&56u32.to_le_bytes());
        r[8] = level;
        r[12..16].copy_from_slice(&(kb * 1024).to_le_bytes());
        // GroupCount 0 on older Windows means one GroupMask
        r[40..48].copy_from_slice(&mask.to_le_bytes());
        r
    }

    #[test]
    fn parses_l3_and_efficiency_classes() {
        let mut b = Vec::new();
        b.extend(core_rec(1, 0b0011));
        b.extend(core_rec(0, 0b1100));
        b.extend(l3_rec(2, 1024, 0b0011)); // L2: ignored
        b.extend(l3_rec(3, 96 * 1024, 0b0011));
        b.extend(l3_rec(3, 32 * 1024, 0b1100));
        let (l3, eff) = parse_topology(&b);
        assert_eq!(l3, vec![(96 * 1024, vec![0, 1]), (32 * 1024, vec![2, 3])]);
        assert_eq!(eff, vec![(1, vec![0, 1]), (0, vec![2, 3])]);
        // truncated record: stops, no panic
        let (l3, _) = parse_topology(&b[..b.len() - 10]);
        assert_eq!(l3.len(), 1);
    }

    #[test]
    fn x3d_dual_ccd_gets_roles() {
        let l3 = vec![(96 * 1024, (0..16).collect()), (32 * 1024, (16..32).collect())];
        let c = clusters(&l3, &[], 32, "AMD Ryzen 9 9950X3D 16-Core Processor");
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].name.as_str(), c[0].role.as_str()), ("CCD0", "V-Cache"));
        assert_eq!((c[1].name.as_str(), c[1].role.as_str()), ("CCD1", "Frequency"));
        assert_eq!(c[1].threads, (16..32).collect::<Vec<_>>());
        // plain dual CCD: no roles
        let l3 = vec![(32 * 1024, (0..16).collect()), (32 * 1024, (16..32).collect())];
        let c = clusters(&l3, &[], 32, "AMD Ryzen 9 7950X 16-Core Processor");
        assert!(c.iter().all(|c| c.role.is_empty()));
    }

    #[test]
    fn hybrid_intel_splits_p_and_e() {
        let l3 = vec![(36 * 1024, (0..24).collect())];
        let eff = vec![(0, (16..24).collect()), (1, (0..16).collect())];
        let c = clusters(&l3, &eff, 24, "Intel(R) Core(TM) i7-14700K");
        assert_eq!(c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["P-cores", "E-cores"]);
        assert_eq!(c[0].threads.len(), 16);
    }

    #[test]
    fn single_l3_is_one_cluster() {
        let c = clusters(&[(32 * 1024, (0..12).collect())], &[(0, (0..12).collect())], 12, "AMD Ryzen 5 7600");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].threads.len(), 12);
        // no topology at all
        assert_eq!(clusters(&[], &[], 4, "CPU")[0].threads, vec![0, 1, 2, 3]);
    }

    #[test]
    fn parses_memory_lists() {
        let mut b = vec![0u8; MEM_LIST_LEN];
        let mut put = |i: usize, v: u64| b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        put(0, 10); // zero
        put(1, 5); // free
        put(2, 100); // modified
        put(3, 1); // modified no-write
        put(4, 999); // bad: ignored
        for p in 5..13 {
            put(p, 2); // standby by priority
        }
        put(13, 777); // repurposed: ignored
        let m = parse_mem_lists(&b, 4096).unwrap();
        assert_eq!(m, MemLists { free: 15 * 4096, modified: 101 * 4096, standby: 16 * 4096 });
        assert_eq!(parse_mem_lists(&b[..100], 4096), None);
    }

    /// `cargo test mem_lists_live -- --ignored --nocapture`: does the raw
    /// query work in this token (with and without the privilege)?
    #[test]
    #[ignore]
    fn mem_lists_live() {
        let mut buf = [0u64; MEM_LIST_LEN / 8];
        let mut ret = 0u32;
        let st = unsafe { crate::sample::nt::NtQuerySystemInformation(SYSTEM_MEMORY_LIST_INFORMATION, buf.as_mut_ptr() as *mut _, MEM_LIST_LEN as u32, &mut ret) };
        eprintln!("elevated={} raw status=0x{:08x}", crate::osutil::is_elevated(), st as u32);
        eprintln!("mem_lists() = {:?}", mem_lists());
        let s = read_static();
        eprintln!("dimms {} installed {} clusters {:?}", s.dimms.len(), s.ram_installed, s.clusters);
    }
}
