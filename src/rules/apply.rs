//! Apply rule actions to a pid, plus "while running" holds (power plan,
//! prevent sleep) that are released when the process exits.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use windows::core::{GUID, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Power::*;
use windows::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows::Win32::System::Threading::*;

use super::model::{format_cpus, format_guid, parse_guid, Actions, Awake, Priority};
use crate::osutil::{self, wide, Handle, OpenFail};

#[link(name = "ntdll")]
extern "system" {
    fn NtSetInformationProcess(h: *mut c_void, class: u32, info: *const c_void, len: u32) -> i32;
}
const PROCESS_IO_PRIORITY: u32 = 33;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    /// protected process: some actions refused
    pub protected: bool,
    pub gone: bool,
    pub text: String,
}

fn prio_flag(p: Priority) -> PROCESS_CREATION_FLAGS {
    match p {
        Priority::Idle => IDLE_PRIORITY_CLASS,
        Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        Priority::Normal => NORMAL_PRIORITY_CLASS,
        Priority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        Priority::High => HIGH_PRIORITY_CLASS,
        Priority::Realtime => REALTIME_PRIORITY_CLASS,
    }
}

/// Apply all actions once. Never retries. `reason` labels power requests
/// ("Blink: game.exe running"). `create` is the creation time the caller
/// identified the process by: every handle used here is opened against it
/// (`osutil::open_verified`), so a PID that was reused after the caller chose
/// its target is never touched. `create == 0` (unknown) applies nothing.
pub fn apply(pid: u32, create: i64, a: &Actions, reason: &str) -> Outcome {
    let mut done: Vec<String> = Vec::new();
    let mut denied: Vec<&str> = Vec::new();
    let mut failed: Vec<String> = Vec::new();

    // CPU selections pass the topology gate first: an empty, missing-CPU or
    // multi-group mask never reaches Windows (it would fail or mean something else).
    let mut a = a.clone();
    for (name, field) in [("affinity", &mut a.affinity), ("cpu sets", &mut a.cpu_sets)] {
        if let Some(m) = *field {
            if let Some(why) = super::cpu::rule_mask_problem(m) {
                let shown = if m == 0 { "(empty)".to_string() } else { format_cpus(m) };
                failed.push(format!("{name} {shown} not applied: {why}"));
                *field = None;
            }
        }
    }
    let a = &a;

    let needs_full = a.priority.is_some() || a.affinity.is_some() || a.io.is_some() || a.memory.is_some() || a.efficiency.is_some() || a.boost.is_some();
    let full = PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION;
    let limited = PROCESS_SET_LIMITED_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION;
    let mut protected = false;
    let gone = || Outcome { gone: true, text: "process exited".into(), ..Default::default() };
    let h = if needs_full {
        match osutil::open_verified(pid, create, full) {
            Ok(h) => Some(h),
            Err(OpenFail::Denied) => {
                protected = true;
                None
            }
            Err(OpenFail::Gone) | Err(OpenFail::NoIdentity) => return gone(),
            Err(OpenFail::Other(e)) => {
                failed.push(format!("open: {e}"));
                None
            }
        }
    } else {
        None
    };
    let hl = if h.is_none() && (a.cpu_sets.is_some()) {
        match osutil::open_verified(pid, create, limited) {
            Ok(h) => Some(h),
            Err(OpenFail::Gone) | Err(OpenFail::NoIdentity) => return gone(),
            Err(_) => None,
        }
    } else {
        None
    };
    let hh = h.as_ref().map(|x| x.0);

    macro_rules! step {
        ($name:expr, $needs:expr, $body:expr) => {
            match hh {
                Some(h) => {
                    let r: windows::core::Result<()> = ($body)(h);
                    match r {
                        Ok(()) => done.push($name),
                        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => denied.push($needs),
                        Err(e) => failed.push(format!("{}: {}", $needs, e.message())),
                    }
                }
                None => denied.push($needs),
            }
        };
    }

    // Efficiency mode first: like Task Manager it implies Idle, and an explicit priority wins.
    if let Some(on) = a.efficiency {
        step!(if on { "efficiency on".to_string() } else { "efficiency off".to_string() }, "efficiency", |h| unsafe {
            let st = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
                StateMask: if on { PROCESS_POWER_THROTTLING_EXECUTION_SPEED } else { 0 },
            };
            SetProcessInformation(h, ProcessPowerThrottling, &st as *const _ as *const c_void, std::mem::size_of_val(&st) as u32)?;
            if on && a.priority.is_none() {
                SetPriorityClass(h, IDLE_PRIORITY_CLASS)?;
            }
            Ok(())
        });
    }
    if let Some(p) = a.priority {
        step!(format!("priority {}", p.key()), "priority", |h| unsafe { SetPriorityClass(h, prio_flag(p)) });
    }
    if let Some(m) = a.affinity {
        step!(format!("affinity {}", format_cpus(m)), "affinity", |h| unsafe { SetProcessAffinityMask(h, m as usize) });
    }
    if let Some(m) = a.cpu_sets {
        let h2 = hh.or(hl.as_ref().map(|x| x.0));
        match h2 {
            Some(h) => {
                let ga = [GROUP_AFFINITY { Mask: m as usize, Group: 0, Reserved: [0; 3] }];
                if unsafe { SetProcessDefaultCpuSetMasks(h, Some(&ga)) }.as_bool() {
                    done.push(format!("cpu sets {}", format_cpus(m)));
                } else {
                    let e = unsafe { GetLastError() };
                    if e == ERROR_ACCESS_DENIED {
                        denied.push("cpu sets");
                    } else {
                        failed.push(format!("cpu sets: error {}", e.0));
                    }
                }
            }
            None => denied.push("cpu sets"),
        }
    }
    if let Some(io) = a.io {
        step!(format!("i/o {}", io.key()), "i/o", |h: HANDLE| unsafe {
            let v = io.hint();
            let st = NtSetInformationProcess(h.0, PROCESS_IO_PRIORITY, &v as *const u32 as *const c_void, 4);
            if st >= 0 {
                Ok(())
            } else if st == 0xC0000022u32 as i32 {
                Err(windows::core::Error::from(ERROR_ACCESS_DENIED.to_hresult()))
            } else {
                Err(windows::core::Error::from(windows::core::HRESULT(st)))
            }
        });
    }
    if let Some(m) = a.memory {
        step!(format!("memory {}", m), "memory", |h| unsafe {
            let mi = MEMORY_PRIORITY_INFORMATION { MemoryPriority: MEMORY_PRIORITY(m.clamp(1, 5) as u32) };
            SetProcessInformation(h, ProcessMemoryPriority, &mi as *const _ as *const c_void, 4)
        });
    }
    if let Some(on) = a.boost {
        step!(if on { "boost on".to_string() } else { "boost off".to_string() }, "boost", |h| unsafe { SetProcessPriorityBoost(h, !on) });
    }
    if a.power_plan.is_some() || a.keep_awake.is_some() {
        match hold(pid, create, a.power_plan, a.keep_awake, reason) {
            Ok(()) => {
                if a.power_plan.is_some() {
                    done.push("power plan".into());
                }
                if a.keep_awake.is_some() {
                    done.push("prevent sleep".into());
                }
            }
            Err(e) => failed.push(e),
        }
    }

    let mut parts = Vec::new();
    if !denied.is_empty() {
        parts.push(format!("{}: {}", if protected { "access denied (protected)" } else { "access denied" }, denied.join(", ")));
    }
    if !failed.is_empty() {
        parts.push(format!("failed: {}", failed.join(", ")));
    }
    if !done.is_empty() {
        parts.push(format!("applied: {}", done.join(", ")));
    }
    if parts.is_empty() {
        parts.push("nothing to do".into());
    }
    Outcome { ok: denied.is_empty() && failed.is_empty(), protected, gone: false, text: parts.join("; ") }
}

// ---- power plan helpers ----------------------------------------------------

pub fn active_plan() -> Option<GUID> {
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

pub fn set_plan(g: &GUID) -> bool {
    unsafe { PowerSetActiveScheme(None, Some(g)) == ERROR_SUCCESS }
}

/// True when the plan's "processor minimum cores" (CPMINCORES, AC) is 100 %:
/// core parking off, which defeats AMD's V-Cache CCD parking. UI shows a warning.
pub fn plan_disables_parking(plan: &GUID) -> bool {
    const SUB_PROCESSOR: GUID = GUID::from_u128(0x54533251_82be_4824_96c1_47b60b740d00);
    const CPMINCORES: GUID = GUID::from_u128(0x0cc5b647_c1df_4637_891a_dec35c318583);
    let mut v = 0u32;
    let r = unsafe { PowerReadACValueIndex(None, Some(plan), Some(&SUB_PROCESSOR), Some(&CPMINCORES), &mut v) };
    r == ERROR_SUCCESS && v >= 100
}

// ---- holds -------------------------------------------------------------------

struct HoldEntry {
    seq: u64,
    proc_handle: Handle,
    wait: HANDLE,
    plan: Option<GUID>,
    request: Option<Handle>,
}
unsafe impl Send for HoldEntry {}

#[derive(Default)]
struct Holds {
    by_pid: HashMap<u32, HoldEntry>,
    seq: u64,
    original: Option<GUID>,
}

fn holds() -> &'static Mutex<Holds> {
    static H: OnceLock<Mutex<Holds>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(Holds::default()))
}

fn restore_file() -> std::path::PathBuf {
    osutil::data_dir().join("power-restore.txt")
}

/// Call once at startup: if Blink died while holding a plan, put the user's plan back.
pub fn recover_power_plan() {
    let f = restore_file();
    if let Ok(s) = std::fs::read_to_string(&f) {
        if let Some(g) = parse_guid(&s) {
            set_plan(&g);
        }
        let _ = std::fs::remove_file(&f);
    }
}

unsafe extern "system" fn on_exit(ctx: *mut c_void, _timeout: bool) {
    release(ctx as usize as u32);
}

fn hold(pid: u32, create: i64, plan: Option<GUID>, awake: Option<Awake>, reason: &str) -> Result<(), String> {
    let mut hs = holds().lock().unwrap();
    if hs.by_pid.contains_key(&pid) {
        return Ok(()); // already held for this pid
    }
    let ph = osutil::open_verified(pid, create, PROCESS_SYNCHRONIZE).map_err(|e| match e {
        OpenFail::Gone | OpenFail::NoIdentity => "hold: process exited or replaced".to_string(),
        OpenFail::Denied => "hold: access denied".to_string(),
        OpenFail::Other(m) => format!("hold: {m}"),
    })?;
    let request = match awake {
        Some(kind) => unsafe {
            let mut w = wide(reason);
            let ctx = REASON_CONTEXT {
                Version: 0, // POWER_REQUEST_CONTEXT_VERSION
                Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
                Reason: REASON_CONTEXT_0 { SimpleReasonString: PWSTR(w.as_mut_ptr()) },
            };
            let r = PowerCreateRequest(&ctx).map_err(|e| format!("prevent sleep: {}", e.message()))?;
            let _ = PowerSetRequest(r, PowerRequestSystemRequired);
            if kind == Awake::Display {
                let _ = PowerSetRequest(r, PowerRequestDisplayRequired);
            }
            Some(Handle(r))
        },
        None => None,
    };
    if let Some(g) = plan {
        let had_plan = hs.by_pid.values().any(|e| e.plan.is_some());
        if !had_plan {
            hs.original = active_plan();
            if let Some(o) = &hs.original {
                crate::bgio::write_file(restore_file(), format_guid(o).into_bytes());
            }
        }
        set_plan(&g);
    }
    let mut wait = HANDLE::default();
    unsafe {
        RegisterWaitForSingleObject(&mut wait, ph.0, Some(on_exit), Some(pid as usize as *const c_void), INFINITE, WT_EXECUTEONLYONCE)
            .map_err(|e| format!("hold wait: {}", e.message()))?;
    }
    hs.seq += 1;
    let seq = hs.seq;
    hs.by_pid.insert(pid, HoldEntry { seq, proc_handle: ph, wait, plan, request });
    Ok(())
}

fn release(pid: u32) {
    let mut hs = holds().lock().unwrap();
    let Some(e) = hs.by_pid.remove(&pid) else { return };
    unsafe {
        let _ = UnregisterWaitEx(e.wait, None); // non-blocking: safe inside the wait callback
    }
    if let Some(r) = &e.request {
        unsafe {
            let _ = PowerClearRequest(r.0, PowerRequestSystemRequired);
            let _ = PowerClearRequest(r.0, PowerRequestDisplayRequired);
        }
    }
    drop(e.proc_handle);
    if e.plan.is_some() {
        // newest remaining plan hold wins; none left → restore the user's plan
        let next = hs.by_pid.values().filter(|x| x.plan.is_some()).max_by_key(|x| x.seq).and_then(|x| x.plan);
        match next {
            Some(g) => {
                set_plan(&g);
            }
            None => {
                if let Some(o) = hs.original.take() {
                    set_plan(&o);
                }
                let _ = std::fs::remove_file(restore_file());
            }
        }
    }
}

/// Number of live holds (pids with power plan / prevent sleep).
pub fn hold_count() -> usize {
    holds().lock().unwrap().by_pid.len()
}

/// Blink shutdown: drop every hold and restore the plan.
pub fn release_all() {
    let pids: Vec<u32> = holds().lock().unwrap().by_pid.keys().copied().collect();
    for p in pids {
        release(p);
    }
}

// ---- CCD detection ---------------------------------------------------------------

/// CPU masks grouped by shared L3 (one per CCD on Ryzen), largest L3 first
/// (V-Cache CCD first on X3D). Returns (mask, L3 bytes).
pub fn l3_groups() -> Vec<(u64, u32)> {
    use windows::Win32::System::SystemInformation::*;
    let mut len = 0u32;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationCache, None, &mut len);
    }
    if len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u64; len as usize / 8 + 1];
    if unsafe { GetLogicalProcessorInformationEx(RelationCache, Some(buf.as_mut_ptr() as *mut SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX), &mut len) }.is_err() {
        return Vec::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, len as usize) };
    let mut out: Vec<(u64, u32)> = Vec::new();
    let mut off = 0usize;
    while off + 48 <= bytes.len() {
        let rd32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let rel = rd32(off);
        let size = rd32(off + 4) as usize;
        if size == 0 {
            break;
        }
        // CACHE_RELATIONSHIP: Level u8 @8, CacheSize u32 @12, Type u32 @16, GroupMask.Mask @40, Group @48
        if rel == 2 && bytes[off + 8] == 3 {
            let cache = rd32(off + 12);
            let mask = u64::from_le_bytes(bytes[off + 40..off + 48].try_into().unwrap());
            let group = u16::from_le_bytes(bytes[off + 48..off + 50].try_into().unwrap());
            if group == 0 && !out.iter().any(|(m, _)| *m == mask) {
                out.push((mask, cache));
            }
        }
        off += size;
    }
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_cpu_masks_are_refused_before_windows() {
        let me = std::process::id();
        let create = osutil::identity(me).unwrap().0;
        // empty mask (what a hand-edited `affinity = 0x0` used to become)
        let o = apply(me, create, &Actions { affinity: Some(0), ..Default::default() }, "test");
        assert!(!o.ok);
        assert!(o.text.contains("affinity (empty) not applied"), "{}", o.text);
        assert!(o.text.contains("at least one"), "{}", o.text);
        // CPUs that do not exist
        let valid = super::super::cpu::topology().valid;
        if valid != u64::MAX {
            let o = apply(me, create, &Actions { cpu_sets: Some(!valid), ..Default::default() }, "test");
            assert!(!o.ok);
            assert!(o.text.contains("does not exist"), "{}", o.text);
        }
    }

    fn sleeper() -> (std::process::Child, i64) {
        let c = crate::osutil::quiet_cmd("ping").args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        let create = osutil::identity(c.id()).expect("child identity").0;
        (c, create)
    }

    fn prio(pid: u32) -> u32 {
        let h = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.unwrap());
        unsafe { GetPriorityClass(h.0) }
    }

    #[test]
    fn apply_touches_only_the_process_it_was_given_identity_for() {
        let (mut c, create) = sleeper();
        let before = prio(c.id());
        let want = Actions { priority: Some(Priority::Idle), ..Default::default() };
        // the PID now belongs to "someone else" as far as the caller knows: nothing changes
        let o = apply(c.id(), create + 1, &want, "test");
        assert!(o.gone, "{o:?}");
        assert_eq!(prio(c.id()), before, "wrong identity changed the process");
        let o = apply(c.id(), 0, &want, "test");
        assert!(o.gone, "unknown start time applies nothing: {o:?}");
        assert_eq!(prio(c.id()), before);
        // the right identity applies
        let o = apply(c.id(), create, &want, "test");
        assert!(o.ok, "{o:?}");
        assert_eq!(prio(c.id()), IDLE_PRIORITY_CLASS.0);
        // exited: gone, never an error against a stranger
        let _ = c.kill();
        let _ = c.wait();
        assert!(apply(c.id(), create, &want, "test").gone);
    }

    #[test]
    fn power_hold_is_verified_too() {
        let (mut c, create) = sleeper();
        let a = Actions { keep_awake: Some(Awake::System), ..Default::default() };
        let o = apply(c.id(), create + 1, &a, "test");
        assert!(!o.ok && o.text.contains("replaced"), "{o:?}");
        assert!(!holds().lock().unwrap().by_pid.contains_key(&c.id()), "no hold on an unverified process");
        let _ = c.kill();
        let _ = c.wait();
    }
}
