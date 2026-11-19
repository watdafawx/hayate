//! CPU topology, selection checks, readback and one-shot apply behind the
//! "CPU affinity" / "CPU sets" panel (and the guard for saved Always rules).
//!
//! Two different Windows mechanisms, never mixed up here:
//! - **Hard affinity** (`SetProcessAffinityMask`): a hard limit, inherited by
//!   children. Needs `PROCESS_SET_INFORMATION`, so protected processes refuse.
//! - **CPU sets** (`SetProcessDefaultCpuSetMasks`): a scheduler selection for
//!   the process's threads. Not a hard limit; hard affinity takes precedence
//!   and threads can carry their own selection. Needs only limited access.
//!
//! Group 0 only. Blink's rule format is one `u64` mask, so a PC with several
//! processor groups (more than 64 logical CPUs in a group split) gets no CPU
//! selection at all rather than a silently truncated one.

use std::sync::OnceLock;

use windows::Win32::Foundation::*;
use windows::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows::Win32::System::Threading::*;

use super::model::format_cpus;
use crate::osutil::Handle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// hard affinity mask
    Affinity,
    /// default CPU sets of the process
    CpuSets,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Affinity => "CPU affinity",
            Kind::CpuSets => "CPU sets",
        }
    }
    /// What the default (unrestricted) state is called for this kind.
    pub fn default_label(self) -> &'static str {
        match self {
            Kind::Affinity => "All CPUs",
            Kind::CpuSets => "Default",
        }
    }
}

// ---- topology ----------------------------------------------------------------

/// One logical CPU of group 0 as Windows labels it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lp {
    pub index: u8,
    /// same value = SMT siblings of one core
    pub core: u8,
    pub l3: u8,
    /// Windows efficiency class: higher = faster, less efficient core
    pub class: u8,
}

/// Raw row of `GetSystemCpuSetInformation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuEntry {
    pub group: u16,
    pub index: u8,
    pub core: u8,
    pub l3: u8,
    pub class: u8,
}

/// Walk a `SYSTEM_CPU_SET_INFORMATION` buffer (Size u32, Type u32, then for
/// type 0: Id u32 @8, Group u16 @12, LogicalProcessorIndex @14, CoreIndex @15,
/// LastLevelCacheIndex @16, NumaNodeIndex @17, EfficiencyClass @18).
pub fn parse_cpu_sets(b: &[u8]) -> Vec<CpuEntry> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 8 <= b.len() {
        let size = u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) as usize;
        let ty = u32::from_le_bytes(b[off + 4..off + 8].try_into().unwrap());
        if size < 8 || off + size > b.len() {
            break;
        }
        if ty == 0 && size >= 19 {
            out.push(CpuEntry {
                group: u16::from_le_bytes(b[off + 12..off + 14].try_into().unwrap()),
                index: b[off + 14],
                core: b[off + 15],
                l3: b[off + 16],
                class: b[off + 18],
            });
        }
        off += size;
    }
    out
}

/// What Blink knows about this PC's logical CPUs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topology {
    /// active processor groups
    pub groups: u16,
    /// logical CPUs of group 0 (bit n = CPU n)
    pub valid: u64,
    /// labelled subset of `valid`, ascending
    pub lps: Vec<Lp>,
}

impl Topology {
    /// Pure builder: `active0` = logical CPUs in group 0.
    pub fn from_parts(groups: u16, active0: u32, entries: &[CpuEntry]) -> Topology {
        let valid = if active0 >= 64 { u64::MAX } else { (1u64 << active0) - 1 };
        let mut lps: Vec<Lp> = entries
            .iter()
            .filter(|e| e.group == 0 && e.index < 64 && valid & (1u64 << e.index) != 0)
            .map(|e| Lp { index: e.index, core: e.core, l3: e.l3, class: e.class })
            .collect();
        lps.sort_by_key(|l| l.index);
        lps.dedup_by_key(|l| l.index);
        Topology { groups, valid, lps }
    }

    pub fn count(&self) -> u32 {
        self.valid.count_ones()
    }

    /// Err(reason) when Blink cannot address this PC's CPUs faithfully.
    pub fn supported(&self) -> Result<(), String> {
        if self.groups > 1 {
            return Err(format!(
                "This PC has {} processor groups. Hayate's CPU rules only address group 0, so CPU selection is off instead of applying a partial mask.",
                self.groups
            ));
        }
        if self.valid == 0 {
            return Err("Windows reported no logical CPUs.".into());
        }
        Ok(())
    }

    /// The only gate a CPU selection passes: addressable PC, not empty, only CPUs that exist.
    pub fn validate(&self, mask: u64) -> Result<u64, String> {
        self.supported()?;
        if mask == 0 {
            return Err("Select at least one CPU. An empty selection is never applied.".into());
        }
        let extra = mask & !self.valid;
        if extra != 0 {
            return Err(format!("CPU {} does not exist on this PC ({} logical CPUs).", format_cpus(extra), self.count()));
        }
        Ok(mask)
    }

    fn lp(&self, i: u8) -> Option<&Lp> {
        self.lps.iter().find(|l| l.index == i)
    }
}

fn query_topology() -> Topology {
    use windows::Win32::System::SystemInformation::*;
    let (groups, active0) = unsafe { (GetActiveProcessorGroupCount(), GetActiveProcessorCount(0)) };
    let mut len = 0u32;
    unsafe {
        let _ = GetSystemCpuSetInformation(None, 0, &mut len, None, None);
    }
    let mut entries = Vec::new();
    if len > 0 {
        let mut buf = vec![0u64; len as usize / 8 + 1];
        let mut got = 0u32;
        let ok = unsafe { GetSystemCpuSetInformation(Some(buf.as_mut_ptr() as *mut SYSTEM_CPU_SET_INFORMATION), (buf.len() * 8) as u32, &mut got, None, None) };
        if ok.as_bool() {
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, (got as usize).min(buf.len() * 8)) };
            entries = parse_cpu_sets(bytes);
        }
    }
    Topology::from_parts(groups.max(1), active0, &entries)
}

/// This PC's topology, read once (CPU hot-add is not a desktop case).
pub fn topology() -> &'static Topology {
    static T: OnceLock<Topology> = OnceLock::new();
    T.get_or_init(query_topology)
}

/// (name, mask) per CCD (L3 group) when the PC has two or more and Blink can
/// address it; read once.
pub fn ccds() -> &'static [(String, u64)] {
    static C: OnceLock<Vec<(String, u64)>> = OnceLock::new();
    C.get_or_init(|| if topology().supported().is_ok() { super::ccd_names(&super::apply::l3_groups()) } else { Vec::new() })
}

// ---- sections for the panel --------------------------------------------------------

/// A labelled run of logical CPUs the panel draws as one block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub label: String,
    pub mask: u64,
    /// SMT siblings grouped per core, cores by lowest CPU
    pub cores: Vec<Vec<u8>>,
}

impl Section {
    pub fn detail(&self) -> String {
        let threads = self.mask.count_ones();
        let cores = self.cores.len() as u32;
        if cores < threads {
            format!("{cores} cores · {threads} threads")
        } else if threads == 1 {
            "1 CPU".to_string()
        } else {
            format!("{threads} CPUs")
        }
    }
}

fn cores_of(topo: &Topology, mask: u64) -> Vec<Vec<u8>> {
    let mut cores: Vec<(Option<u8>, Vec<u8>)> = Vec::new();
    for i in 0..64u8 {
        if mask & (1u64 << i) == 0 {
            continue;
        }
        match topo.lp(i).map(|l| l.core) {
            Some(c) => match cores.iter_mut().find(|(k, _)| *k == Some(c)) {
                Some((_, v)) => v.push(i),
                None => cores.push((Some(c), vec![i])),
            },
            // unlabelled CPU: its own core, nothing claimed about SMT
            None => cores.push((None, vec![i])),
        }
    }
    cores.sort_by_key(|(_, v)| v[0]);
    cores.into_iter().map(|(_, v)| v).collect()
}

/// Group the CPUs for display. `ccds` = (name, mask) per L3 group when the PC has
/// two or more; hybrid CPUs (two or more efficiency classes) split by class;
/// otherwise one block. CPUs no group claims go in a trailing "Other CPUs" block.
pub fn sections(topo: &Topology, ccds: &[(String, u64)]) -> Vec<Section> {
    let mut parts: Vec<(String, u64)> = Vec::new();
    if ccds.len() >= 2 {
        for (n, m) in ccds {
            parts.push((n.clone(), m & topo.valid));
        }
    } else {
        let mut classes: Vec<u8> = topo.lps.iter().map(|l| l.class).collect();
        classes.sort_unstable();
        classes.dedup();
        if classes.len() >= 2 {
            for (k, c) in classes.iter().enumerate().rev() {
                let m = topo.lps.iter().filter(|l| l.class == *c).fold(0u64, |a, l| a | 1u64 << l.index);
                let name = if classes.len() == 2 {
                    if k == 1 { "Performance cores".to_string() } else { "Efficiency cores".to_string() }
                } else {
                    format!("Efficiency class {c}")
                };
                parts.push((name, m));
            }
        }
    }
    if parts.is_empty() {
        parts.push(("All logical CPUs".into(), topo.valid));
    }
    let claimed = parts.iter().fold(0u64, |a, (_, m)| a | m);
    if topo.valid & !claimed != 0 {
        parts.push(("Other CPUs".into(), topo.valid & !claimed));
    }
    parts.into_iter().filter(|(_, m)| *m != 0).map(|(label, mask)| Section { label, mask, cores: cores_of(topo, mask) }).collect()
}

// ---- readback --------------------------------------------------------------------------

/// A failure with its plain reason; `gone` = the process is not there any more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fail {
    pub gone: bool,
    pub msg: String,
}

impl Fail {
    fn gone() -> Fail {
        Fail { gone: true, msg: "The process has exited.".into() }
    }
    fn text(s: impl Into<String>) -> Fail {
        Fail { gone: false, msg: s.into() }
    }
}

/// What Windows reports for one setting of a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cur {
    Mask(u64),
    /// CPU sets only: no default set, Windows chooses
    Default,
    Unreadable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Readback {
    pub affinity: Cur,
    pub sets: Cur,
}

impl Readback {
    pub fn of(&self, k: Kind) -> &Cur {
        match k {
            Kind::Affinity => &self.affinity,
            Kind::CpuSets => &self.sets,
        }
    }
}

fn denied(k: Kind) -> String {
    match k {
        Kind::Affinity => "Access denied. Hard affinity needs full process access; protected and higher-privilege processes refuse it. CPU sets may still work.".into(),
        Kind::CpuSets => "Access denied. This process refuses CPU sets changes (protected or higher privilege). Running Hayate as admin helps only for some.".into(),
    }
}

/// Open `pid` with `rights` (plus query) and prove it is still the process the
/// list showed: same creation time, not exited.
fn open_checked(pid: u32, create: i64, rights: PROCESS_ACCESS_RIGHTS, k: Kind) -> Result<Handle, Fail> {
    if pid <= 4 {
        return Err(Fail::text("System processes cannot be changed."));
    }
    if create == 0 {
        return Err(Fail::text("This process's start time is unknown, so Hayate cannot be sure it is the same one. Refresh and try again."));
    }
    let h = match unsafe { OpenProcess(rights | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => Handle(h),
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => return Err(Fail::text(denied(k))),
        Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Err(Fail::gone()),
        Err(e) => return Err(Fail::text(format!("Cannot open the process: {}", e.message()))),
    };
    let (mut c, mut x, mut kt, mut ut) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    if let Err(e) = unsafe { GetProcessTimes(h.0, &mut c, &mut x, &mut kt, &mut ut) } {
        return Err(Fail::text(format!("Cannot read the process start time: {}", e.message())));
    }
    let made = ((c.dwHighDateTime as i64) << 32) | c.dwLowDateTime as i64;
    if made != create {
        return Err(Fail { gone: true, msg: "That PID now belongs to a different process. The one you picked has exited.".into() });
    }
    // fail closed: an unreadable exit code does not prove the process is still running
    let mut code = 0u32;
    match unsafe { GetExitCodeProcess(h.0, &mut code) } {
        Ok(()) if code == STILL_ACTIVE.0 as u32 => Ok(h),
        Ok(()) => Err(Fail::gone()),
        Err(e) => Err(Fail::text(format!("Cannot confirm the process is still running: {}", e.message()))),
    }
}

fn read_affinity(h: HANDLE) -> Cur {
    let mut n = 8u16;
    let mut groups = [0u16; 8];
    if !unsafe { GetProcessGroupAffinity(h, &mut n, groups.as_mut_ptr()) }.as_bool() {
        return Cur::Unreadable(format!("group affinity error {}", unsafe { GetLastError() }.0));
    }
    if n != 1 || groups[0] != 0 {
        return Cur::Unreadable("process is not confined to processor group 0".into());
    }
    let (mut p, mut s) = (0usize, 0usize);
    match unsafe { GetProcessAffinityMask(h, &mut p, &mut s) } {
        Ok(()) => Cur::Mask(p as u64),
        Err(e) => Cur::Unreadable(e.message()),
    }
}

fn read_sets(h: HANDLE) -> Cur {
    let mut need = 0u16;
    let first = unsafe { GetProcessDefaultCpuSetMasks(h, None, &mut need) }.as_bool();
    if first && need == 0 {
        return Cur::Default;
    }
    if !first && unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
        return Cur::Unreadable(format!("cpu sets error {}", unsafe { GetLastError() }.0));
    }
    if need == 0 {
        return Cur::Default;
    }
    let mut buf = vec![GROUP_AFFINITY::default(); need as usize];
    let mut got = need;
    if !unsafe { GetProcessDefaultCpuSetMasks(h, Some(&mut buf), &mut got) }.as_bool() {
        return Cur::Unreadable(format!("cpu sets error {}", unsafe { GetLastError() }.0));
    }
    buf.truncate(got as usize);
    if buf.iter().any(|g| g.Group != 0) {
        return Cur::Unreadable("CPU sets reach outside processor group 0".into());
    }
    Cur::Mask(buf.iter().fold(0u64, |a, g| a | g.Mask as u64))
}

/// Current affinity and CPU sets of a process (identity-checked).
pub fn read(pid: u32, create: i64) -> Result<Readback, Fail> {
    let h = open_checked(pid, create, PROCESS_QUERY_LIMITED_INFORMATION, Kind::Affinity)?;
    Ok(Readback { affinity: read_affinity(h.0), sets: read_sets(h.0) })
}

// ---- manual apply ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// these CPUs, validated against the topology
    Set(u64),
    /// unrestricted: affinity = every CPU, CPU sets = no default set
    Reset,
}

/// Set or reset one setting of ONE process, nothing saved. `create` is the
/// creation time the list showed (a reused PID is refused).
pub fn apply_manual(pid: u32, create: i64, kind: Kind, change: Change) -> Result<String, Fail> {
    let topo = topology();
    topo.supported().map_err(Fail::text)?;
    let want = match change {
        Change::Set(m) => Some(topo.validate(m).map_err(Fail::text)?),
        Change::Reset => None,
    };
    let rights = match kind {
        Kind::Affinity => PROCESS_SET_INFORMATION,
        Kind::CpuSets => PROCESS_SET_LIMITED_INFORMATION,
    };
    let h = open_checked(pid, create, rights, kind)?;
    let err = |code: WIN32_ERROR| -> Fail {
        if code == ERROR_ACCESS_DENIED {
            return Fail::text(denied(kind));
        }
        // exited between the check and the call
        let mut c = 0u32;
        if unsafe { GetExitCodeProcess(h.0, &mut c) }.is_ok() && c != STILL_ACTIVE.0 as u32 {
            return Fail::gone();
        }
        Fail::text(format!("Windows refused the change (error {}).", code.0))
    };
    match kind {
        Kind::Affinity => {
            // the all-CPUs mask is the process's own system mask, never a guess
            let (mut p, mut s) = (0usize, 0usize);
            if let Err(e) = unsafe { GetProcessAffinityMask(h.0, &mut p, &mut s) } {
                return Err(Fail::text(format!("Cannot read the current affinity: {}", e.message())));
            }
            let mask = want.unwrap_or(s as u64);
            if mask == 0 {
                return Err(Fail::text("Select at least one CPU. An empty selection is never applied."));
            }
            if let Err(e) = unsafe { SetProcessAffinityMask(h.0, mask as usize) } {
                return Err(err(WIN32_ERROR((e.code().0 & 0xffff) as u32)));
            }
        }
        Kind::CpuSets => {
            let ok = match want {
                Some(m) => {
                    let ga = [GROUP_AFFINITY { Mask: m as usize, Group: 0, Reserved: [0; 3] }];
                    unsafe { SetProcessDefaultCpuSetMasks(h.0, Some(&ga)) }.as_bool()
                }
                None => unsafe { SetProcessDefaultCpuSetMasks(h.0, None) }.as_bool(),
            };
            if !ok {
                return Err(err(unsafe { GetLastError() }));
            }
        }
    }
    let now = match kind {
        Kind::Affinity => read_affinity(h.0),
        Kind::CpuSets => read_sets(h.0),
    };
    Ok(done_text(kind, change, &now, topo))
}

/// Result line after a successful change, from what Windows reports back.
pub fn done_text(kind: Kind, change: Change, now: &Cur, topo: &Topology) -> String {
    let all = topo.count();
    let said = match now {
        Cur::Mask(m) if matches!(change, Change::Set(w) if w == *m) => String::new(),
        Cur::Mask(m) if matches!(change, Change::Reset) && kind == Kind::Affinity && *m == topo.valid => String::new(),
        Cur::Mask(m) => format!(" Windows now reports {}.", format_cpus(*m)),
        Cur::Default if matches!(change, Change::Reset) => String::new(),
        Cur::Default => " Windows now reports no CPU set.".into(),
        Cur::Unreadable(e) => format!(" Could not read it back ({e})."),
    };
    let head = match (kind, change) {
        (Kind::Affinity, Change::Set(m)) => format!("Affinity set to {} ({} of {all} CPUs). Children started later inherit it.", format_cpus(m), m.count_ones()),
        (Kind::Affinity, Change::Reset) => format!("Affinity reset: all {all} CPUs."),
        (Kind::CpuSets, Change::Set(m)) => format!("CPU sets set to {} ({} of {all} CPUs).", format_cpus(m), m.count_ones()),
        (Kind::CpuSets, Change::Reset) => "CPU sets cleared: Windows picks CPUs again.".into(),
    };
    format!("{head}{said}")
}

/// For `apply()`: why a saved rule's mask must not be applied here (None = fine).
pub fn rule_mask_problem(mask: u64) -> Option<String> {
    topology().validate(mask).err()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(index: u8, core: u8, l3: u8, class: u8) -> CpuEntry {
        CpuEntry { group: 0, index, core, l3, class }
    }

    /// 2 CCDs x 4 cores x SMT2 = 16 CPUs; CPU n and n+1 share a core.
    fn ryzen() -> Topology {
        let entries: Vec<CpuEntry> = (0..16u8).map(|i| e(i, i / 2, i / 8, 0)).collect();
        Topology::from_parts(1, 16, &entries)
    }

    /// 2 P cores (SMT) + 4 E cores, one L3: 8 CPUs.
    fn hybrid() -> Topology {
        let mut v = vec![e(0, 0, 0, 1), e(1, 0, 0, 1), e(2, 1, 0, 1), e(3, 1, 0, 1)];
        v.extend((4..8u8).map(|i| e(i, i, 0, 0)));
        Topology::from_parts(1, 8, &v)
    }

    #[test]
    fn validate_rejects_empty_missing_and_multigroup() {
        let t = ryzen();
        assert_eq!(t.validate(0b11).unwrap(), 0b11);
        assert!(t.validate(0).unwrap_err().contains("at least one"));
        let err = t.validate(1 << 20).unwrap_err();
        assert!(err.contains("20") && err.contains("16 logical"), "{err}");
        assert!(t.validate(0xFFFF).is_ok());
        assert!(t.validate(0x1_FFFF).is_err());
        let multi = Topology::from_parts(2, 64, &[]);
        assert!(multi.supported().unwrap_err().contains("2 processor groups"));
        assert!(multi.validate(1).is_err());
    }

    #[test]
    fn sixty_four_cpus_use_the_whole_mask() {
        let t = Topology::from_parts(1, 64, &[]);
        assert_eq!(t.valid, u64::MAX);
        assert!(t.validate(1 << 63).is_ok());
        // 128 logical CPUs split in two groups is refused, never cut to 64
        assert!(Topology::from_parts(2, 64, &[]).validate(u64::MAX).is_err());
    }

    #[test]
    fn other_group_entries_and_unknown_cpus_are_dropped() {
        let t = Topology::from_parts(1, 4, &[e(0, 0, 0, 0), e(9, 4, 0, 0), CpuEntry { group: 1, ..e(1, 1, 0, 0) }]);
        assert_eq!(t.lps.iter().map(|l| l.index).collect::<Vec<_>>(), vec![0]);
        assert_eq!(t.valid, 0b1111);
    }

    #[test]
    fn parses_cpu_set_buffer() {
        let mut b = Vec::new();
        for i in 0..3u8 {
            let mut row = vec![0u8; 32];
            row[0..4].copy_from_slice(&32u32.to_le_bytes());
            row[8..12].copy_from_slice(&(256 + i as u32).to_le_bytes());
            row[14] = i;
            row[15] = i / 2;
            row[16] = 1;
            row[18] = 4;
            b.extend(row);
        }
        let v = parse_cpu_sets(&b);
        assert_eq!(v.len(), 3);
        assert_eq!(v[2], e(2, 1, 1, 4));
        // truncated tail never panics
        assert_eq!(parse_cpu_sets(&b[..70]).len(), 2);
        assert!(parse_cpu_sets(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn sections_split_by_ccd_with_smt_pairs() {
        let t = ryzen();
        let s = sections(&t, &[("CCD0 · V-Cache".into(), 0x00FF), ("CCD1 · Frequency".into(), 0xFF00)]);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].label, "CCD0 · V-Cache");
        assert_eq!(s[1].mask, 0xFF00);
        assert_eq!(s[0].cores, vec![vec![0, 1], vec![2, 3], vec![4, 5], vec![6, 7]]);
        assert_eq!(s[0].detail(), "4 cores · 8 threads");
    }

    #[test]
    fn single_ccd_without_labels_is_one_block() {
        let t = Topology::from_parts(1, 6, &[]);
        let s = sections(&t, &[("CCD0".into(), 0x3F)]);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label, "All logical CPUs");
        // no labels: nothing claimed about SMT
        assert_eq!(s[0].detail(), "6 CPUs");
        assert_eq!(s[0].cores.len(), 6);
    }

    #[test]
    fn hybrid_splits_by_efficiency_class() {
        let s = sections(&hybrid(), &[]);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].label, "Performance cores");
        assert_eq!(s[0].mask, 0x0F);
        assert_eq!(s[1].label, "Efficiency cores");
        assert_eq!(s[1].mask, 0xF0);
        assert_eq!(s[1].detail(), "4 CPUs");
    }

    #[test]
    fn uncovered_cpus_get_an_other_block() {
        let t = Topology::from_parts(1, 8, &[]);
        let s = sections(&t, &[("CCD0".into(), 0x0F), ("CCD1".into(), 0x30)]);
        assert_eq!(s.len(), 3);
        assert_eq!(s[2].label, "Other CPUs");
        assert_eq!(s[2].mask, 0xC0);
    }

    #[test]
    fn result_text_reports_what_windows_says() {
        let t = ryzen();
        let ok = done_text(Kind::Affinity, Change::Set(0x0F), &Cur::Mask(0x0F), &t);
        assert_eq!(ok, "Affinity set to 0-3 (4 of 16 CPUs). Children started later inherit it.");
        let off = done_text(Kind::Affinity, Change::Set(0x0F), &Cur::Mask(0xFF), &t);
        assert!(off.contains("Windows now reports 0-7"));
        assert_eq!(done_text(Kind::Affinity, Change::Reset, &Cur::Mask(0xFFFF), &t), "Affinity reset: all 16 CPUs.");
        assert_eq!(done_text(Kind::CpuSets, Change::Reset, &Cur::Default, &t), "CPU sets cleared: Windows picks CPUs again.");
        assert!(done_text(Kind::CpuSets, Change::Reset, &Cur::Mask(3), &t).contains("now reports 0-1"));
    }

    // ---- real Windows calls against a disposable child process -------------------------------

    fn spawn_sleeper() -> (std::process::Child, i64) {
        let mut c = crate::osutil::quiet_cmd("ping").args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        let h = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, c.id()) }.unwrap());
        let (mut cr, mut x, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetProcessTimes(h.0, &mut cr, &mut x, &mut k, &mut u) }.unwrap();
        let create = ((cr.dwHighDateTime as i64) << 32) | cr.dwLowDateTime as i64;
        let _ = c.try_wait();
        (c, create)
    }

    #[test]
    fn manual_roundtrip_on_a_child_process() {
        let topo = topology();
        if topo.supported().is_err() || topo.count() < 2 {
            return;
        }
        let (mut c, create) = spawn_sleeper();
        let pid = c.id();
        let first = 1u64 << topo.valid.trailing_zeros();
        // identity: a wrong creation time is refused, nothing changed
        let bad = apply_manual(pid, create + 1, Kind::Affinity, Change::Set(first)).unwrap_err();
        assert!(bad.gone, "{bad:?}");
        // empty and out-of-range selections never reach Windows
        assert!(apply_manual(pid, create, Kind::Affinity, Change::Set(0)).unwrap_err().msg.contains("at least one"));
        assert!(apply_manual(pid, create, Kind::CpuSets, Change::Set(!topo.valid)).is_err());
        let before = read(pid, create).unwrap();
        // a child inherits its parent's affinity (cargo-ccd1 pins the test run to one CCD):
        // "unrestricted" is therefore every CPU, not whatever the process started with
        assert!(matches!(before.affinity, Cur::Mask(m) if m != 0 && m & !topo.valid == 0), "{before:?}");
        assert_eq!(before.sets, Cur::Default);
        // hard affinity: set, read back, reset to every CPU
        apply_manual(pid, create, Kind::Affinity, Change::Set(first)).unwrap();
        assert_eq!(read(pid, create).unwrap().affinity, Cur::Mask(first));
        apply_manual(pid, create, Kind::Affinity, Change::Reset).unwrap();
        assert_eq!(read(pid, create).unwrap().affinity, Cur::Mask(topo.valid));
        // CPU sets: set, read back, clear to default; affinity untouched
        apply_manual(pid, create, Kind::CpuSets, Change::Set(first)).unwrap();
        let mid = read(pid, create).unwrap();
        assert_eq!(mid.sets, Cur::Mask(first));
        assert_eq!(mid.affinity, Cur::Mask(topo.valid));
        apply_manual(pid, create, Kind::CpuSets, Change::Reset).unwrap();
        assert_eq!(read(pid, create).unwrap().sets, Cur::Default);
        // exited process: a clear "exited", not a crash
        let _ = c.kill();
        let _ = c.wait();
        let gone = apply_manual(pid, create, Kind::Affinity, Change::Set(first)).unwrap_err();
        assert!(gone.gone, "{gone:?}");
    }

    #[test]
    fn system_pids_are_refused() {
        assert!(!apply_manual(4, 1, Kind::Affinity, Change::Reset).unwrap_err().gone);
        assert!(read(0, 1).is_err());
    }
}
