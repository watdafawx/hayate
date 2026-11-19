//! Signature check: WinVerifyTrust (embedded Authenticode, no network), then
//! Windows catalog fallback (most System32 files are catalog-signed).
//! Cached by (path, size, mtime). Called only from the Watch worker, which
//! runs in THREAD_MODE_BACKGROUND (low CPU + I/O priority).

use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Security::Cryptography::Catalog::*;
use windows::Win32::Security::Cryptography::*;
use windows::Win32::Security::WinTrust::*;
use windows::Win32::Storage::FileSystem::*;

use super::store::SigState;
use crate::osutil::{wide, Handle};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Sig {
    pub state: SigState,
    pub publisher: String,
}

/// File identity used as cache key: (size, last write FILETIME).
pub fn stat(path: &str) -> Option<(u64, u64)> {
    let w = wide(path);
    let mut d = WIN32_FILE_ATTRIBUTE_DATA::default();
    unsafe { GetFileAttributesExW(PCWSTR(w.as_ptr()), GetFileExInfoStandard, &mut d as *mut _ as *mut c_void).ok()? };
    let size = ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64;
    let mtime = ((d.ftLastWriteTime.dwHighDateTime as u64) << 32) | d.ftLastWriteTime.dwLowDateTime as u64;
    Some((size, mtime))
}

/// FileVersion from the version resource ("1.2.3.4"), empty if none.
pub fn file_version(path: &str) -> String {
    let w = wide(path);
    unsafe {
        let n = GetFileVersionInfoSizeW(PCWSTR(w.as_ptr()), None);
        if n == 0 {
            return String::new();
        }
        let mut buf = vec![0u8; n as usize];
        if GetFileVersionInfoW(PCWSTR(w.as_ptr()), None, n, buf.as_mut_ptr() as *mut c_void).is_err() {
            return String::new();
        }
        let mut p: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let root = wide("\\");
        if !VerQueryValueW(buf.as_ptr() as *const c_void, PCWSTR(root.as_ptr()), &mut p, &mut len).as_bool() || p.is_null() {
            return String::new();
        }
        let f = &*(p as *const VS_FIXEDFILEINFO);
        format!("{}.{}.{}.{}", f.dwFileVersionMS >> 16, f.dwFileVersionMS & 0xffff, f.dwFileVersionLS >> 16, f.dwFileVersionLS & 0xffff)
    }
}

unsafe fn publisher_of(state: HANDLE) -> String {
    let prov = WTHelperProvDataFromStateData(state);
    if prov.is_null() {
        return String::new();
    }
    let sgnr = WTHelperGetProvSignerFromChain(prov, 0, false, 0);
    if sgnr.is_null() || (*sgnr).csCertChain == 0 || (*sgnr).pasCertChain.is_null() {
        return String::new();
    }
    let cert = (*(*sgnr).pasCertChain).pCert;
    if cert.is_null() {
        return String::new();
    }
    let mut buf = [0u16; 256];
    let n = CertGetNameStringW(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, Some(&mut buf));
    if n <= 1 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize - 1])
}

const TRUST_E_NOSIGNATURE: i32 = 0x800B0100u32 as i32;
const TRUST_E_SUBJECT_FORM_UNKNOWN: i32 = 0x800B0003u32 as i32;
const TRUST_E_PROVIDER_UNKNOWN: i32 = 0x800B0001u32 as i32;

/// Run WinVerifyTrust on prepared data; returns (status, publisher).
unsafe fn verify(wd: &mut WINTRUST_DATA) -> (i32, String) {
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let hwnd = HWND(INVALID_HANDLE_VALUE.0);
    wd.dwStateAction = WTD_STATEACTION_VERIFY;
    let r = WinVerifyTrust(hwnd, &mut action, wd as *mut _ as *mut c_void);
    let p = if wd.hWVTStateData.is_invalid() || wd.hWVTStateData.0.is_null() { String::new() } else { publisher_of(wd.hWVTStateData) };
    wd.dwStateAction = WTD_STATEACTION_CLOSE;
    let _ = WinVerifyTrust(hwnd, &mut action, wd as *mut _ as *mut c_void);
    (r, p)
}

fn base_data() -> WINTRUST_DATA {
    WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_SAFER_FLAG,
        ..Default::default()
    }
}

fn check_embedded(path: &[u16]) -> (i32, String) {
    unsafe {
        let mut fi = WINTRUST_FILE_INFO { cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32, pcwszFilePath: PCWSTR(path.as_ptr()), ..Default::default() };
        let mut wd = base_data();
        wd.dwUnionChoice = WTD_CHOICE_FILE;
        wd.Anonymous.pFile = &mut fi;
        verify(&mut wd)
    }
}

/// Catalog lookup with one hash algorithm (None = SHA1 default).
fn check_catalog_alg(path: &[u16], file: HANDLE, alg: Option<&str>) -> Option<(i32, String)> {
    unsafe {
        let mut admin: isize = 0;
        let algw = alg.map(wide);
        CryptCATAdminAcquireContext2(&mut admin, Some(&DRIVER_ACTION_VERIFY), algw.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr())), None, None).ok()?;
        let mut out = None;
        let mut cb = 0u32;
        let _ = CryptCATAdminCalcHashFromFileHandle2(admin, file, &mut cb, None, None);
        if cb > 0 && cb <= 64 {
            let mut hash = vec![0u8; cb as usize];
            if CryptCATAdminCalcHashFromFileHandle2(admin, file, &mut cb, Some(hash.as_mut_ptr()), None).is_ok() {
                let cat = CryptCATAdminEnumCatalogFromHash(admin, &hash, None, None);
                if cat != 0 {
                    let mut ci = CATALOG_INFO { cbStruct: std::mem::size_of::<CATALOG_INFO>() as u32, ..Default::default() };
                    if CryptCATCatalogInfoFromContext(cat, &mut ci, 0).is_ok() {
                        let tag = wide(&hash.iter().map(|b| format!("{:02X}", b)).collect::<String>());
                        let mut wci = WINTRUST_CATALOG_INFO {
                            cbStruct: std::mem::size_of::<WINTRUST_CATALOG_INFO>() as u32,
                            pcwszCatalogFilePath: PCWSTR(ci.wszCatalogFile.as_ptr()),
                            pcwszMemberTag: PCWSTR(tag.as_ptr()),
                            pcwszMemberFilePath: PCWSTR(path.as_ptr()),
                            hMemberFile: file,
                            pbCalculatedFileHash: hash.as_mut_ptr(),
                            cbCalculatedFileHash: cb,
                            hCatAdmin: admin,
                            ..Default::default()
                        };
                        let mut wd = base_data();
                        wd.dwUnionChoice = WTD_CHOICE_CATALOG;
                        wd.Anonymous.pCatalog = &mut wci;
                        out = Some(verify(&mut wd));
                    }
                    let _ = CryptCATAdminReleaseCatalogContext(admin, cat, 0);
                }
            }
        }
        let _ = CryptCATAdminReleaseContext(admin, 0);
        out
    }
}

/// Full check (blocking, ms to tens of ms for big files).
pub fn check(path: &str) -> Sig {
    if stat(path).is_none() {
        return Sig::default();
    }
    let w = wide(path);
    let (r, publisher) = check_embedded(&w);
    if r == 0 {
        return Sig { state: SigState::Signed, publisher };
    }
    if r != TRUST_E_NOSIGNATURE && r != TRUST_E_SUBJECT_FORM_UNKNOWN && r != TRUST_E_PROVIDER_UNKNOWN {
        return Sig { state: SigState::Invalid, publisher };
    }
    // no embedded signature: try the catalogs
    let file = unsafe {
        CreateFileW(PCWSTR(w.as_ptr()), FILE_GENERIC_READ.0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None)
    };
    let Ok(file) = file else { return Sig { state: SigState::Unknown, publisher: String::new() } };
    let file = Handle(file);
    for alg in [Some("SHA256"), None] {
        if let Some((r, p)) = check_catalog_alg(&w, file.0, alg) {
            return Sig { state: if r == 0 { SigState::Catalog } else { SigState::Invalid }, publisher: p };
        }
    }
    Sig { state: SigState::Unsigned, publisher: String::new() }
}

/// In-RAM cache keyed by lowercase path; entry valid while (size, mtime) match.
#[derive(Default)]
pub struct Cache {
    map: HashMap<String, ((u64, u64), Sig)>,
}

impl Cache {
    const MAX: usize = 4000;

    pub fn get(&mut self, path: &str) -> Sig {
        let Some(id) = stat(path) else { return Sig::default() };
        let key = path.to_lowercase();
        if let Some((k, s)) = self.map.get(&key) {
            if *k == id {
                return s.clone();
            }
        }
        let s = check(path);
        if self.map.len() >= Self::MAX {
            self.map.clear(); // rare; simpler than LRU
        }
        self.map.insert(key, (id, s.clone()));
        s
    }

    /// Seed from the persisted seen-app table.
    pub fn seed(&mut self, path: &str, id: (u64, u64), sig: Sig) {
        if sig.state != SigState::Unknown {
            self.map.insert(path.to_lowercase(), (id, sig));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs real files; fast enough to run always.
    #[test]
    fn os_files() {
        let root = std::env::var("SystemRoot").unwrap_or("C:\\Windows".into());
        let n = check(&format!("{}\\System32\\cmd.exe", root));
        assert!(n.state.is_trusted(), "{:?}", n);
        assert_eq!(check(&format!("{}\\no-such-file.exe", root)).state, SigState::Unknown);
        let k = check(&format!("{}\\System32\\kernel32.dll", root));
        assert!(k.state.is_trusted(), "{:?}", k);
        assert!(k.publisher.contains("Microsoft"), "{:?}", k);
        // a text file is not signable → unsigned (or catalog-miss)
        let tmp = std::env::temp_dir().join("blink-sig-test.txt");
        std::fs::write(&tmp, b"hello").unwrap();
        assert_eq!(check(tmp.to_str().unwrap()).state, SigState::Unsigned);
        assert!(!file_version(&format!("{}\\System32\\kernel32.dll", root)).is_empty());
    }
}
