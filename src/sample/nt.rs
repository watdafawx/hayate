//! Raw ntdll calls: one NtQuerySystemInformation per tick gives every process
//! with CPU times, memory and I/O counters (same source System Informer uses).

use std::ffi::c_void;

#[link(name = "ntdll")]
extern "system" {
    pub fn NtQuerySystemInformation(class: u32, buf: *mut c_void, len: u32, ret: *mut u32) -> i32;
    pub fn NtQueryInformationProcess(h: *mut c_void, class: u32, buf: *mut c_void, len: u32, ret: *mut u32) -> i32;
}

pub const SYSTEM_PROCESS_INFORMATION: u32 = 5;
pub const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;
pub const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC0000004u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC0000023u32 as i32;
const STATUS_BUFFER_OVERFLOW: i32 = 0x80000005u32 as i32;

#[repr(C)]
pub struct UStr {
    pub len: u16,
    pub max: u16,
    pub buf: *const u16,
}

#[repr(C)]
pub struct Spi {
    pub next: u32,
    pub nthreads: u32,
    pub ws_private: i64,
    pub hard_faults: u32,
    pub nthreads_hw: u32,
    pub cycle: u64,
    pub create: i64,
    pub user: i64,
    pub kernel: i64,
    pub name: UStr,
    pub base_pri: i32,
    pub pid: usize,
    pub ppid: usize,
    pub handles: u32,
    pub session: u32,
    pub key: usize,
    pub peak_vsize: usize,
    pub vsize: usize,
    pub page_faults: u32,
    pub peak_ws: usize,
    pub ws: usize,
    pub qpp_peak: usize,
    pub qpp: usize,
    pub qnp_peak: usize,
    pub qnp: usize,
    pub pagefile: usize,
    pub peak_pagefile: usize,
    pub private_pages: usize,
    pub read_ops: i64,
    pub write_ops: i64,
    pub other_ops: i64,
    pub read_bytes: i64,
    pub write_bytes: i64,
    pub other_bytes: i64,
}

/// SystemBasicProcessInformation (Windows 11 25H2+): pid, parent, sequence
/// number and image name only, no per-thread records. Two orders of
/// magnitude cheaper than SystemProcessInformation (test machine, 350 processes:
/// 26 us vs 6.5 ms; dev PC, 700 processes: 37 us vs 13 ms), so a process list
/// that needs no counters uses this when the kernel knows the class.
pub const SYSTEM_BASIC_PROCESS_INFORMATION: u32 = 252;

#[repr(C)]
pub struct Bpi {
    pub next: u32,
    pub pid: usize,
    pub ppid: usize,
    /// unique per process for the boot (pid reuse gets a new one)
    pub seq: u64,
    pub name: UStr,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CpuTimes {
    pub idle: i64,
    pub kernel: i64,
    pub user: i64,
    pub dpc: i64,
    pub intr: i64,
    pub count: u32,
}

/// Calls NtQuerySystemInformation into a reusable buffer, growing it as needed.
pub fn query(class: u32, buf: &mut Vec<u64>) -> bool {
    loop {
        let bytes = (buf.len() * 8) as u32;
        let mut ret = 0u32;
        let st = unsafe { NtQuerySystemInformation(class, buf.as_mut_ptr() as *mut c_void, bytes, &mut ret) };
        if st >= 0 {
            return true;
        }
        if st == STATUS_INFO_LENGTH_MISMATCH || st == STATUS_BUFFER_TOO_SMALL {
            let want = (ret as usize).max(bytes as usize * 2) + 64 * 1024;
            buf.resize(want / 8 + 1, 0);
            continue;
        }
        return false;
    }
}

pub fn cpu_times(n: usize, buf: &mut Vec<u64>) -> Vec<CpuTimes> {
    let sz = std::mem::size_of::<CpuTimes>();
    if buf.len() * 8 < n * sz {
        buf.resize((n * sz) / 8 + 1, 0);
    }
    let mut ret = 0u32;
    let st = unsafe {
        NtQuerySystemInformation(SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION, buf.as_mut_ptr() as *mut c_void, (n * sz) as u32, &mut ret)
    };
    if st < 0 {
        return vec![CpuTimes::default(); n];
    }
    let p = buf.as_ptr() as *const CpuTimes;
    (0..n).map(|i| unsafe { *p.add(i) }).collect()
}

/// Command line through ProcessCommandLineInformation (needs only limited query access).
pub fn command_line(h: *mut c_void) -> Option<String> {
    let mut buf = vec![0u64; 128];
    loop {
        let mut ret = 0u32;
        let st = unsafe {
            NtQueryInformationProcess(h, PROCESS_COMMAND_LINE_INFORMATION, buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut ret)
        };
        if st >= 0 {
            let us = unsafe { &*(buf.as_ptr() as *const UStr) };
            if us.buf.is_null() || us.len == 0 {
                return None;
            }
            let s = unsafe { std::slice::from_raw_parts(us.buf, us.len as usize / 2) };
            return Some(String::from_utf16_lossy(s));
        }
        if (st == STATUS_INFO_LENGTH_MISMATCH || st == STATUS_BUFFER_TOO_SMALL || st == STATUS_BUFFER_OVERFLOW) && ret as usize > buf.len() * 8 && ret < 1 << 20 {
            buf.resize(ret as usize / 8 + 2, 0);
            continue;
        }
        return None;
    }
}
