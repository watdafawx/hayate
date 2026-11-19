//! Service cache (from the registry, no service handles held) and SCM
//! created/deleted notifications (NotifyServiceStatusChangeW on the SCM
//! handle; APC delivered to the alertable Watch notify thread).

use std::collections::HashMap;
use std::sync::Mutex;

use windows::core::PWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Services::*;

use crate::osutil::{self, from_wide, pcw, wide};

const SERVICES: &str = "SYSTEM\\CurrentControlSet\\Services";

#[derive(Clone, Debug, PartialEq, Default)]
pub struct SvcInfo {
    pub name: String,
    pub display: String,
    /// cleaned Win32 path
    pub path: String,
    /// raw `Type` (1 kernel driver, 2 fs driver, 0x10/0x20 own/shared process)
    pub kind: u32,
    /// raw `Start` (0 boot, 1 system, 2 auto, 3 demand, 4 disabled)
    pub start: u32,
}

impl SvcInfo {
    pub fn is_driver(&self) -> bool {
        self.kind & 0x0B != 0 && self.kind & 0x30 == 0
    }
}

pub fn start_label(s: u32) -> &'static str {
    match s {
        0 => "boot",
        1 => "system",
        2 => "auto",
        3 => "demand",
        4 => "disabled",
        _ => "?",
    }
}

fn reg_string(k: HKEY, name: &str) -> String {
    let w = wide(name);
    let mut buf = vec![0u16; 1024];
    let mut n = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(k, None, pcw(&w), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut n)) };
    if r == ERROR_SUCCESS {
        from_wide(&buf)
    } else {
        String::new()
    }
}

fn reg_dword(k: HKEY, name: &str) -> u32 {
    let w = wide(name);
    let mut v = 0u32;
    let mut n = 4u32;
    let r = unsafe { RegGetValueW(k, None, pcw(&w), RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut n)) };
    if r == ERROR_SUCCESS {
        v
    } else {
        u32::MAX
    }
}

/// Read one service from the registry (None if the key is gone).
pub fn read_service(name: &str) -> Option<SvcInfo> {
    let w = wide(&format!("{}\\{}", SERVICES, name));
    let mut k = HKEY::default();
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, pcw(&w), None, KEY_READ, &mut k) } != ERROR_SUCCESS {
        return None;
    }
    let raw = reg_string(k, "ImagePath");
    let mut display = reg_string(k, "DisplayName");
    if display.starts_with('@') {
        display = load_indirect(&display).unwrap_or_default();
    }
    let info = SvcInfo {
        name: name.to_string(),
        display: if display.is_empty() { name.to_string() } else { display },
        path: if raw.is_empty() { String::new() } else { osutil::clean_image_path(&raw) },
        kind: reg_dword(k, "Type"),
        start: reg_dword(k, "Start"),
    };
    unsafe {
        let _ = RegCloseKey(k);
    }
    // keys without Type are not services (driver parameter groups etc.)
    (info.kind != u32::MAX).then_some(info)
}

/// Resolve an `@dll,-id` string (service description / display name).
pub fn indirect_string(s: &str) -> Option<String> {
    load_indirect(s)
}

/// `@%SystemRoot%\system32\x.dll,-101` → string resource.
fn load_indirect(s: &str) -> Option<String> {
    use windows::Win32::UI::Shell::SHLoadIndirectString;
    let w = wide(s);
    let mut buf = [0u16; 512];
    unsafe { SHLoadIndirectString(pcw(&w), &mut buf, None).ok()? };
    Some(from_wide(&buf))
}

/// All services + drivers. ~700 registry reads, run once on the background worker.
pub fn read_all() -> HashMap<String, SvcInfo> {
    let mut out = HashMap::new();
    let w = wide(SERVICES);
    let mut k = HKEY::default();
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, pcw(&w), None, KEY_READ, &mut k) } != ERROR_SUCCESS {
        return out;
    }
    let mut i = 0;
    loop {
        let mut buf = [0u16; 260];
        let mut n = buf.len() as u32;
        if unsafe { RegEnumKeyExW(k, i, Some(PWSTR(buf.as_mut_ptr())), &mut n, None, None, None, None) } != ERROR_SUCCESS {
            break;
        }
        let name = from_wide(&buf[..n as usize]);
        if let Some(s) = read_service(&name) {
            out.insert(name.to_lowercase(), s);
        }
        i += 1;
    }
    unsafe {
        let _ = RegCloseKey(k);
    }
    out
}

/// Names from a SERVICE_NOTIFY pszServiceNames multi-sz: `/name` created, `\name` deleted.
pub fn split_names(multi: &[u16]) -> Vec<(bool, String)> {
    multi
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let n = String::from_utf16_lossy(s);
            match n.strip_prefix('/') {
                Some(r) => (true, r.to_string()),
                None => (false, n.strip_prefix('\\').unwrap_or(&n).to_string()),
            }
        })
        .collect()
}

// ---- SCM notify ---------------------------------------------------------------

/// Names collected by the APC; drained by the notify thread after each wake.
pub static PENDING: Mutex<Vec<(bool, String)>> = Mutex::new(Vec::new());

unsafe extern "system" fn on_notify(p: *const core::ffi::c_void) {
    let n = &*(p as *const SERVICE_NOTIFY_2W);
    if n.dwNotificationStatus == ERROR_SUCCESS.0 && !n.pszServiceNames.is_null() {
        let mut len = 0;
        let s = n.pszServiceNames.0;
        while !(*s.add(len) == 0 && *s.add(len + 1) == 0) {
            len += 1;
        }
        let v = std::slice::from_raw_parts(s, len + 1);
        PENDING.lock().unwrap().extend(split_names(v));
        let _ = LocalFree(Some(HLOCAL(s as *mut _)));
    }
}

/// Owns the SCM handle and the notify buffer (must stay put while armed).
pub struct Notifier {
    scm: SC_HANDLE,
    buf: Box<SERVICE_NOTIFY_2W>,
}

unsafe impl Send for Notifier {}

impl Notifier {
    pub fn open() -> Result<Notifier, String> {
        let scm = unsafe { OpenSCManagerW(None, None, SC_MANAGER_ENUMERATE_SERVICE) }.map_err(|e| format!("OpenSCManager: {}", e.message()))?;
        let buf = Box::new(SERVICE_NOTIFY_2W { dwVersion: SERVICE_NOTIFY_STATUS_CHANGE, pfnNotifyCallback: Some(on_notify), ..Default::default() });
        let n = Notifier { scm, buf };
        n.arm()?;
        Ok(n)
    }

    /// Arm (again). Call on the alertable thread after each APC.
    pub fn arm(&self) -> Result<(), String> {
        let r = unsafe { NotifyServiceStatusChangeW(self.scm, SERVICE_NOTIFY_CREATED | SERVICE_NOTIFY_DELETED, &*self.buf) };
        if r == ERROR_SUCCESS.0 {
            Ok(())
        } else {
            Err(format!("NotifyServiceStatusChange: {}", r))
        }
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.scm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let w: Vec<u16> = "/NewSvc\0\\OldDrv\0\0".encode_utf16().collect();
        assert_eq!(split_names(&w), vec![(true, "NewSvc".into()), (false, "OldDrv".into())]);
    }

    #[test]
    fn cache_reads_known_services() {
        let all = read_all();
        assert!(all.len() > 100, "{}", all.len());
        let s = all.get("eventlog").expect("EventLog service");
        assert!(!s.is_driver());
        assert!(s.path.to_lowercase().ends_with("svchost.exe"), "{}", s.path);
        assert!(all.values().any(|s| s.is_driver()));
        assert_eq!(read_service("NoSuchService_hayate_xyz"), None);
    }
}
