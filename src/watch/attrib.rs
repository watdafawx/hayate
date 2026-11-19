//! "Who installed / changed / removed this service?" v1 heuristic (spec §3.2):
//! 1. a recent process whose image folder contains the service binary;
//! 2. else the newest installer-like process (msiexec, setup*, *install*,
//!    DrvInst, unins*, ...) started in the last 60 s, shown with its parent.

use crate::etw::ProcStart;
use crate::osutil::{file_name, parent_dir};
use crate::rules::glob;

#[derive(Clone, Debug, PartialEq)]
pub struct Attribution {
    /// "GoogleUpdater.exe" or "Zen updater.exe → setup.exe"
    pub actor: String,
    /// image of the process that did it (for the signer lookup)
    pub actor_path: String,
    /// true when step 1 did not match (shown as "likely")
    pub likely: bool,
}

const WINDOW_MS: u64 = 60_000;

const INSTALLERS: &[&str] = &[
    "msiexec.exe",
    "trustedinstaller.exe",
    "tiworker.exe",
    "drvinst.exe",
    "pnputil.exe",
    "dism.exe",
    "sc.exe",
    "setup*.exe",
    "*setup.exe",
    "*install*.exe",
    "unins*.exe",
    "*uninst*.exe",
    "*updater*.exe",
    "*update.exe",
];

/// Parents not worth showing in the chain.
const BORING_PARENTS: &[&str] = &["services.exe", "wininit.exe", "explorer.exe", "userinit.exe", "smss.exe", "system"];

pub fn is_installer(name: &str) -> bool {
    INSTALLERS.iter().any(|p| glob::matches(p, name))
}

fn is_system_root_dir(dir: &str, sysroot: &str) -> bool {
    let d = dir.to_ascii_lowercase();
    let r = sysroot.to_ascii_lowercase();
    d == r || d == format!("{}\\system32", r) || d == format!("{}\\syswow64", r) || d.is_empty()
}

fn has_dir_prefix(path: &str, dir: &str) -> bool {
    path.len() > dir.len() && path[..dir.len()].eq_ignore_ascii_case(dir) && matches!(path.as_bytes()[dir.len()], b'\\' | b'/')
}

/// `starts` = recent process starts (any order). `parent_path` looks up a
/// pid that is not in the ring (still running, started long ago).
pub fn attribute(svc_path: &str, event_ms: u64, starts: &[ProcStart], sysroot: &str, parent_path: &dyn Fn(u32) -> Option<String>) -> Option<Attribution> {
    let recent: Vec<&ProcStart> = starts.iter().filter(|p| p.time <= event_ms + 2_000 && event_ms.saturating_sub(p.time) <= WINDOW_MS).collect();

    // step 1: image folder is a prefix of the service binary
    if !svc_path.is_empty() {
        let best = recent
            .iter()
            .filter(|p| {
                let d = parent_dir(&p.path);
                !is_system_root_dir(d, sysroot) && has_dir_prefix(svc_path, d)
            })
            .max_by_key(|p| p.time);
        if let Some(p) = best {
            return Some(Attribution { actor: file_name(&p.path).to_string(), actor_path: p.path.clone(), likely: false });
        }
    }

    // step 2: newest installer-like process, with its parent
    let p = recent.iter().filter(|p| is_installer(p.name())).max_by_key(|p| p.time)?;
    let mut chain = vec![file_name(&p.path).to_string()];
    let mut cur: ProcStart = (*p).clone();
    for _ in 0..2 {
        let parent = starts.iter().filter(|q| q.pid == cur.ppid && q.time <= cur.time).max_by_key(|q| q.time).cloned().or_else(|| {
            parent_path(cur.ppid).map(|path| ProcStart { pid: cur.ppid, ppid: 0, session: 0, path, time: 0 })
        });
        let Some(par) = parent else { break };
        let pn = file_name(&par.path).to_string();
        if BORING_PARENTS.iter().any(|b| b.eq_ignore_ascii_case(&pn)) {
            break;
        }
        // msiexec → msiexec (client → server) adds nothing
        if !chain[0].eq_ignore_ascii_case(&pn) {
            chain.insert(0, pn);
        }
        if !is_installer(par.name()) {
            break; // stop at the first human-meaningful app
        }
        cur = par;
    }
    Some(Attribution { actor: chain.join(" → "), actor_path: p.path.clone(), likely: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(pid: u32, ppid: u32, path: &str, time: u64) -> ProcStart {
        ProcStart { pid, ppid, session: 1, path: path.into(), time }
    }
    const ROOT: &str = "C:\\Windows";

    #[test]
    fn folder_prefix_wins() {
        let starts = vec![
            ps(10, 1, "C:\\Windows\\System32\\msiexec.exe", 99_000),
            ps(11, 1, "C:\\Program Files (x86)\\Google\\GoogleUpdater\\GoogleUpdater.exe", 95_000),
            ps(12, 1, "C:\\Windows\\System32\\svchost.exe", 99_500),
        ];
        let a = attribute("C:\\Program Files (x86)\\Google\\GoogleUpdater\\159.0.8140.0\\updater.exe", 100_000, &starts, ROOT, &|_| None).unwrap();
        assert_eq!(a, Attribution { actor: "GoogleUpdater.exe".into(), actor_path: starts[1].path.clone(), likely: false });
    }

    #[test]
    fn system32_tools_do_not_prefix_match_drivers() {
        // sc.exe lives in System32 and the driver too: must not be a "folder match"
        let starts = vec![ps(20, 5, "C:\\Windows\\System32\\sc.exe", 99_000)];
        let a = attribute("C:\\Windows\\System32\\drivers\\foo.sys", 100_000, &starts, ROOT, &|_| None).unwrap();
        assert!(a.likely);
        assert_eq!(a.actor, "sc.exe");
    }

    #[test]
    fn installer_with_parent_chain() {
        let starts = vec![
            ps(30, 4000, "C:\\Users\\me\\AppData\\Local\\Zen\\Zen updater.exe", 50_000),
            ps(31, 30, "C:\\Users\\me\\AppData\\Local\\Temp\\x\\setup.exe", 60_000),
            ps(32, 2, "C:\\Windows\\explorer.exe", 61_000),
        ];
        let a = attribute("C:\\Program Files\\ZenSvc\\zsvc.exe", 70_000, &starts, ROOT, &|_| None).unwrap();
        // "Zen updater.exe" is installer-like too, so walk continues to its parent (not in ring, unknown) and stops
        assert_eq!(a.actor, "Zen updater.exe → setup.exe");
        assert!(a.likely);
    }

    #[test]
    fn drvinst_from_svchost() {
        let starts = vec![ps(40, 900, "C:\\Windows\\System32\\DrvInst.exe", 10_000)];
        let a = attribute("C:\\Windows\\System32\\drivers\\nvx.sys", 12_000, &starts, ROOT, &|pid| (pid == 900).then(|| "C:\\Windows\\System32\\svchost.exe".to_string())).unwrap();
        assert_eq!(a.actor, "svchost.exe → DrvInst.exe");
    }

    #[test]
    fn nothing_recent() {
        let starts = vec![ps(50, 1, "C:\\Windows\\System32\\msiexec.exe", 1_000)];
        assert!(attribute("C:\\x\\y.exe", 200_000, &starts, ROOT, &|_| None).is_none());
        assert!(attribute("C:\\x\\y.exe", 200_000, &[ps(1, 1, "C:\\Tools\\notepad.exe", 199_000)], ROOT, &|_| None).is_none());
    }

    #[test]
    fn installer_names() {
        for n in ["msiexec.exe", "Setup.exe", "vc_redist_setup.exe", "NvInstaller.exe", "unins000.exe", "DrvInst.exe", "GoogleUpdater.exe"] {
            assert!(is_installer(n), "{}", n);
        }
        for n in ["notepad.exe", "chrome.exe", "svchost.exe"] {
            assert!(!is_installer(n), "{}", n);
        }
    }
}
