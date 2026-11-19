//! Power plans: list, read active, switch.

use windows::core::GUID;
use windows::Win32::Foundation::*;
use windows::Win32::System::Power::*;

#[derive(Clone)]
pub struct Plan {
    pub guid: GUID,
    pub name: String,
}

pub fn plans() -> Vec<Plan> {
    let mut out = Vec::new();
    for i in 0..32u32 {
        let mut g = GUID::zeroed();
        let mut size = std::mem::size_of::<GUID>() as u32;
        let r = unsafe { PowerEnumerate(None, None, None, ACCESS_SCHEME, i, Some(&mut g as *mut GUID as *mut u8), &mut size) };
        if r != ERROR_SUCCESS {
            break;
        }
        let mut buf = [0u16; 128];
        let mut bsize = (buf.len() * 2) as u32;
        let name = unsafe {
            if PowerReadFriendlyName(None, Some(&g), None, None, Some(buf.as_mut_ptr() as *mut u8), &mut bsize) == ERROR_SUCCESS {
                let n = (bsize as usize / 2).saturating_sub(1);
                String::from_utf16_lossy(&buf[..n])
            } else {
                "Plan".into()
            }
        };
        out.push(Plan { guid: g, name });
    }
    // usual order: saver, balanced, high, ultimate, then custom
    let rank = |n: &str| {
        let n = n.to_lowercase();
        if n.contains("saver") { 0 } else if n.contains("balanced") { 1 } else if n.contains("high") { 2 } else if n.contains("ultimate") { 3 } else { 4 }
    };
    out.sort_by_key(|p| rank(&p.name));
    out
}

pub fn active() -> Option<GUID> {
    unsafe {
        let mut p: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut p) != ERROR_SUCCESS || p.is_null() {
            return None;
        }
        let g = *p;
        let _ = LocalFree(Some(HLOCAL(p as *mut _)));
        Some(g)
    }
}

pub fn set(g: &GUID) -> bool {
    unsafe { PowerSetActiveScheme(None, Some(g)) == ERROR_SUCCESS }
}

/// Plan dot colour by name.
pub fn color(name: &str) -> u32 {
    let n = name.to_lowercase();
    if n.contains("saver") {
        0x5ee38a
    } else if n.contains("balanced") {
        0x5aa2ff
    } else if n.contains("ultimate") {
        0xff5d5d
    } else if n.contains("high") {
        0xffb347
    } else {
        0xa78bfa
    }
}
