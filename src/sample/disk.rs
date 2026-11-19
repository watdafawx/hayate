//! Physical disks: throughput and busy time from IOCTL_DISK_PERFORMANCE,
//! model from the storage descriptor, temperature from the storage
//! temperature property (works for NVMe and most SATA drives).
//!
//! Every IOCTL is overlapped and bounded: a drive that is asleep, spinning
//! up, 100 % busy or dead never holds a thread for long. A call that does not
//! complete in time is cancelled (CancelIoEx); if the driver will not even
//! cancel, its buffers are parked (not freed) and that drive is skipped until
//! the call finally completes, while the other drives keep updating.
//! Temperatures go to the drive itself, so they run on the background I/O
//! worker (`bgio`); the sampler only reads the cached result.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};

use crate::bgio;

const IOCTL_DISK_PERFORMANCE: u32 = 0x0007_0020;
const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;
/// FILE_ANY_ACCESS, so it works on the unelevated handle where LENGTH_INFO is denied.
const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x0007_00A0;

/// How long the worker waits for a temperature before cancelling it.
const TEMP_WAIT_MS: u32 = 2000;
/// Enumeration-time queries (descriptor, seek penalty, size).
const ENUM_WAIT_MS: u32 = 300;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DiskPerf {
    bytes_read: i64,
    bytes_written: i64,
    read_time: i64,
    write_time: i64,
    idle_time: i64,
    read_count: u32,
    write_count: u32,
    queue_depth: u32,
    split_count: u32,
    query_time: i64,
    device_number: u32,
    manager: [u16; 8],
}

/// One overlapped IOCTL. Boxed so the OVERLAPPED and buffers keep their
/// address for as long as the driver may still write to them.
struct Op {
    ov: OVERLAPPED,
    ev: HANDLE,
    inb: Vec<u8>,
    out: Vec<u8>,
}

impl Drop for Op {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.ev);
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum IoErr {
    /// an earlier call to this device has not completed yet
    Busy,
    /// did not complete within the wait (cancelled)
    TimedOut,
    Failed,
}

/// A device handle opened for overlapped I/O, with at most one parked call.
pub struct Probe {
    h: HANDLE,
    parked: Option<Box<Op>>,
}

// the handle and parked buffers are only touched by whichever thread owns the Probe
unsafe impl Send for Probe {}

impl Drop for Probe {
    fn drop(&mut self) {
        if let Some(op) = self.parked.take() {
            // the driver may still complete into it: never free those buffers
            std::mem::forget(op);
        }
        unsafe {
            let _ = CloseHandle(self.h);
        }
    }
}

impl Probe {
    pub fn open(path: &str) -> Option<Probe> {
        let w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        unsafe {
            CreateFileW(PCWSTR(w.as_ptr()), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAG_OVERLAPPED, None)
                .ok()
                .filter(|h| !h.is_invalid())
                .map(|h| Probe { h, parked: None })
        }
    }

    /// True while a call that did not finish in time is still outstanding.
    pub fn busy(&mut self) -> bool {
        if let Some(op) = &self.parked {
            if unsafe { WaitForSingleObject(op.ev, 0) } != WAIT_OBJECT_0 {
                return true;
            }
            self.parked = None;
        }
        false
    }

    /// Send `code`, wait up to `wait_ms` for the answer (0 = only take an
    /// answer the driver gives at once). On timeout the call is cancelled;
    /// `cancel_ms` is how long to wait for the cancel before parking it.
    pub fn ioctl(&mut self, code: u32, inb: &[u8], out_len: usize, wait_ms: u32, cancel_ms: u32) -> std::result::Result<Vec<u8>, IoErr> {
        if self.busy() {
            return Err(IoErr::Busy);
        }
        unsafe {
            let Ok(ev) = CreateEventW(None, true, false, PCWSTR::null()) else { return Err(IoErr::Failed) };
            let mut op = Box::new(Op { ov: OVERLAPPED::default(), ev, inb: inb.to_vec(), out: vec![0u8; out_len] });
            op.ov.hEvent = ev;
            let r = DeviceIoControl(
                self.h,
                code,
                if op.inb.is_empty() { None } else { Some(op.inb.as_ptr() as *const c_void) },
                op.inb.len() as u32,
                Some(op.out.as_mut_ptr() as *mut c_void),
                out_len as u32,
                None,
                Some(&mut op.ov),
            );
            let done = match r {
                Ok(()) => true,
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => WaitForSingleObject(ev, wait_ms) == WAIT_OBJECT_0,
                Err(_) => return Err(IoErr::Failed),
            };
            if !done {
                let _ = CancelIoEx(self.h, Some(&op.ov));
                if WaitForSingleObject(ev, cancel_ms) != WAIT_OBJECT_0 {
                    self.parked = Some(op);
                }
                return Err(IoErr::TimedOut);
            }
            let mut n = 0u32;
            if GetOverlappedResult(self.h, &op.ov, &mut n, false).is_err() {
                return Err(IoErr::Failed);
            }
            let mut out = std::mem::take(&mut op.out);
            out.truncate(n as usize);
            Ok(out)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Temp {
    pub c: f32,
    pub warn: Option<f32>,
}

pub struct Disk {
    pub num: u32,
    perf: Probe,
    pub model: String,
    pub bus: &'static str,
    /// seek penalty flag: Some(true) spinning, Some(false) flash, None not reported
    pub rotational: Option<bool>,
    /// firmware revision from the storage descriptor (empty: not reported)
    pub firmware: String,
    pub letters: String,
    pub size: u64,
    last: Option<DiskPerf>,
    pub read_bps: f64,
    pub write_bps: f64,
    pub active: f32,
    /// average time per completed I/O over the last interval, ms (None: no I/O)
    pub resp_ms: Option<f32>,
    /// I/Os outstanding at the last read
    pub queue: u32,
    /// holds the Windows volume / a page file
    pub system: bool,
    pub pagefile: bool,
    /// the worker's own handle for temperature queries
    tprobe: Arc<Mutex<Option<Probe>>>,
    pub temp: bgio::Cell<Temp>,
    /// per volume (letter, label, file system, free, total), from the worker
    pub vols: bgio::Cell<Vec<Volume>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    pub letter: char,
    pub label: String,
    pub fs: String,
    pub free: u64,
    pub total: u64,
}

/// Letters of `ExistingPageFiles` entries (`\??\C:\pagefile.sys`).
pub fn pagefile_letters(entries: &[String]) -> Vec<char> {
    entries
        .iter()
        .filter_map(|e| {
            let p = e.trim_start_matches("\\??\\");
            let mut c = p.chars();
            let l = c.next()?.to_ascii_uppercase();
            (l.is_ascii_alphabetic() && c.next() == Some(':')).then_some(l)
        })
        .collect()
}

fn page_files() -> Vec<char> {
    let mut buf = vec![0u16; 2048];
    let mut len = (buf.len() * 2) as u32;
    let r = unsafe {
        windows::Win32::System::Registry::RegGetValueW(
            windows::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            w!("SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management"),
            w!("ExistingPageFiles"),
            windows::Win32::System::Registry::RRF_RT_REG_MULTI_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        )
    };
    if r.is_err() {
        return Vec::new();
    }
    let s = String::from_utf16_lossy(&buf[..len as usize / 2]);
    pagefile_letters(&s.split('\0').filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>())
}

fn system_letter() -> Option<char> {
    let mut b = [0u16; 260];
    let n = unsafe { windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW(Some(&mut b)) } as usize;
    String::from_utf16_lossy(&b[..n.min(b.len())]).chars().next().map(|c| c.to_ascii_uppercase())
}

/// Free space, label and file system of each letter. Runs on the worker:
/// a volume that is busy or spinning up may take a while to answer.
fn read_volumes(letters: &str) -> Option<Vec<Volume>> {
    let mut out = Vec::new();
    for l in letters.chars().filter(|c| c.is_ascii_alphabetic()) {
        let root: Vec<u16> = format!("{l}:\\").encode_utf16().chain(Some(0)).collect();
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(PCWSTR(root.as_ptr()), None, Some(&mut total), Some(&mut free)) }.is_err() {
            continue;
        }
        let (mut name, mut fsn) = ([0u16; 64], [0u16; 32]);
        let _ = unsafe { GetVolumeInformationW(PCWSTR(root.as_ptr()), Some(&mut name), None, None, None, Some(&mut fsn)) };
        let z = |a: &[u16]| String::from_utf16_lossy(&a[..a.iter().position(|&c| c == 0).unwrap_or(a.len())]);
        out.push(Volume { letter: l, label: z(&name), fs: z(&fsn), free, total });
    }
    Some(out)
}

/// Ask the worker for every disk's volume free space, at most every `every`.
pub fn request_volumes(disks: &[Disk], every: Duration) {
    for d in disks.iter().filter(|d| !d.letters.is_empty()) {
        let letters = d.letters.clone();
        d.vols.refresh("volume space", every, move || read_volumes(&letters));
    }
}

/// STORAGE_PROPERTY_QUERY { PropertyId, QueryType=0 (standard), AdditionalParameters[1] }
fn prop_query(id: u32) -> [u8; 12] {
    let mut q = [0u8; 12];
    q[..4].copy_from_slice(&id.to_le_bytes());
    q
}

fn descriptor(p: &mut Probe) -> (String, &'static str, String) {
    let Ok(out) = p.ioctl(IOCTL_STORAGE_QUERY_PROPERTY, &prop_query(0), 1024, ENUM_WAIT_MS, 50) else { return (String::new(), "", String::new()) };
    if out.len() < 32 {
        return (String::new(), "", String::new());
    }
    let rd = |off: usize| u32::from_le_bytes([out[off], out[off + 1], out[off + 2], out[off + 3]]) as usize;
    let cstr = |off: usize| -> String {
        if off == 0 || off >= out.len() {
            return String::new();
        }
        let end = out[off..].iter().position(|&b| b == 0).map(|p| off + p).unwrap_or(out.len());
        String::from_utf8_lossy(&out[off..end]).trim().to_string()
    };
    // STORAGE_DEVICE_DESCRIPTOR: VendorIdOffset @12, ProductIdOffset @16, ProductRevisionOffset @20, BusType @28
    let vendor = cstr(rd(12));
    let product = cstr(rd(16));
    let firmware = crate::sample::hw::clean_text(cstr(rd(20)).as_bytes(), 16);
    let bus = match rd(28) as u8 {
        0x11 => "NVMe",
        0x0B => "SATA",
        0x07 => "USB",
        0x0A => "SAS",
        _ => "Disk",
    };
    let model = if vendor.is_empty() || product.starts_with(&vendor) { product } else { format!("{vendor} {product}") };
    (model, bus, firmware)
}

/// STORAGE_TEMPERATURE_DATA_DESCRIPTOR → (temperature, warning threshold).
fn parse_temp(out: &[u8]) -> Option<Temp> {
    if out.len() < 28 {
        return None;
    }
    let count = u16::from_le_bytes([out[12], out[13]]);
    if count == 0 {
        return None;
    }
    let warn = i16::from_le_bytes([out[10], out[11]]);
    let t = i16::from_le_bytes([out[26], out[27]]);
    let ok = |v: i16| (v > -40 && v < 150).then_some(v as f32);
    Some(Temp { c: ok(t)?, warn: ok(warn).filter(|w| *w > 0.0) })
}

/// One temperature query, on the worker. `None` when the drive has no
/// sensor, the query failed, timed out, or an earlier one is still stuck.
fn read_temp(p: &mut Probe) -> Option<Temp> {
    bgio::fake_stall("disk");
    // StorageDeviceTemperatureProperty = 52
    p.ioctl(IOCTL_STORAGE_QUERY_PROPERTY, &prop_query(52), 256, TEMP_WAIT_MS, 200).ok().and_then(|o| parse_temp(&o))
}

/// DEVICE_SEEK_PENALTY_DESCRIPTOR.IncursSeekPenalty: true for rotational drives.
fn seek_penalty(p: &mut Probe) -> Option<bool> {
    // StorageDeviceSeekPenaltyProperty = 7; { Version u32, Size u32, IncursSeekPenalty u8 }
    let out = p.ioctl(IOCTL_STORAGE_QUERY_PROPERTY, &prop_query(7), 12, ENUM_WAIT_MS, 50).ok()?;
    if out.len() < 9 {
        return None;
    }
    let size = u32::from_le_bytes([out[4], out[5], out[6], out[7]]);
    (size >= 9).then_some(out[8] != 0)
}

pub fn is_rotational(bus: &str) -> bool {
    bus.ends_with("HDD")
}

/// True for a drive letter that is a network (or otherwise redirected)
/// mapping: opening `\\.\X:` for one can wait on the network.
fn remote_letter(letter: char) -> bool {
    let name: Vec<u16> = format!("{letter}:").encode_utf16().chain(Some(0)).collect();
    let mut buf = [0u16; 512];
    let n = unsafe { QueryDosDeviceW(PCWSTR(name.as_ptr()), Some(&mut buf)) } as usize;
    let target = String::from_utf16_lossy(&buf[..n.min(buf.len())]).to_ascii_lowercase();
    // \Device\HarddiskVolumeN and friends are local; Mup / LanmanRedirector / WebDav / RDP are not
    n == 0 || target.contains("\\device\\mup") || target.contains("redirector") || target.contains("\\??\\unc") || target.starts_with("\\??\\")
}

pub fn enumerate() -> Vec<Disk> {
    let mut disks = Vec::new();
    for n in 0..24u32 {
        let path = format!("\\\\.\\PhysicalDrive{n}");
        let Some(mut p) = Probe::open(&path) else { continue };
        let (model, bus, firmware) = descriptor(&mut p);
        // SATA carries both SSDs and spinning disks: the seek-penalty flag
        // tells them apart (one query per disk, at enumeration only).
        let rotational = seek_penalty(&mut p);
        let bus = match (bus, rotational) {
            ("SATA", Some(true)) => "SATA HDD",
            ("SATA", Some(false)) => "SATA SSD",
            ("SAS", Some(true)) => "SAS HDD",
            ("USB", Some(true)) => "USB HDD",
            (b, _) => b,
        };
        let size = p
            .ioctl(IOCTL_DISK_GET_LENGTH_INFO, &[], 8, ENUM_WAIT_MS, 50)
            .ok()
            .filter(|o| o.len() >= 8)
            .map(|o| i64::from_le_bytes(o[..8].try_into().unwrap()) as u64)
            .or_else(|| {
                // DISK_GEOMETRY_EX: 24-byte DISK_GEOMETRY, then DiskSize
                p.ioctl(IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], 256, ENUM_WAIT_MS, 50)
                    .ok()
                    .filter(|o| o.len() >= 32)
                    .map(|o| i64::from_le_bytes(o[24..32].try_into().unwrap()) as u64)
            })
            .unwrap_or(0);
        // no temperature here: on a spinning disk that query waits for the drive
        // (~0.6 s from idle, seconds from standby); the worker reads it later
        disks.push(Disk {
            num: n,
            perf: p,
            model,
            bus,
            rotational,
            firmware,
            letters: String::new(),
            size,
            last: None,
            read_bps: 0.0,
            write_bps: 0.0,
            active: 0.0,
            resp_ms: None,
            queue: 0,
            system: false,
            pagefile: false,
            tprobe: Arc::new(Mutex::new(None)),
            temp: bgio::Cell::default(),
            vols: bgio::Cell::default(),
        });
    }
    // map drive letters to disk numbers (local volumes only)
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        if remote_letter(letter) {
            continue;
        }
        let Some(mut v) = Probe::open(&format!("\\\\.\\{letter}:")) else { continue };
        // VOLUME_DISK_EXTENTS { NumberOfDiskExtents u32, pad, DISK_EXTENT { DiskNumber u32, pad, Start i64, Len i64 } }
        if let Ok(out) = v.ioctl(IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], 256, ENUM_WAIT_MS, 50) {
            if out.len() >= 12 && u32::from_le_bytes([out[0], out[1], out[2], out[3]]) >= 1 {
                let dn = u32::from_le_bytes([out[8], out[9], out[10], out[11]]);
                if let Some(d) = disks.iter_mut().find(|d| d.num == dn) {
                    if !d.letters.is_empty() {
                        d.letters.push(' ');
                    }
                    d.letters.push(letter);
                    d.letters.push(':');
                }
            }
        }
    }
    disks.sort_by_key(|d| (d.letters.is_empty(), d.letters.clone(), d.num));
    let (sys, pf) = (system_letter(), page_files());
    for d in disks.iter_mut() {
        let has = |c: char| d.letters.chars().any(|l| l == c);
        d.system = sys.is_some_and(has);
        d.pagefile = pf.iter().any(|&c| has(c));
    }
    disks
}

/// Throughput and busy time: in-memory counters, never touches the drive.
/// Only an answer the driver gives at once is taken; a disk whose driver
/// pends the call is skipped this tick (and until that call completes).
pub fn update(disks: &mut [Disk]) {
    for d in disks {
        let Ok(out) = d.perf.ioctl(IOCTL_DISK_PERFORMANCE, &[], std::mem::size_of::<DiskPerf>(), 0, 0) else { continue };
        if out.len() < std::mem::size_of::<DiskPerf>() {
            continue;
        }
        let p: DiskPerf = unsafe { std::ptr::read_unaligned(out.as_ptr() as *const DiskPerf) };
        if let Some(l) = d.last {
            let dt = (p.query_time - l.query_time) as f64 / 1e7;
            if dt > 0.0 {
                d.read_bps = (p.bytes_read - l.bytes_read).max(0) as f64 / dt;
                d.write_bps = (p.bytes_written - l.bytes_written).max(0) as f64 / dt;
                let idle = (p.idle_time - l.idle_time) as f64 / (p.query_time - l.query_time) as f64;
                d.active = ((1.0 - idle) * 100.0).clamp(0.0, 100.0) as f32;
                d.resp_ms = response_ms(&l, &p);
            }
        }
        d.queue = p.queue_depth;
        d.last = Some(p);
    }
}

/// Average time per I/O completed between two DISK_PERFORMANCE reads, ms.
/// Read/write times are summed per I/O in 100 ns; the u32 counts wrap.
fn response_ms(a: &DiskPerf, b: &DiskPerf) -> Option<f32> {
    let n = b.read_count.wrapping_sub(a.read_count) as u64 + b.write_count.wrapping_sub(a.write_count) as u64;
    let t = (b.read_time - a.read_time) + (b.write_time - a.write_time);
    (n > 0 && t >= 0).then(|| (t as f64 / n as f64 / 1e4) as f32)
}

/// Ask the worker for drive temperatures at most every `every`, one job per
/// drive (a hung drive never holds up the others' next round). The query goes
/// to the drive itself, so a spinning disk is only asked while it is busy
/// anyway: an idle or spun-down HDD would otherwise be kept from spinning down.
pub fn request_temps(disks: &[Disk], every: Duration) {
    for d in disks {
        if is_rotational(d.bus) && d.read_bps + d.write_bps <= 0.0 {
            continue;
        }
        let (p, num) = (d.tprobe.clone(), d.num);
        d.temp.refresh("disk temperature", every, move || {
            let mut g = p.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_none() {
                *g = Probe::open(&format!("\\\\.\\PhysicalDrive{num}"));
            }
            read_temp(g.as_mut()?)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_temperature_descriptor() {
        let mut b = [0u8; 64];
        b[10..12].copy_from_slice(&70i16.to_le_bytes());
        b[12..14].copy_from_slice(&1u16.to_le_bytes());
        b[26..28].copy_from_slice(&41i16.to_le_bytes());
        assert_eq!(parse_temp(&b), Some(Temp { c: 41.0, warn: Some(70.0) }));
        b[12] = 0;
        assert_eq!(parse_temp(&b), None, "no sensors");
        assert_eq!(parse_temp(&b[..20]), None, "short buffer");
    }

    #[test]
    fn response_time_per_io() {
        let a = DiskPerf { read_count: u32::MAX - 1, write_count: 10, read_time: 1_000, write_time: 0, ..Default::default() };
        // 2 reads (wrapping) + 2 writes, 80_000 x 100 ns total = 8 ms over 4 I/Os
        let b = DiskPerf { read_count: 0, write_count: 12, read_time: 61_000, write_time: 20_000, ..Default::default() };
        assert_eq!(response_ms(&a, &b), Some(2.0));
        assert_eq!(response_ms(&b, &b), None, "no I/O: no response time");
    }

    #[test]
    fn page_file_letters() {
        let e = ["\\??\\C:\\pagefile.sys", "\\??\\e:\\pagefile.sys", "junk", ""].map(String::from);
        assert_eq!(pagefile_letters(&e), vec!['C', 'E']);
    }

    #[test]
    fn missing_device_does_not_open() {
        assert!(Probe::open("\\\\.\\PhysicalDrive99").is_none());
    }
}
