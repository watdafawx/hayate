//! Per-process GPU use, like Task Manager's GPU / GPU engine columns.
//!
//! Source: the kernel's "GPU Engine" performance counter set (dxgkrnl, one
//! instance per process x engine, named `pid_N_luid_0xH_0xL_phys_0_eng_E_engtype_T`),
//! read raw through the PerfLib V2 consumer API in advapi32. That is the
//! data Task Manager shows and it covers every process unelevated
//! (dwm, services, elevated apps). Going through PDH instead costs ~6 MB of
//! working set for its name tables; the D3DKMT per-process query needs
//! PROCESS_QUERY_INFORMATION and so misses dwm and every elevated or
//! system process when Blink runs unelevated.
//!
//! Cost: one PerfQueryCounterData call per tick (~1 ms for ~1300 instances),
//! only while the Processes tab shows a GPU column.

use std::collections::HashMap;
use std::sync::Arc;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{HANDLE, LUID};
use windows::Win32::System::Performance::*;

/// "GPU Engine" counter set; counter 1 = Running Time (100 ns units).
const GPU_ENGINE: GUID = GUID::from_u128(0x978c167d_4764_4d9c_9824_14747351dc81);
const RUNNING_TIME: u32 = 1;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    pid: u32,
    luid: (u32, i32),
    eng: u32,
}

pub struct ProcGpu {
    q: HANDLE,
    buf: Vec<u64>,
    last: HashMap<Key, (u64, u32)>,
    last_t: i64,
    tick: u32,
    /// per adapter LUID: engine labels by engine index
    labels: Vec<(LUID, Vec<Arc<str>>)>,
    other: HashMap<String, Arc<str>>,
    name: String,
    out: HashMap<u32, (f32, Option<Arc<str>>)>,
}

impl Drop for ProcGpu {
    fn drop(&mut self) {
        unsafe {
            PerfCloseQueryHandle(self.q);
        }
    }
}

impl ProcGpu {
    pub fn open(labels: Vec<(LUID, Vec<Arc<str>>)>) -> Option<Self> {
        unsafe {
            let mut q = HANDLE::default();
            if PerfOpenQueryHandle(PCWSTR::null(), &mut q) != 0 {
                return None;
            }
            // one identifier: Running Time of every instance ("*")
            let mut id = [0u64; 6]; // 40-byte identifier + L"*" padded to 8
            let ci = &mut *(id.as_mut_ptr() as *mut PERF_COUNTER_IDENTIFIER);
            ci.CounterSetGuid = GPU_ENGINE;
            ci.Size = 48;
            ci.CounterId = RUNNING_TIME;
            ci.InstanceId = u32::MAX;
            let name = (id.as_mut_ptr() as *mut u8).add(std::mem::size_of::<PERF_COUNTER_IDENTIFIER>()) as *mut u16;
            *name = b'*' as u16;
            *name.add(1) = 0;
            if PerfAddCounters(q, ci, 48) != 0 || ci.Status != 0 {
                PerfCloseQueryHandle(q);
                return None;
            }
            Some(ProcGpu {
                q,
                buf: vec![0; 4096],
                last: HashMap::new(),
                last_t: 0,
                tick: 0,
                labels,
                other: HashMap::new(),
                name: String::new(),
                out: HashMap::new(),
            })
        }
    }

    fn query(&mut self) -> bool {
        unsafe {
            for _ in 0..3 {
                let mut need = 0u32;
                let cb = (self.buf.len() * 8) as u32;
                match PerfQueryCounterData(self.q, Some(self.buf.as_mut_ptr() as *mut PERF_DATA_HEADER), cb, &mut need) {
                    0 => return true,
                    ERROR_NOT_ENOUGH_MEMORY => self.buf.resize((need as usize + 16384) / 8, 0),
                    _ => return false,
                }
            }
            false
        }
    }

    /// Fill `gpu` / `gpu_engine` of every process from one counter read.
    pub fn sample(&mut self, procs: &mut [super::Proc]) {
        if !self.query() {
            return;
        }
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        let bytes = unsafe { std::slice::from_raw_parts(self.buf.as_ptr() as *const u8, self.buf.len() * 8) };
        let rd = |o: usize| -> u32 { bytes.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0) };
        let rd64 = |o: usize| -> u64 { bytes.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).unwrap_or(0) };
        let total = (rd(0) as usize).min(bytes.len());
        let now_t = rd64(16) as i64; // PerfTime100NSec
        let dt = if self.last_t > 0 { (now_t - self.last_t) as f64 } else { 0.0 };
        self.last_t = now_t;
        self.out.clear();

        let hdr = std::mem::size_of::<PERF_DATA_HEADER>();
        // counter block header, then PERF_MULTI_INSTANCES
        let ch = hdr;
        if total < ch + 24 || rd(ch) != 0 || rd(ch + 4) != PERF_MULTIPLE_INSTANCES.0 as u32 {
            return;
        }
        let n = rd(ch + 16 + 4) as usize;
        let mut off = ch + 16 + 8;
        for _ in 0..n {
            let size = rd(off) as usize;
            if size < 8 || off + size + 8 > total {
                break;
            }
            // instance name: UTF-16 from off+8, null-terminated
            self.name.clear();
            let mut i = off + 8;
            while i + 1 < off + size {
                let c = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
                if c == 0 {
                    break;
                }
                self.name.push(char::from_u32(c as u32).unwrap_or('?'));
                i += 2;
            }
            let cd = off + size;
            let cd_size = rd(cd + 4) as usize;
            let run = rd64(cd + 8);
            off = cd + cd_size.max(16);
            let Some((key, engtype)) = parse(&self.name) else { continue };
            let prev = self.last.insert(key, (run, tick));
            let Some((prev, _)) = prev else { continue };
            if dt <= 0.0 || run <= prev {
                continue;
            }
            let u = ((run - prev) as f64 / dt * 100.0) as f32;
            let e = self.out.entry(key.pid).or_insert((0.0, None));
            if u > e.0 {
                let label = self
                    .labels
                    .iter()
                    .find(|(l, _)| (l.LowPart, l.HighPart) == key.luid)
                    .and_then(|(_, v)| v.get(key.eng as usize).cloned());
                let label = label.unwrap_or_else(|| self.other.entry(engtype.to_string()).or_insert_with(|| Arc::from(engtype)).clone());
                *e = (u, Some(label));
            }
        }
        self.last.retain(|_, v| v.1 == tick);

        for p in procs.iter_mut() {
            if let Some((u, e)) = self.out.get(&p.pid) {
                p.gpu = u.clamp(0.0, 100.0);
                p.gpu_engine = if p.gpu >= 0.05 { e.clone() } else { None };
            }
        }
    }
}

/// `pid_1234_luid_0x00000000_0x0000D1C1_phys_0_eng_3_engtype_VideoDecode`
fn parse(s: &str) -> Option<(Key, &str)> {
    let mut it = s.split('_');
    let mut pid = None;
    let (mut hi, mut lo, mut eng) = (None, None, None);
    let mut engtype = "";
    while let Some(t) = it.next() {
        match t {
            "pid" => pid = it.next()?.parse::<u32>().ok(),
            "luid" => {
                hi = u32::from_str_radix(it.next()?.trim_start_matches("0x"), 16).ok();
                lo = u32::from_str_radix(it.next()?.trim_start_matches("0x"), 16).ok();
            }
            "eng" => eng = it.next()?.parse::<u32>().ok(),
            "engtype" => {
                // the rest of the name (may itself contain '_', e.g. "JPEG_Decode_1")
                let at = s.find("engtype_")? + 8;
                engtype = &s[at..];
                break;
            }
            _ => {}
        }
    }
    Some((Key { pid: pid?, luid: (lo?, hi? as i32), eng: eng? }, engtype))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_instance_names() {
        let (k, t) = parse("pid_1234_luid_0x00000000_0x0000D1C1_phys_0_eng_3_engtype_VideoDecode").unwrap();
        assert_eq!(k.pid, 1234);
        assert_eq!(k.luid, (0xD1C1, 0));
        assert_eq!(k.eng, 3);
        assert_eq!(t, "VideoDecode");
        let (k, t) = parse("pid_8_luid_0x00000001_0x00012345_phys_0_eng_12_engtype_JPEG_Decode_1").unwrap();
        assert_eq!((k.pid, k.luid, k.eng, t), (8, (0x12345, 1), 12, "JPEG_Decode_1"));
        assert!(parse("garbage").is_none());
    }

    /// Cost of one per-process GPU read on this machine.
    /// `cargo test proc_gpu_bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn proc_gpu_bench() {
        let g = super::super::gpu::Gpus::load();
        let mut pg = ProcGpu::open(g.engine_labels()).expect("open");
        let mut procs: Vec<super::super::Proc> = Vec::new();
        let t = std::time::Instant::now();
        pg.sample(&mut procs);
        println!("first {:.2} ms, buf {} KB, {} instances", t.elapsed().as_secs_f64() * 1e3, pg.buf.len() * 8 / 1024, pg.last.len());
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let t = std::time::Instant::now();
            pg.sample(&mut procs);
            println!("pass {:.2} ms", t.elapsed().as_secs_f64() * 1e3);
        }
        let mut top: Vec<_> = pg.out.iter().filter(|(_, v)| v.0 > 0.0).map(|(p, v)| (*p, v.0, v.1.clone())).collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1));
        for t in top.iter().take(12) {
            println!("{t:?}");
        }
    }
}
