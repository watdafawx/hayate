//! Debug-only: where does Blink's working set go? Walks the address space
//! (VirtualQuery) and the resident pages (QueryWorkingSet), then logs a
//! breakdown to blink.log. Enable with BLINK_MEMLOG=1.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_IMAGE, MEM_MAPPED, MEM_PRIVATE};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, QueryWorkingSet, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;

struct Region {
    base: usize,
    end: usize,
    alloc: usize,
    kind: u32,
}

pub fn start() {
    if std::env::var("BLINK_MEMLOG").is_err() {
        return;
    }
    std::thread::spawn(|| {
        for secs in [5u64, 25, 60] {
            std::thread::sleep(std::time::Duration::from_secs(if secs == 5 { 5 } else { secs - 5 }));
            crate::osutil::log(&format!("memreport t~{secs}s\n{}", report()));
        }
    });
}

fn report() -> String {
    unsafe {
        // 1. regions
        let mut regions = Vec::new();
        let mut commit_by: HashMap<&str, usize> = HashMap::new();
        let mut addr = 0usize;
        loop {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            if VirtualQuery(Some(addr as *const c_void), &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) == 0 {
                break;
            }
            let base = mbi.BaseAddress as usize;
            let end = base + mbi.RegionSize;
            if mbi.State == MEM_COMMIT {
                let k = match mbi.Type {
                    MEM_IMAGE => "image",
                    MEM_MAPPED => "mapped",
                    MEM_PRIVATE => "private",
                    _ => "other",
                };
                *commit_by.entry(k).or_default() += mbi.RegionSize;
                regions.push(Region { base, end, alloc: mbi.AllocationBase as usize, kind: mbi.Type.0 });
            }
            if end <= addr {
                break;
            }
            addr = end;
        }

        // 2. resident pages
        let mut buf: Vec<usize> = vec![0; 1 << 16];
        loop {
            match QueryWorkingSet(GetCurrentProcess(), buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32) {
                Ok(()) => break,
                Err(_) if buf.len() < (1 << 22) => buf = vec![0; buf.len() * 4],
                Err(e) => return format!("QueryWorkingSet: {}", e.message()),
            }
        }
        let n = buf[0].min(buf.len() - 1);
        let page = 4096usize;
        let mut ws: HashMap<String, (usize, usize)> = HashMap::new(); // key -> (private pages, shared pages)
        let mut by_alloc_priv: HashMap<usize, usize> = HashMap::new();
        for &e in &buf[1..=n] {
            let va = e & !0xfff;
            let shared = (e >> 8) & 1 == 1;
            let r = match regions.binary_search_by(|r| if va < r.base { std::cmp::Ordering::Greater } else if va >= r.end { std::cmp::Ordering::Less } else { std::cmp::Ordering::Equal }) {
                Ok(i) => &regions[i],
                Err(_) => continue,
            };
            let key = if r.kind == MEM_IMAGE.0 {
                let mut name = [0u16; 260];
                let l = GetModuleFileNameW(Some(HMODULE(r.alloc as *mut c_void)), &mut name) as usize;
                let s = String::from_utf16_lossy(&name[..l]);
                format!("img {}", s.rsplit('\\').next().unwrap_or(&s))
            } else if r.kind == MEM_MAPPED.0 {
                "mapped".into()
            } else {
                *by_alloc_priv.entry(r.alloc).or_default() += 1;
                "private".into()
            };
            let ent = ws.entry(key).or_default();
            if shared {
                ent.1 += 1;
            } else {
                ent.0 += 1;
            }
        }

        let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc as *mut _ as *mut _, std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32);
        let mb = |p: usize| p as f64 * page as f64 / 1048576.0;
        let mut out = format!(
            "  WS {:.1} MB, private bytes {:.1} MB; commit image {:.1} mapped {:.1} private {:.1} MB\n",
            pmc.WorkingSetSize as f64 / 1048576.0,
            pmc.PrivateUsage as f64 / 1048576.0,
            commit_by.get("image").copied().unwrap_or(0) as f64 / 1048576.0,
            commit_by.get("mapped").copied().unwrap_or(0) as f64 / 1048576.0,
            commit_by.get("private").copied().unwrap_or(0) as f64 / 1048576.0,
        );
        let tot_priv: usize = ws.values().map(|v| v.0).sum();
        let tot_sh: usize = ws.values().map(|v| v.1).sum();
        out += &format!("  resident: {:.1} MB not shared, {:.1} MB shared\n", mb(tot_priv), mb(tot_sh));
        let mut v: Vec<_> = ws.into_iter().collect();
        v.sort_by(|a, b| (b.1 .0 + b.1 .1).cmp(&(a.1 .0 + a.1 .1)));
        for (k, (p, s)) in v.iter().take(25) {
            out += &format!("  {:>7.2} MB  ({:>6.2} own, {:>6.2} shared)  {k}\n", mb(p + s), mb(*p), mb(*s));
        }
        let mut a: Vec<_> = by_alloc_priv.into_iter().collect();
        a.sort_by(|x, y| y.1.cmp(&x.1));
        out += "  biggest private allocations (resident):\n";
        for (base, p) in a.iter().take(8) {
            out += &format!("    {:#x}: {:.2} MB\n", base, mb(*p));
        }
        out
    }
}
