//! Camera / microphone / location use from the CapabilityAccessManager
//! ConsentStore (HKCU + HKLM). LastUsedTimeStop == 0 (or < Start) = in use.

use std::collections::HashMap;

use windows::core::PWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::System::Registry::*;

use crate::osutil::{from_wide, pcw, wide};

pub const ROOT: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\CapabilityAccessManager\\ConsentStore";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cap {
    Camera,
    Microphone,
    Location,
}

impl Cap {
    pub const ALL: [Cap; 3] = [Cap::Camera, Cap::Microphone, Cap::Location];
    pub fn key(self) -> &'static str {
        match self {
            Cap::Camera => "webcam",
            Cap::Microphone => "microphone",
            Cap::Location => "location",
        }
    }
    pub fn noun(self) -> &'static str {
        match self {
            Cap::Camera => "camera",
            Cap::Microphone => "microphone",
            Cap::Location => "location",
        }
    }
}

/// Subkey path relative to the capability key → (start, stop) FILETIMEs.
pub type Snapshot = HashMap<String, (u64, u64)>;

#[derive(Clone, Debug, PartialEq)]
pub enum CapChange {
    Started { app: String },
    Stopped { app: String, start: u64, stop: u64 },
}

pub fn in_use(start: u64, stop: u64) -> bool {
    start != 0 && (stop == 0 || stop < start)
}

/// Pure diff between two reads of one capability tree.
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<CapChange> {
    let mut out = Vec::new();
    for (k, &(s, t)) in new {
        let prev = old.get(k).copied();
        if in_use(s, t) {
            let was = prev.map_or(false, |(ps, pt)| in_use(ps, pt) && ps == s);
            if !was {
                out.push(CapChange::Started { app: k.clone() });
            }
        } else if t != 0 && prev.map_or(true, |(_, pt)| pt != t) {
            out.push(CapChange::Stopped { app: k.clone(), start: s, stop: t });
        }
    }
    out.sort_by(|a, b| format!("{:?}", a).cmp(&format!("{:?}", b)));
    out
}

/// Subkey → (display name, exe path or package family).
/// `NonPackaged\C:#Program Files#Zen#zen.exe` → ("zen.exe", "C:\Program Files\Zen\zen.exe").
/// `Microsoft.WindowsCamera_8wekyb3d8bbwe` → ("Microsoft.WindowsCamera", same).
pub fn app_name(sub: &str) -> (String, String) {
    if let Some(p) = sub.strip_prefix("NonPackaged\\") {
        let path = p.replace('#', "\\");
        let name = crate::osutil::file_name(&path).to_string();
        return (name, path);
    }
    let name = sub.split('_').next().unwrap_or(sub).to_string();
    (name, sub.to_string())
}

fn read_qword(k: HKEY, name: &str) -> u64 {
    let w = wide(name);
    let mut v = 0u64;
    let mut n = 8u32;
    let mut ty = REG_VALUE_TYPE::default();
    let r = unsafe { RegQueryValueExW(k, pcw(&w), None, Some(&mut ty), Some(&mut v as *mut u64 as *mut u8), Some(&mut n)) };
    if r == ERROR_SUCCESS && ty == REG_QWORD {
        v
    } else {
        0
    }
}

fn subkeys(k: HKEY) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let mut buf = [0u16; 512];
        let mut n = buf.len() as u32;
        let r = unsafe { RegEnumKeyExW(k, i, Some(PWSTR(buf.as_mut_ptr())), &mut n, None, None, None, None) };
        if r != ERROR_SUCCESS {
            break;
        }
        out.push(from_wide(&buf[..n as usize]));
        i += 1;
    }
    out
}

fn open(root: HKEY, path: &str, sam: REG_SAM_FLAGS) -> Option<HKEY> {
    let w = wide(path);
    let mut k = HKEY::default();
    (unsafe { RegOpenKeyExW(root, pcw(&w), None, sam, &mut k) } == ERROR_SUCCESS).then_some(k)
}

/// Read one capability tree (`root` = opened capability key).
pub fn read(cap_key: HKEY) -> Snapshot {
    let mut snap = Snapshot::new();
    for s in subkeys(cap_key) {
        let Some(k) = open(cap_key, &s, KEY_READ) else { continue };
        if s.eq_ignore_ascii_case("NonPackaged") {
            for a in subkeys(k) {
                if let Some(ak) = open(k, &a, KEY_READ) {
                    snap.insert(format!("NonPackaged\\{}", a), (read_qword(ak, "LastUsedTimeStart"), read_qword(ak, "LastUsedTimeStop")));
                    unsafe {
                        let _ = RegCloseKey(ak);
                    }
                }
            }
        } else {
            snap.insert(s, (read_qword(k, "LastUsedTimeStart"), read_qword(k, "LastUsedTimeStop")));
        }
        unsafe {
            let _ = RegCloseKey(k);
        }
    }
    snap
}

/// One watched key: hive + capability, its notify event and last snapshot.
pub struct Watched {
    pub cap: Cap,
    pub machine: bool,
    pub key: HKEY,
    pub event: HANDLE,
    pub snap: Snapshot,
}

impl Watched {
    pub fn open(cap: Cap, machine: bool) -> Option<Watched> {
        let root = if machine { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER };
        let key = open(root, &format!("{}\\{}", ROOT, cap.key()), KEY_READ | KEY_NOTIFY)?;
        let event = unsafe { windows::Win32::System::Threading::CreateEventW(None, false, false, None).ok()? };
        let mut w = Watched { cap, machine, key, event, snap: Snapshot::new() };
        w.snap = read(key);
        w.arm();
        Some(w)
    }

    /// (Re-)arm the async notification. Must be re-armed after every signal.
    pub fn arm(&self) -> bool {
        unsafe { RegNotifyChangeKeyValue(self.key, true, REG_NOTIFY_CHANGE_NAME | REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_THREAD_AGNOSTIC, Some(self.event), true) == ERROR_SUCCESS }
    }

    /// After a signal: re-arm, re-read, diff.
    pub fn changed(&mut self) -> Vec<CapChange> {
        self.arm();
        let new = read(self.key);
        let d = diff(&self.snap, &new);
        self.snap = new;
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(v: &[(&str, u64, u64)]) -> Snapshot {
        v.iter().map(|(k, s, t)| (k.to_string(), (*s, *t))).collect()
    }

    #[test]
    fn diffs() {
        let a = snap(&[("NonPackaged\\C:#Zen#zen.exe", 100, 200), ("App_x", 50, 60)]);
        // zen starts using it
        let b = snap(&[("NonPackaged\\C:#Zen#zen.exe", 300, 0), ("App_x", 50, 60)]);
        assert_eq!(diff(&a, &b), vec![CapChange::Started { app: "NonPackaged\\C:#Zen#zen.exe".into() }]);
        // no change while in use (other value written)
        assert!(diff(&b, &b).is_empty());
        // stops
        let c = snap(&[("NonPackaged\\C:#Zen#zen.exe", 300, 420), ("App_x", 50, 60)]);
        assert_eq!(diff(&b, &c), vec![CapChange::Stopped { app: "NonPackaged\\C:#Zen#zen.exe".into(), start: 300, stop: 420 }]);
        // short use between two reads → just a stop
        let d = snap(&[("NonPackaged\\C:#Zen#zen.exe", 500, 510), ("App_x", 50, 60)]);
        assert_eq!(diff(&c, &d), vec![CapChange::Stopped { app: "NonPackaged\\C:#Zen#zen.exe".into(), start: 500, stop: 510 }]);
        // new app that never used it: nothing
        let e = snap(&[("New_y", 0, 0)]);
        assert!(diff(&Snapshot::new(), &e).is_empty());
    }

    #[test]
    fn names() {
        assert_eq!(app_name("NonPackaged\\C:#Program Files#Zen Browser#zen.exe"), ("zen.exe".into(), "C:\\Program Files\\Zen Browser\\zen.exe".into()));
        assert_eq!(app_name("Microsoft.WindowsCamera_8wekyb3d8bbwe"), ("Microsoft.WindowsCamera".into(), "Microsoft.WindowsCamera_8wekyb3d8bbwe".into()));
    }

    #[test]
    fn reads_live_tree() {
        // HKCU tree exists on any Win10 1903+ desktop; just make sure reading works
        if let Some(w) = Watched::open(Cap::Microphone, false) {
            for (k, (s, t)) in &w.snap {
                assert!(!k.is_empty());
                let _ = (s, t);
            }
        }
    }
}
