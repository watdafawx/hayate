//! Minimal registry access for the startup code: open / read raw values /
//! write one binary value / delete one value. Every call takes the root HKEY,
//! so the same code runs on the real HKCU/HKLM (production) and on a scratch
//! subtree under HKCU (tests; nothing of the user's real Run keys is touched).

use windows::core::PWSTR;
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::*;

use crate::osutil::{from_wide, pcw, wide};

/// Raw registry values larger than this are not read (a Run command is never near it).
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
/// Values listed from one key (bounds a hostile or runaway key).
pub const MAX_VALUES: u32 = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegErr(pub u32);

impl RegErr {
    pub fn denied(self) -> bool {
        self.0 == ERROR_ACCESS_DENIED.0
    }
    pub fn not_found(self) -> bool {
        self.0 == ERROR_FILE_NOT_FOUND.0 || self.0 == ERROR_PATH_NOT_FOUND.0
    }
    pub fn text(self) -> String {
        if self.denied() {
            "access denied".into()
        } else if self.not_found() {
            "not found".into()
        } else if self.0 == 13 {
            "the existing value is in a form Hayate does not recognise".into()
        } else {
            format!("Windows error {}", self.0)
        }
    }
}

fn ok(e: WIN32_ERROR) -> Result<(), RegErr> {
    if e == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(RegErr(e.0))
    }
}

/// An open key, closed on drop.
pub struct Key(pub HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Open `path` under `root`. `sam` e.g. `KEY_READ | KEY_WOW64_64KEY`.
pub fn open(root: HKEY, path: &str, sam: REG_SAM_FLAGS) -> Result<Key, RegErr> {
    let w = wide(path);
    let mut k = HKEY::default();
    ok(unsafe { RegOpenKeyExW(root, pcw(&w), None, sam, &mut k) })?;
    Ok(Key(k))
}

/// Open `path` for writing, creating it when absent (StartupApproved subkeys).
pub fn create(root: HKEY, path: &str, sam: REG_SAM_FLAGS) -> Result<Key, RegErr> {
    let w = wide(path);
    let mut k = HKEY::default();
    ok(unsafe { RegCreateKeyExW(root, pcw(&w), None, None, REG_OPTION_NON_VOLATILE, sam, None, &mut k, None) })?;
    Ok(Key(k))
}

/// One value: (registry type, raw bytes).
pub type Raw = (u32, Vec<u8>);

/// The raw value `name`, or `Ok(None)` when it does not exist.
pub fn get(key: &Key, name: &str) -> Result<Option<Raw>, RegErr> {
    let w = wide(name);
    let mut ty = REG_VALUE_TYPE(0);
    let mut n = 0u32;
    let r = unsafe { RegQueryValueExW(key.0, pcw(&w), None, Some(&mut ty), None, Some(&mut n)) };
    if r == ERROR_FILE_NOT_FOUND || r == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    ok(r)?;
    if n as usize > MAX_VALUE_BYTES {
        return Err(RegErr(ERROR_MORE_DATA.0));
    }
    let mut buf = vec![0u8; n as usize];
    let mut n2 = n;
    let r = unsafe { RegQueryValueExW(key.0, pcw(&w), None, Some(&mut ty), Some(buf.as_mut_ptr()), Some(&mut n2)) };
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    ok(r)?;
    buf.truncate(n2 as usize);
    Ok(Some((ty.0, buf)))
}

/// Write `data` as REG_BINARY.
pub fn set_binary(key: &Key, name: &str, data: &[u8]) -> Result<(), RegErr> {
    let w = wide(name);
    ok(unsafe { RegSetValueExW(key.0, pcw(&w), None, REG_BINARY, Some(data)) })
}

/// Delete one value; a value that is already gone is fine.
pub fn delete(key: &Key, name: &str) -> Result<(), RegErr> {
    let w = wide(name);
    let r = unsafe { RegDeleteValueW(key.0, pcw(&w)) };
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    ok(r)
}

/// Every value of the key (name, type, bytes). Oversized values are listed with empty data.
pub fn values(key: &Key) -> Result<Vec<(String, u32, Vec<u8>)>, RegErr> {
    let mut out = Vec::new();
    let mut name = vec![0u16; 16384];
    for i in 0..MAX_VALUES {
        let mut nlen = name.len() as u32;
        let r = unsafe { RegEnumValueW(key.0, i, Some(PWSTR(name.as_mut_ptr())), &mut nlen, None, None, None, None) };
        if r == ERROR_NO_MORE_ITEMS {
            break;
        }
        ok(r)?;
        let vname = from_wide(&name[..nlen as usize]);
        let (ty, data) = match get(key, &vname)? {
            Some(v) => v,
            None => continue,
        };
        out.push((vname, ty, data));
    }
    Ok(out)
}

/// UTF-16 text of a REG_SZ / REG_EXPAND_SZ value (None for any other type).
pub fn text(raw: &Raw) -> Option<String> {
    if raw.0 != REG_SZ.0 && raw.0 != REG_EXPAND_SZ.0 {
        return None;
    }
    let u: Vec<u16> = raw.1.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(from_wide(&u).trim().to_string())
}

#[cfg(test)]
pub mod scratch {
    //! A disposable registry subtree under HKCU used as a stand-in for HKCU and
    //! HKLM in tests. The subtree is named HayateStartupFixture so nothing else
    //! owns it, and it is deleted when the guard drops.
    use super::*;

    pub struct Scratch {
        pub base: String,
        pub user: Key,
        pub machine: Key,
    }

    impl Scratch {
        pub fn new(tag: &str) -> Scratch {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let base = format!(r"Software\HayateStartupFixture\{}-{}-{}", tag, std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            let user = create(HKEY_CURRENT_USER, &format!(r"{base}\user"), KEY_READ | KEY_WRITE).expect("scratch user root");
            let machine = create(HKEY_CURRENT_USER, &format!(r"{base}\machine"), KEY_READ | KEY_WRITE).expect("scratch machine root");
            Scratch { base, user, machine }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let w = wide(&self.base);
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, pcw(&w));
                // remove the parent when this was the last fixture
                let p = wide(r"Software\HayateStartupFixture");
                let _ = RegDeleteKeyW(HKEY_CURRENT_USER, pcw(&p));
            }
        }
    }
}
