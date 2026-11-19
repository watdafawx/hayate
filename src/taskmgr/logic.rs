//! Pure parts of the opt-in "Open Blink instead of Task Manager" feature
//! (IFEO `Debugger` redirect, docs/task-manager-replacement.md §7): which keys
//! may be written, the enable / disable / status state machine, the launcher's
//! decision, the ACL policy for the launcher file. No I/O: the registry and the
//! machine are the `Registry` and `Host` seams, so every test runs against an
//! in-memory fake and **nothing here can touch a real IFEO key**.

/// The one real image this feature redirects.
pub const REAL_TARGET: &str = "taskmgr.exe";
/// Staging targets: images that are not a system component. Test machine only.
pub const SURROGATE_PREFIX: &str = "blink-surrogate-";
pub const IFEO_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options";
/// Machine state written beside the redirect: `<STATE_KEY>\<target>`.
pub const STATE_KEY: &str = r"SOFTWARE\Blink\TaskManager";
pub const LAUNCHER_NAME: &str = "blink-tm.exe";
pub const SCHEMA: &str = "1";
/// Marker argument: the enable-time probe asks the launcher for a dry run.
pub const SELF_TEST_ARG: &str = "--blink-self-test";
/// Set in the environment of the native Task Manager the launcher starts: a
/// launcher that finds it already set is in a loop and starts nothing.
pub const GUARD_ENV: &str = "BLINK_TM_LAUNCHED";

pub const DEBUG_PROCESS: u32 = 0x1;
pub const DEBUG_ONLY_THIS_PROCESS: u32 = 0x2;
/// Creation flags of the native fallback. `DEBUG_PROCESS` is what makes
/// `CreateProcess` skip IFEO (no recursion, no registry toggling).
pub const NATIVE_FLAGS: u32 = DEBUG_PROCESS | DEBUG_ONLY_THIS_PROCESS;

// launcher / helper exit codes
pub const EXIT_OK: i32 = 0;
pub const EXIT_SELF_TEST_OK: i32 = 42;
pub const EXIT_UNTRUSTED: i32 = 10;
pub const EXIT_NO_STATE: i32 = 11;
pub const EXIT_NATIVE_FAILED: i32 = 12;
pub const EXIT_LOOP: i32 = 3;
pub const EXIT_NOT_ELEVATED: i32 = 5;
pub const EXIT_REFUSED: i32 = 6;
pub const EXIT_FOREIGN: i32 = 7;
pub const EXIT_PROBE: i32 = 8;
pub const EXIT_FAILED: i32 = 1;
/// refresh: the new launcher failed its check, the previous one was put back and works
pub const EXIT_KEPT: i32 = 13;
/// refresh: failed and the previous launcher could not be restored: the redirect was removed
pub const EXIT_REMOVED: i32 = 14;
/// refresh: a cold start holds the lock (or the lock is unusable); nothing was changed
pub const EXIT_BUSY: i32 = 15;
/// refresh: the launcher file could not be staged/swapped (nothing changed)
pub const EXIT_IO: i32 = 16;
/// the registry could not be read or written
pub const EXIT_REGISTRY: i32 = 17;

/// A valid target name (lower-case, no path): `taskmgr.exe` or a surrogate.
pub fn valid_target(name: &str) -> Option<String> {
    let n = name.trim().to_lowercase();
    let chars = !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    let ok_name = n == REAL_TARGET || (n.starts_with(SURROGATE_PREFIX) && n.ends_with(".exe") && n.len() > SURROGATE_PREFIX.len() + 4);
    (chars && ok_name && !n.contains("..")).then_some(n)
}

pub fn is_real(target: &str) -> bool {
    target == REAL_TARGET
}

pub fn ifeo_key(target: &str) -> String {
    format!(r"{IFEO_KEY}\{target}")
}

pub fn state_key(target: &str) -> String {
    format!(r"{STATE_KEY}\{target}")
}

pub fn norm(p: &str) -> String {
    p.trim().trim_matches('"').replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// The only launcher path that may ever be written into a `Debugger` value.
pub fn expected_launcher(program_files: &str) -> Option<String> {
    let pf = program_files.trim().trim_end_matches('\\');
    let b = pf.as_bytes();
    (b.len() >= 3 && b[1] == b':' && b[2] == b'\\').then(|| format!(r"{pf}\Blink\{LAUNCHER_NAME}"))
}

/// The `Debugger` string: the quoted launcher path, nothing else.
pub fn debugger_value(launcher: &str) -> String {
    format!("\"{launcher}\"")
}

pub fn is_ours(value: &str, launcher: &str) -> bool {
    norm(value) == norm(launcher) && value.trim().starts_with('"') && value.trim().ends_with('"')
}

/// Ours = the quoted launcher path Blink would write today, or the launcher
/// path recorded in our own admin-owned state at enable time (a Program Files
/// move or a different install root must never turn our own value Foreign).
/// The recorded path must be `%ProgramFiles%\Blink\blink-tm.exe` (== `launcher`,
/// the only place Blink ever installs it); a state that records anything else is
/// untrusted and never makes a value "ours" or a file deletable.
pub fn recorded_launcher_ok(recorded: &str, launcher: &str) -> bool {
    !recorded.trim().is_empty() && norm(recorded) == norm(launcher) && norm(crate_file_name(recorded)) == LAUNCHER_NAME && !recorded.contains("..")
}

pub fn is_ours_any(value: &str, launcher: &str, state: Option<&State>) -> bool {
    is_ours(value, launcher) || state.is_some_and(|s| recorded_launcher_ok(&s.launcher, launcher) && is_ours(value, &s.launcher))
}

/// The recorded state may be used: its key is admin-only and the launcher it
/// names is the Program Files one. `Ok` when there is no state at all.
pub fn state_usable(host: &dyn Host, state: Option<&State>, launcher: &str) -> Result<(), String> {
    host.state_trusted()?;
    match state {
        Some(s) if !recorded_launcher_ok(&s.launcher, launcher) => Err(format!("the recorded launcher path ({}) is not {launcher}", s.launcher)),
        _ => Ok(()),
    }
}

/// `--target-path` and staging targets exist for debug builds only (test machine runs).
/// A release build only ever redirects `taskmgr.exe` -> the Program Files launcher.
pub fn helper_args_allowed(target: &str, image: Option<&str>, debug_build: bool) -> bool {
    debug_build || (is_real(target) && image.is_none())
}

/// Elevated paths must not write into the user-writable data dir (%LOCALAPPDATA%).
pub fn log_allowed(elevated: bool) -> bool {
    !elevated
}

/// Where the native Task Manager is started from. Never from the launcher's
/// argv (untrusted): System32 for the real one, an admin-written state value
/// for a surrogate. Never the launcher itself (recursion).
pub fn native_path(target: &str, sysroot: &str, state_native: Option<&str>, launcher: &str) -> Option<String> {
    let p = if is_real(target) {
        format!(r"{}\System32\{REAL_TARGET}", sysroot.trim_end_matches('\\'))
    } else {
        let s = state_native?.to_string();
        if norm(crate_file_name(&s)) != target {
            return None;
        }
        s
    };
    (norm(&p) != norm(launcher)).then_some(p)
}

fn crate_file_name(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

// ---- the seams ----------------------------------------------------------------

/// HKLM (64-bit view) as a key/value store of strings.
pub trait Registry {
    fn get(&self, key: &str, name: &str) -> Result<Option<String>, String>;
    fn set(&mut self, key: &str, name: &str, value: &str) -> Result<(), String>;
    fn delete_value(&mut self, key: &str, name: &str) -> Result<(), String>;
    /// delete a key with everything under it
    fn delete_key(&mut self, key: &str) -> Result<(), String>;
    /// the key exists and has neither values nor subkeys
    fn is_empty(&self, key: &str) -> bool;
    fn subkeys(&self, key: &str) -> Vec<String>;
}

/// What the machine does for the state machine.
pub trait Host {
    /// the launcher file is where it must be and nobody but admins can change it or anything above it
    fn launcher_trusted(&self, path: &str) -> Result<(), String>;
    /// BEFORE anything is copied: every part of the path that exists already (the
    /// folder, Program Files, ...) is admin-only and no link/junction
    fn precheck(&self, path: &str) -> Result<(), String>;
    /// put the launcher at `path` (atomic swap, keeps the old one)
    fn install_launcher(&mut self, path: &str) -> Result<(), String>;
    /// Returns true when no launcher is left at `path`. A launcher that is running cannot be
    /// deleted: it is renamed away (and removed at reboot) when possible; false = still there
    /// (the next switch-off / uninstall removes it).
    fn remove_launcher(&mut self, path: &str) -> bool;
    /// HKLM\SOFTWARE\Blink and \TaskManager (when they exist) can be changed by admins only
    fn state_trusted(&self) -> Result<(), String>;
    /// is a launcher file at `path` right now
    fn launcher_exists(&self, path: &str) -> bool;
    /// is there a rollback copy (`blink-tm.old.exe`) beside `path`
    fn old_exists(&self, path: &str) -> bool;
    /// build ordinal of the image the helper runs from (a newer build has a larger number)
    fn source_ordinal(&self) -> u64;
    /// start the target image once and see the launcher answer the dry run
    fn probe(&mut self, target: &str, native: Option<&str>) -> Result<(), String>;

    // ---- launcher refresh (docs §7.5 "Launcher update") ----
    /// Take the cold-start lock (`.coldstart.lock`): a launcher that is cold-starting a Blink holds it.
    /// Err = held (or unusable): nothing may be swapped now.
    fn lock_cold_start(&mut self) -> Result<(), String>;
    fn unlock_cold_start(&mut self);
    /// sha256 of the image the helper runs from: the only source a launcher is ever copied from
    fn source_sha(&self) -> Result<String, String>;
    /// sha256 of the launcher file at `path`
    fn launcher_sha(&self, path: &str) -> Result<String, String>;
    /// Copy the source image beside `path` (`blink-tm.new.exe`) and prove by sha256 that the copy is
    /// `sha`. Err = nothing left behind, the launcher untouched.
    fn stage_launcher(&mut self, path: &str, sha: &str) -> Result<(), String>;
    /// Keep the launcher as `blink-tm.old.exe` and put the staged copy in its place (`swap_files`: the
    /// file at `path` never goes missing, except for a running image). Err = the old launcher is at
    /// `path` (the caller checks `launcher_exists`).
    fn swap_launcher(&mut self, path: &str) -> Result<(), String>;
    /// Put a copy of `blink-tm.old.exe` at `path` in one replace (`restore_files`). Err = not restored.
    fn restore_launcher(&mut self, path: &str) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Enabled,
    AlreadyOn,
    Disabled,
    AlreadyOff,
    /// disable: the value was someone else's; ours (state) is gone, theirs untouched
    ForeignKept,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Fail {
    BadTarget,
    Refused(String),
    Foreign(String),
    Untrusted(String),
    Registry(String),
    Probe(String),
    /// refresh: a cold start holds the lock, or the lock cannot be taken (nothing changed)
    Busy(String),
    /// refresh: staging or swapping the file failed (nothing changed)
    Io(String),
    /// refresh: the new launcher failed its check; the previous one is back and works
    Kept(String),
    /// refresh: failed and the previous launcher could not be restored: the redirect is off
    Removed(String),
}

impl Fail {
    pub fn exit_code(&self) -> i32 {
        match self {
            Fail::BadTarget | Fail::Refused(_) => EXIT_REFUSED,
            Fail::Foreign(_) => EXIT_FOREIGN,
            Fail::Untrusted(_) => EXIT_UNTRUSTED,
            Fail::Registry(_) => EXIT_REGISTRY,
            Fail::Probe(_) => EXIT_PROBE,
            Fail::Busy(_) => EXIT_BUSY,
            Fail::Io(_) => EXIT_IO,
            Fail::Kept(_) => EXIT_KEPT,
            Fail::Removed(_) => EXIT_REMOVED,
        }
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::BadTarget => write!(f, "Not a target Hayate may redirect"),
            Fail::Refused(m) => write!(f, "{m}"),
            Fail::Foreign(v) => write!(f, "Another tool already redirects Task Manager ({v}). Hayate did not touch it."),
            Fail::Untrusted(m) => write!(f, "The launcher is not protected enough: {m}. Nothing was changed."),
            Fail::Registry(m) => write!(f, "{m}"),
            Fail::Probe(m) => write!(f, "The check run failed ({m}). Everything was rolled back; Windows Task Manager is unchanged."),
            Fail::Busy(m) => write!(f, "A Task Manager start is in progress ({m}). Nothing was changed; try again in a few seconds."),
            Fail::Io(m) => write!(f, "{m}. Nothing was changed."),
            Fail::Kept(m) => write!(f, "The new launcher did not pass its check ({m}). The previous launcher was put back and works."),
            Fail::Removed(m) => write!(f, "The launcher update failed ({m}) and the previous launcher could not be restored. Hayate turned the Task Manager redirect off: Windows Task Manager opens again."),
        }
    }
}

/// Settings hint when the switch will not turn off. A raw string: the path keeps
/// its literal backslash (a backslash-t in a normal literal is a tab).
pub const RECOVERY_HINT: &str = r"Stuck? As administrator: reg delete the Debugger value of IFEO\taskmgr.exe (exact line: docs/task-manager-replacement.md)";

/// Text for a helper exit code (the unelevated Blink only sees the code).
pub fn exit_message(code: i32) -> String {
    match code {
        EXIT_OK => "done".into(),
        EXIT_NOT_ELEVATED => "needs administrator rights".into(),
        EXIT_REFUSED => "refused: this copy of Hayate may not do that".into(),
        EXIT_FOREIGN => "another tool already redirects Task Manager; Hayate left it alone".into(),
        EXIT_UNTRUSTED => "the launcher in Program Files is not protected enough; nothing was changed".into(),
        EXIT_PROBE => "the check run failed; everything was rolled back".into(),
        EXIT_KEPT => "the new launcher did not pass its check; the previous launcher was put back and works".into(),
        EXIT_REMOVED => "the launcher update failed and could not be undone: Hayate turned the redirect off, Windows Task Manager opens again".into(),
        EXIT_BUSY => "a Task Manager start is in progress; nothing was changed, try again in a few seconds".into(),
        EXIT_IO => "the launcher file could not be changed; nothing was changed".into(),
        EXIT_REGISTRY => "the registry could not be changed".into(),
        c => format!("failed (code {c})"),
    }
}

/// Admin-owned record of one redirect.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct State {
    pub enabled: bool,
    pub launcher: String,
    pub blink_path: String,
    pub native_path: Option<String>,
    pub schema: Option<String>,
    pub prev_existed: Option<String>,
    /// build ordinal of the launcher image (a refresh only ever moves it forward)
    pub build: Option<u64>,
}

pub fn read_state(reg: &dyn Registry, target: &str) -> Option<State> {
    let k = state_key(target);
    let g = |n: &str| reg.get(&k, n).ok().flatten();
    let launcher = g("LauncherPath")?;
    Some(State { enabled: g("Enabled").as_deref() == Some("1"), launcher, blink_path: g("BlinkPath").unwrap_or_default(), native_path: g("NativePath"), schema: g("SchemaVersion"), prev_existed: g("PreviousDebuggerExisted"), build: g("BuildOrdinal").and_then(|b| b.trim().parse().ok()) })
}

fn put(reg: &mut dyn Registry, key: &str, name: &str, v: &str) -> Result<(), Fail> {
    reg.set(key, name, v).map_err(Fail::Registry)
}

/// Enable. Order: launcher verified -> state saved (Enabled=0) -> Debugger
/// written -> probe -> Enabled=1. Any failure before the end restores the
/// previous registry exactly (a foreign value is refused up front, so
/// "previous" is always "no Debugger value").
pub fn enable(reg: &mut dyn Registry, host: &mut dyn Host, target: &str, program_files: &str, blink_path: &str, native: Option<&str>) -> Result<Outcome, Fail> {
    let target = valid_target(target).ok_or(Fail::BadTarget)?;
    let launcher = expected_launcher(program_files).ok_or_else(|| Fail::Refused("Program Files folder unknown".into()))?;
    match (is_real(&target), native) {
        (false, None) => return Err(Fail::Refused("a staging target needs its image path".into())),
        (false, Some(n)) if norm(crate_file_name(n)) != target => return Err(Fail::Refused("staging image name does not match the target".into())),
        (true, Some(_)) => return Err(Fail::Refused("the real Task Manager takes no image path (System32 is used)".into())),
        _ => {}
    }
    let (ik, sk) = (ifeo_key(&target), state_key(&target));
    let cur = reg.get(&ik, "Debugger").map_err(Fail::Registry)?;
    let old_state = read_state(reg, &target);
    state_usable(host, old_state.as_ref(), &launcher).map_err(Fail::Untrusted)?;
    // `prev`: a value of ours that is already there (rollback puts it back)
    let prev = match &cur {
        Some(v) if is_ours_any(v, &launcher, old_state.as_ref()) => {
            if is_ours(v, &launcher) && old_state.as_ref().is_some_and(|s| s.enabled) {
                return Ok(Outcome::AlreadyOn);
            }
            Some(v.clone())
        }
        Some(v) => return Err(Fail::Foreign(v.clone())),
        None => None,
    };
    // the folder chain is verified before a single byte is copied
    host.precheck(&launcher).map_err(Fail::Untrusted)?;
    let install = host.install_launcher(&launcher).map_err(Fail::Untrusted).and_then(|()| host.launcher_trusted(&launcher).map_err(Fail::Untrusted));
    if let Err(e) = install {
        drop_launcher_if_unused(reg, host, &launcher, prev.is_some());
        return Err(e);
    }
    let written = debugger_value(&launcher);
    // state first, Enabled=0: a crash from here on never claims "on"
    let result = (|| -> Result<(), Fail> {
        put(reg, &sk, "SchemaVersion", SCHEMA)?;
        put(reg, &sk, "Enabled", "0")?;
        put(reg, &sk, "LauncherPath", &launcher)?;
        put(reg, &sk, "BlinkPath", blink_path)?;
        put(reg, &sk, "PreviousDebuggerExisted", if prev.is_some() { "1" } else { "0" })?;
        put(reg, &sk, "BuildOrdinal", &host.source_ordinal().to_string())?;
        if let Some(n) = native {
            put(reg, &sk, "NativePath", n)?;
        }
        // a Debugger that appeared since the check above is someone else's: never overwritten
        match reg.get(&ik, "Debugger").map_err(Fail::Registry)? {
            Some(v) if prev.as_deref() != Some(v.as_str()) => return Err(Fail::Foreign(v)),
            None if prev.is_some() => return Err(Fail::Foreign("the value was removed while Hayate was enabling".into())),
            _ => {}
        }
        put(reg, &ik, "Debugger", &written)?;
        host.probe(&target, native).map_err(Fail::Probe)?;
        put(reg, &sk, "Enabled", "1")
    })();
    if let Err(e) = result {
        // roll back: put the Debugger value back as it was, but only if what is there is
        // the one this run wrote (re-read: anything else appeared meanwhile and is not ours to delete)
        let mut errs = Vec::new();
        match reg.get(&ik, "Debugger") {
            Ok(Some(v)) if norm(&v) == norm(&written) => {
                let r = match &prev {
                    Some(p) => reg.set(&ik, "Debugger", p),
                    None => reg.delete_value(&ik, "Debugger"),
                };
                if let Err(x) = r {
                    errs.push(x);
                }
            }
            Ok(_) => {}
            Err(x) => errs.push(x),
        }
        match &old_state {
            Some(o) => {
                let all = [
                    ("SchemaVersion", o.schema.as_deref()),
                    ("Enabled", Some(if o.enabled { "1" } else { "0" })),
                    ("LauncherPath", Some(o.launcher.as_str())),
                    ("BlinkPath", Some(o.blink_path.as_str())),
                    ("PreviousDebuggerExisted", o.prev_existed.as_deref()),
                    ("NativePath", o.native_path.as_deref()),
                ];
                let build = o.build.map(|b| b.to_string());
                let all = all.into_iter().chain([("BuildOrdinal", build.as_deref())]);
                for (n, v) in all {
                    let r = match v {
                        Some(v) => reg.set(&sk, n, v),
                        None => reg.delete_value(&sk, n),
                    };
                    if let Err(x) = r {
                        errs.push(x);
                    }
                }
            }
            None => {
                if let Err(x) = reg.delete_key(&sk) {
                    errs.push(x);
                }
            }
        }
        drop_empty_ifeo(reg, &ik);
        // never while a restored Debugger value of ours could still point at the file
        drop_launcher_if_unused(reg, host, &launcher, prev.is_some());
        if let Some(a) = errs.first() {
            return Err(Fail::Registry(format!("{e}; and the rollback failed: {a}. Delete the Debugger value by hand (docs/task-manager-replacement.md)")));
        }
        return Err(e);
    }
    Ok(Outcome::Enabled)
}

/// After a failed enable: when no other redirect has state, the copied file, its
/// .new/.old and the empty folder go (the registry state is already gone).
fn drop_launcher_if_unused(reg: &mut dyn Registry, host: &mut dyn Host, launcher: &str, value_may_point_here: bool) {
    if !value_may_point_here && reg.subkeys(STATE_KEY).is_empty() {
        if reg.is_empty(STATE_KEY) {
            let _ = reg.delete_key(STATE_KEY);
        }
        host.remove_launcher(launcher);
    }
}

fn drop_empty_ifeo(reg: &mut dyn Registry, ik: &str) {
    if reg.is_empty(ik) {
        let _ = reg.delete_key(ik);
    }
}

/// Disable / restore. Order: Debugger removed (only if it is ours) -> state
/// cleared -> launcher file removed when no redirect needs it.
pub fn disable(reg: &mut dyn Registry, host: &mut dyn Host, target: &str, program_files: &str) -> Result<Outcome, Fail> {
    let target = valid_target(target).ok_or(Fail::BadTarget)?;
    let launcher = expected_launcher(program_files).ok_or_else(|| Fail::Refused("Program Files folder unknown".into()))?;
    let (ik, sk) = (ifeo_key(&target), state_key(&target));
    let cur = reg.get(&ik, "Debugger").map_err(Fail::Registry)?;
    let state = read_state(reg, &target);
    // a state that anyone could have edited decides nothing: refuse, change nothing
    state_usable(host, state.as_ref(), &launcher).map_err(Fail::Untrusted)?;
    let had_state = state.is_some();
    let outcome = match &cur {
        Some(v) if is_ours_any(v, &launcher, state.as_ref()) => {
            reg.delete_value(&ik, "Debugger").map_err(Fail::Registry)?;
            drop_empty_ifeo(reg, &ik);
            Outcome::Disabled
        }
        Some(_) => Outcome::ForeignKept,
        None if had_state => Outcome::Disabled, // Windows already reset it: just tidy up
        None => Outcome::AlreadyOff,
    };
    if had_state {
        put(reg, &sk, "Enabled", "0").ok();
        reg.delete_key(&sk).map_err(Fail::Registry)?;
    }
    if reg.subkeys(STATE_KEY).is_empty() {
        let _ = reg.delete_key(STATE_KEY);
        host.remove_launcher(&launcher);
    }
    Ok(outcome)
}

/// Staging targets that have state of ours (debug builds clean them too).
pub fn staging_targets(reg: &dyn Registry) -> Vec<String> {
    reg.subkeys(STATE_KEY).iter().filter_map(|k| valid_target(k)).filter(|t| !is_real(t)).collect()
}

/// Turn everything of ours off: the real target, and in debug builds every
/// staging target that left state behind. Stops at the first failure.
pub fn disable_all(reg: &mut dyn Registry, host: &mut dyn Host, program_files: &str, include_staging: bool) -> Result<Outcome, Fail> {
    let staging = if include_staging { staging_targets(reg) } else { Vec::new() };
    let mut out = disable(reg, host, REAL_TARGET, program_files)?;
    for t in staging {
        if disable(reg, host, &t, program_files)? == Outcome::Disabled {
            out = Outcome::Disabled;
        }
    }
    Ok(out)
}

// ---- launcher refresh (docs/task-manager-replacement.md §7.5 "Launcher update") ----------------

/// How the launcher file compares with the Blink that is running.
#[derive(Clone, Debug, PartialEq)]
pub enum Fresh {
    /// same sha256
    Current,
    /// a different build and this Blink is the newer one (or the order is unknown): refresh is offered
    Stale,
    /// a different build, but this Blink is NOT newer than the launcher (a rolled-back Blink): never a downgrade
    NotNewer,
    /// a hash could not be read: nothing is offered
    Unknown,
}

/// `*_build`: build ordinals (larger = newer). Equal hashes are current; otherwise the refresh is
/// offered only when this Blink is newer than the launcher. A launcher without a recorded ordinal
/// (enabled by an older Blink) counts as older; an unknown running ordinal (the cold-started
/// launcher comparing with the installed exe) decides by the hashes alone.
pub fn freshness(launcher_sha: Option<&str>, running_sha: Option<&str>, launcher_build: Option<u64>, running_build: Option<u64>) -> Fresh {
    match (launcher_sha.map(str::trim), running_sha.map(str::trim)) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => {
            if a.eq_ignore_ascii_case(b) {
                Fresh::Current
            } else if matches!((launcher_build, running_build), (Some(l), Some(r)) if r <= l) {
                Fresh::NotNewer
            } else {
                Fresh::Stale
            }
        }
        _ => Fresh::Unknown,
    }
}

/// Settings offers "Refresh launcher" only for a redirect that is `On`, a launcher that differs from
/// this Blink, an installed Blink that is not itself the launcher image (a cold-started Blink IS the
/// launcher: it never refreshes itself) and not while a helper run is under way.
pub fn refresh_offer(status: &Status, fresh: &Fresh, allowed: bool, is_launcher_image: bool, busy: bool) -> bool {
    allowed && !is_launcher_image && !busy && *status == Status::On && *fresh == Fresh::Stale
}

/// Key of one (launcher, running Blink) pair: an automatic refresh is tried once per pair.
pub fn pair_key(launcher_sha: &str, running_sha: &str) -> String {
    let short = |s: &str| s.trim().to_ascii_lowercase().chars().take(16).collect::<String>();
    format!("{}:{}", short(launcher_sha), short(running_sha))
}

/// An elevated Blink may refresh the launcher by itself at start (no second UAC prompt: the user
/// already consented to elevation): only when the Settings offer would be there, this pair was not tried
/// yet, or an elevated self-update just finished (`forced`: that update is the reason the pair differs).
/// Settings offers "Repair launcher": our value is on, the launcher file is gone (a swap that was cut
/// short) and the rollback copy is there.
pub fn repair_offer(status: &Status, state_enabled: bool, launcher_exists: bool, old_exists: bool, allowed: bool, is_launcher_image: bool, busy: bool) -> bool {
    allowed && !is_launcher_image && !busy && state_enabled && matches!(status, Status::Stale(_)) && !launcher_exists && old_exists
}

/// Text the elevated helper leaves for the asking Blink: no control characters (a registry value is
/// user-visible text, not markup), one line, at most 400 characters.
pub fn clean_text(t: &str) -> String {
    let one: String = t.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    one.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(400).collect()
}

pub fn auto_refresh_due(offer: bool, elevated: bool, tried_pair: Option<&str>, pair: &str, forced: bool) -> bool {
    offer && elevated && (forced || tried_pair != Some(pair))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Refreshed {
    /// the launcher is the helper's own image already: nothing was touched
    Current,
    /// swapped, verified, probed; the previous file is kept as `blink-tm.old.exe`
    Done,
    /// the launcher file was missing (a cut-short swap): the previous copy was put back and probed
    Repaired,
}

/// The file operations of a launcher swap. Real: `RealFs` (Win32). Tests: an in-memory disk that
/// "crashes" after any number of operations.
pub trait Fs {
    fn exists(&self, p: &str) -> bool;
    /// copy `from` to a NEW file `to` (`to` does not exist)
    fn copy(&mut self, from: &str, to: &str) -> Result<(), String>;
    /// atomic: `to` (a file or nothing) is replaced by `from`; fails while `to` is a running image
    fn replace(&mut self, from: &str, to: &str) -> Result<(), String>;
    /// `to` does not exist; a running image may be renamed
    fn rename(&mut self, from: &str, to: &str) -> Result<(), String>;
    fn remove(&mut self, p: &str) -> Result<(), String>;
    /// administrators/SYSTEM only (best effort)
    fn protect(&mut self, p: &str);
    /// delete the file at the next reboot (best effort)
    fn delete_at_reboot(&mut self, p: &str);
    /// a short wait between two tries
    fn pause(&mut self);
}

/// Keep `path` as `old` and put `new` at `path`. The file at `path` is never missing, except for a
/// running image (it cannot be replaced, only renamed): then a crash between the two renames leaves
/// `old` and no `path`, which `refresh` repairs.
///   1. the rollback copy `old` is a COPY of the launcher (the launcher is not touched)
///   2. ONE atomic replace `new` -> `path`
///   3. (running image only) rename `path` -> `old`, rename `new` -> `path`
pub fn swap_files(fs: &mut dyn Fs, path: &str, new: &str, old: &str) -> Result<(), String> {
    if !fs.exists(new) {
        return Err("the staged launcher is missing".into());
    }
    if fs.exists(old) && fs.remove(old).is_err() && fs.exists(old) {
        let _ = fs.remove(new);
        return Err("the previous copy blink-tm.old.exe is still running; close the Hayate that Task Manager started and try again".into());
    }
    if !fs.exists(path) {
        return fs.rename(new, path).map_err(|e| {
            let _ = fs.remove(new);
            format!("Cannot place the new launcher at {path}: {e}")
        });
    }
    if let Err(e) = fs.copy(path, old) {
        let _ = fs.remove(old);
        let _ = fs.remove(new);
        return Err(format!("Cannot keep the old launcher: {e}"));
    }
    fs.protect(old);
    if fs.replace(new, path).is_ok() {
        return Ok(());
    }
    // a running image: rename it aside (the copy made above is not needed, the file itself is the rollback)
    if fs.remove(old).is_err() && fs.exists(old) {
        let _ = fs.remove(new);
        return Err("Cannot replace the launcher: it is in use".into());
    }
    if let Err(e) = fs.rename(path, old) {
        let _ = fs.remove(new);
        return Err(format!("Cannot move the old launcher aside: {e}"));
    }
    fs.protect(old);
    fs.delete_at_reboot(old);
    if let Err(e) = fs.rename(new, path) {
        let _ = fs.rename(old, path);
        let _ = fs.remove(new);
        return Err(format!("Cannot place the new launcher at {path}: {e}"));
    }
    Ok(())
}

/// Put a copy of `old` at `path` (via the staged name `new`) in one replace: `path` is never removed first.
pub fn restore_files(fs: &mut dyn Fs, path: &str, new: &str, old: &str) -> Result<(), String> {
    if !fs.exists(old) {
        return Err("there is no previous launcher copy (blink-tm.old.exe) to put back".into());
    }
    let _ = fs.remove(new);
    fs.copy(old, new).map_err(|e| {
        let _ = fs.remove(new);
        format!("Cannot copy the previous launcher: {e}")
    })?;
    let mut last = String::new();
    for i in 0..5 {
        match fs.replace(new, path) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
        if i < 4 {
            fs.pause();
        }
    }
    let _ = fs.remove(new);
    Err(format!("Cannot put the previous launcher back: {last}"))
}

enum Step {
    Fail(Fail),
    /// swap and restore both failed: the redirect must go (native Task Manager)
    Remove(String),
}

/// Replace the launcher file with the helper's own image. Order: state and value checked (untrusted
/// state refuses, nothing changes) -> cold-start lock taken (a cold start in progress = `Busy`) ->
/// hashes compared -> the build must be newer -> staged copy beside the target, hash-checked -> swap
/// keeping `blink-tm.old.exe` -> trust check + probe + the file at the launcher path is the staged
/// image. A failed check puts the old file back and probes again; when even that fails the `Debugger`
/// value is removed (`disable`) so it never points at a missing or broken file. A launcher file that
/// is MISSING (a cut-short swap) is put back from `blink-tm.old.exe` the same way (repair).
pub fn refresh(reg: &mut dyn Registry, host: &mut dyn Host, target: &str, program_files: &str, blink_path: &str) -> Result<Refreshed, Fail> {
    let target = valid_target(target).ok_or(Fail::BadTarget)?;
    let launcher = expected_launcher(program_files).ok_or_else(|| Fail::Refused("Program Files folder unknown".into()))?;
    let (ik, sk) = (ifeo_key(&target), state_key(&target));
    let state = read_state(reg, &target);
    state_usable(host, state.as_ref(), &launcher).map_err(Fail::Untrusted)?;
    let Some(state) = state.filter(|s| s.enabled) else { return Err(Fail::Refused("The Task Manager redirect is not on, so there is no launcher to refresh".into())) };
    match reg.get(&ik, "Debugger").map_err(Fail::Registry)? {
        Some(v) if is_ours_any(&v, &launcher, Some(&state)) => {}
        Some(v) => return Err(Fail::Foreign(v)),
        None => return Err(Fail::Refused("The redirect value is gone (Windows reset it)".into())),
    }
    host.lock_cold_start().map_err(Fail::Busy)?;
    let r = locked_refresh(host, &target, &launcher, state.native_path.as_deref(), state.build);
    host.unlock_cold_start();
    match r {
        Ok(done) => {
            // best effort, and only while the redirect is still on: a disable that ran meanwhile must not
            // find its state key written to again (never recreate a key that is gone)
            let alive = read_state(reg, &target).is_some_and(|s| s.enabled);
            if alive && done == Refreshed::Done {
                // the launcher is the newer build now (a missing record counts as older)
                let _ = reg.set(&sk, "BuildOrdinal", &host.source_ordinal().to_string());
                // a Blink that moved (per-user -> all users) is the one the launcher messages next
                if !blink_path.trim().is_empty() && norm(blink_path) != norm(&state.blink_path) {
                    let _ = reg.set(&sk, "BlinkPath", blink_path);
                }
            }
            Ok(done)
        }
        Err(Step::Fail(e)) => Err(e),
        Err(Step::Remove(why)) => Err(match disable(reg, host, &target, program_files) {
            Ok(_) => Fail::Removed(why),
            Err(e) => Fail::Registry(format!("{why}; and turning the redirect off failed: {e}. Delete the Debugger value by hand (docs/task-manager-replacement.md)")),
        }),
    }
}

/// Everything that looks at the launcher file runs under the cold-start lock: whether the file is there
/// (a repair) is decided only now, never before the lock.
fn locked_refresh(host: &mut dyn Host, target: &str, launcher: &str, native: Option<&str>, build: Option<u64>) -> Result<Refreshed, Step> {
    if !host.launcher_exists(launcher) {
        host.precheck(launcher).map_err(|e| Step::Fail(Fail::Untrusted(e)))?;
        repair_step(host, target, launcher, native)
    } else {
        // the file the value points at must be the trusted one before anything is compared or moved
        host.launcher_trusted(launcher).map_err(|e| Step::Fail(Fail::Untrusted(e)))?;
        swap_checked(host, target, launcher, native, build)
    }
}

fn check_launcher(host: &mut dyn Host, target: &str, launcher: &str, native: Option<&str>) -> Result<(), String> {
    host.launcher_trusted(launcher)?;
    host.probe(target, native)
}

/// the file at the launcher path really is the image with this hash
fn is_image(host: &dyn Host, launcher: &str, sha: &str) -> Result<(), String> {
    match host.launcher_sha(launcher) {
        Ok(h) if h.trim().eq_ignore_ascii_case(sha.trim()) => Ok(()),
        Ok(_) => Err("the file at the launcher path is not the expected image (sha256)".into()),
        Err(e) => Err(e),
    }
}

fn repair_step(host: &mut dyn Host, target: &str, launcher: &str, native: Option<&str>) -> Result<Refreshed, Step> {
    if !host.old_exists(launcher) {
        return Err(Step::Remove("the launcher file is missing and there is no previous copy to put back".into()));
    }
    if let Err(x) = host.restore_launcher(launcher) {
        return Err(Step::Remove(format!("the launcher file is missing and putting the previous copy back failed: {x}")));
    }
    match check_launcher(host, target, launcher, native) {
        Ok(()) => Ok(Refreshed::Repaired),
        Err(x) => Err(Step::Remove(format!("the repaired launcher failed its check: {x}"))),
    }
}

fn swap_checked(host: &mut dyn Host, target: &str, launcher: &str, native: Option<&str>, build: Option<u64>) -> Result<Refreshed, Step> {
    let fail = |f: Fail| Step::Fail(f);
    let src = host.source_sha().map_err(|e| fail(Fail::Io(e)))?;
    let cur = host.launcher_sha(launcher).map_err(|e| fail(Fail::Untrusted(e)))?;
    if cur.trim().eq_ignore_ascii_case(src.trim()) {
        return Ok(Refreshed::Current);
    }
    // never a downgrade: a rolled-back Blink leaves a newer launcher alone
    if build.is_some_and(|l| host.source_ordinal() <= l) {
        return Err(fail(Fail::Refused("This Hayate is not newer than the Task Manager launcher; nothing was changed".into())));
    }
    host.precheck(launcher).map_err(|e| fail(Fail::Untrusted(e)))?;
    host.stage_launcher(launcher, &src).map_err(|e| fail(Fail::Io(e)))?;
    if let Err(e) = host.swap_launcher(launcher) {
        if host.launcher_exists(launcher) {
            return Err(fail(Fail::Io(format!("Cannot swap the launcher: {e}"))));
        }
        // the swap lost the file: the old one must come back before anything else
        return Err(back(host, target, launcher, native, format!("the swap failed ({e})"), &cur));
    }
    match check_launcher(host, target, launcher, native).and_then(|()| is_image(host, launcher, &src)) {
        Ok(()) => Ok(Refreshed::Done),
        Err(e) => Err(back(host, target, launcher, native, e, &cur)),
    }
}

/// Put the previous launcher back and prove it is the previous image and works; anything less removes the redirect.
fn back(host: &mut dyn Host, target: &str, launcher: &str, native: Option<&str>, why: String, prev_sha: &str) -> Step {
    if let Err(x) = host.restore_launcher(launcher) {
        return Step::Remove(format!("{why}; restoring the previous launcher failed: {x}"));
    }
    match check_launcher(host, target, launcher, native).and_then(|()| is_image(host, launcher, prev_sha)) {
        Ok(()) => Step::Fail(Fail::Kept(why)),
        Err(x) => Step::Remove(format!("{why}; the restored launcher failed too: {x}")),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Off,
    On,
    /// our value is there but the launcher is missing or not trusted: Task Manager is broken
    Stale(String),
    /// state says on, the value is gone (Windows or a cleaner reset it)
    Reset,
    /// someone else's `Debugger`
    Foreign(String),
    /// off, but the launcher file could not be removed (it was running); the next switch-off or uninstall removes it
    PendingRemoval,
}

pub fn status(reg: &dyn Registry, host: &dyn Host, target: &str, program_files: &str) -> Status {
    let (Some(target), Some(launcher)) = (valid_target(target), expected_launcher(program_files)) else { return Status::Off };
    let cur = reg.get(&ifeo_key(&target), "Debugger").ok().flatten();
    let st = read_state(reg, &target);
    if let Err(e) = state_usable(host, st.as_ref(), &launcher) {
        // untrusted state: report it, whatever the value says (never "on", never deletable)
        return match cur {
            Some(v) if is_ours(&v, &launcher) || st.is_some() => Status::Stale(e),
            Some(v) => Status::Foreign(v),
            None => off_status(host, &launcher),
        };
    }
    match cur {
        Some(v) if is_ours_any(&v, &launcher, st.as_ref()) => match host.launcher_trusted(v.trim().trim_matches('"')) {
            Ok(()) if st.is_some_and(|s| s.enabled) => Status::On,
            Ok(()) => Status::Stale("enable did not finish".into()),
            Err(e) => Status::Stale(e),
        },
        Some(v) => Status::Foreign(v),
        None if st.is_some() => Status::Reset,
        None => off_status(host, &launcher),
    }
}

/// Off, unless an earlier switch-off left the (then running) launcher file behind.
fn off_status(host: &dyn Host, launcher: &str) -> Status {
    if host.launcher_exists(launcher) {
        Status::PendingRemoval
    } else {
        Status::Off
    }
}

/// What a Blink uninstall must do first.
#[derive(Clone, Debug, PartialEq)]
pub enum UninstallStep {
    Nothing,
    /// turn the redirect off (UAC) before anything is deleted
    TurnOff,
    /// a `Debugger` value that mentions our launcher but is not exactly our entry: Blink will not guess
    /// (deleting the launcher could leave no Task Manager); nothing may be removed
    Refuse(String),
}

/// A foreign `Debugger` that still names our launcher (`blink-tm.exe`, or any `\Blink\` folder).
pub fn mentions_us(value: &str) -> bool {
    let v = value.to_ascii_lowercase().replace('/', "\\");
    v.contains("blink-tm.exe") || v.contains(r"\blink\")
}

pub fn uninstall_step(s: &Status) -> UninstallStep {
    match s {
        Status::On | Status::Stale(_) | Status::Reset => UninstallStep::TurnOff,
        Status::Foreign(v) if mentions_us(v) => UninstallStep::Refuse(format!(
            "The Task Manager setting in Windows ({v}) points at Hayate but is not Hayate's own entry, so Hayate will not guess. Nothing was removed. Turn it off in Hayate's Settings, or delete that value as administrator."
        )),
        Status::Off | Status::Foreign(_) | Status::PendingRemoval => UninstallStep::Nothing,
    }
}

/// What the uninstall gate does with `step`. `user_exe`: this Blink runs from the per-user folder, which
/// the user (and anything running as them) can write: it must never be started as administrator.
#[derive(Debug, PartialEq)]
pub enum GatePlan {
    Proceed,
    /// ask the elevated helper (Program Files copy) to turn the redirect off
    TurnOff,
    Refuse(String),
}

pub fn gate_plan(step: UninstallStep, user_exe: bool) -> GatePlan {
    match step {
        UninstallStep::Nothing => GatePlan::Proceed,
        UninstallStep::Refuse(m) => GatePlan::Refuse(m),
        UninstallStep::TurnOff if user_exe => GatePlan::Refuse("Hayate still redirects Task Manager. Turn Task Manager off in Hayate's Settings first, then uninstall. Nothing was removed.".into()),
        UninstallStep::TurnOff => GatePlan::TurnOff,
    }
}

/// Uninstall decision over every redirect of ours (staging ones in debug builds).
pub fn uninstall_needed(reg: &dyn Registry, host: &dyn Host, program_files: &str, include_staging: bool) -> UninstallStep {
    let mut targets = vec![REAL_TARGET.to_string()];
    if include_staging {
        targets.extend(staging_targets(reg));
    }
    let steps: Vec<UninstallStep> = targets.iter().map(|t| uninstall_step(&status(reg, host, t, program_files))).collect();
    if let Some(r) = steps.iter().find(|s| matches!(s, UninstallStep::Refuse(_))) {
        r.clone()
    } else if steps.contains(&UninstallStep::TurnOff) {
        UninstallStep::TurnOff
    } else {
        UninstallStep::Nothing
    }
}

// ---- one helper run at a time ----------------------------------------------------

/// The Settings switch runs the elevated helper off the UI thread (it takes seconds: file
/// copy, Defender, probe). One run at a time: a second click while it works does nothing.
pub struct Gate(std::sync::atomic::AtomicBool);

impl Gate {
    pub const fn new() -> Gate {
        Gate(std::sync::atomic::AtomicBool::new(false))
    }
    /// true: the caller owns the run and must `leave` when it is over
    pub fn enter(&self) -> bool {
        !self.0.swap(true, std::sync::atomic::Ordering::AcqRel)
    }
    pub fn leave(&self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
    pub fn busy(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

// ---- launcher decision ---------------------------------------------------------

pub struct LaunchCtx {
    /// command line after the launcher's own path
    pub args: Vec<String>,
    pub shift: bool,
    pub ctrl: bool,
    pub guard_set: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Why {
    Disabled,
    Shift,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// a launcher started by a launcher: start nothing
    Abort,
    SelfTest,
    Native(Why),
    /// message the running Blink; native Task Manager when that fails
    Show,
}

pub fn plan(c: &LaunchCtx, state: Option<&State>) -> Plan {
    if c.guard_set {
        return Plan::Abort;
    }
    if c.args.iter().any(|a| a == SELF_TEST_ARG) {
        return Plan::SelfTest;
    }
    if !state.is_some_and(|s| s.enabled) {
        return Plan::Native(Why::Disabled);
    }
    // Shift alone: Ctrl+Shift+Esc arrives with Shift still down, that is not an escape
    if c.shift && !c.ctrl {
        return Plan::Native(Why::Shift);
    }
    Plan::Show
}

/// The image the shell asked for, from the first argument's file name; only
/// ever a key into admin-owned state, never a path to run.
pub fn target_from_args(args: &[String]) -> String {
    args.first().and_then(|a| valid_target(crate_file_name(a.trim_matches('"')))).unwrap_or_else(|| REAL_TARGET.to_string())
}

/// May the launcher message this window? Only the Blink recorded at enable time, or
/// the launcher's own image (a Blink the launcher cold-started, see `launch`).
pub fn blink_window_ok(window_image: &str, recorded: &str, launcher: &str) -> bool {
    let w = norm(window_image);
    [recorded, launcher].iter().any(|k| !k.trim().is_empty() && norm(k) == w)
}

// ---- cold start (docs/task-manager-replacement.md §7.3) ---------------------------------

/// How long a cold-started Blink has to show its window before the launcher gives up.
pub const COLD_WAIT_MS: u32 = 3000;

/// What the launcher finds when it looks for a Blink window.
#[derive(Clone, Debug, PartialEq)]
pub enum Seen {
    /// answered within the budget and was shown in front
    Shown,
    /// a window of ours exists and does not answer (a second Blink cannot start next to it)
    Hung,
    Absent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeWhy {
    Disabled,
    Shift,
    /// the launcher is not elevated or its file / folder chain is not admin-only
    Untrusted,
    Hung,
    StartFailed,
    /// started, but no window within `COLD_WAIT_MS` (the started process is ended again)
    NoWindow,
    /// the launcher is not on the interactive desktop (Ctrl+Alt+Del / UAC / lock screen)
    SecureDesktop,
    /// another launcher may be cold-starting a Blink: the lock wait timed out
    Busy,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Via {
    /// a running Blink was shown
    Shown,
    /// no Blink was running: the launcher started its own protected image and its window appeared
    ColdStarted,
    Native(NativeWhy),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Done {
    pub via: Via,
    /// something is on screen (or on its way): false only when even the native start failed
    pub ok: bool,
}

/// The machine, as the launcher sees it. The real one is in `mod.rs`.
pub trait LaunchHost {
    /// the launcher runs on the interactive desktop (not Winlogon / screen saver)
    fn on_interactive_desktop(&self) -> bool;
    /// this launcher holds the cold-start lock (false: the wait timed out or the lock is unusable)
    fn lock_held(&self) -> bool;
    /// elevated and the launcher file + every folder above it admin-only
    fn trusted(&self) -> Result<(), String>;
    /// look for a Blink window (recorded Blink or the launcher's own image), message it
    fn show_running(&mut self) -> Seen;
    /// Re-verify the launcher file and start it as a normal Blink with a clean command line
    /// and environment. Never from any path but the verified launcher.
    fn start_blink(&mut self) -> Result<(), String>;
    /// poll for the started Blink's window for up to `ms`; true once it was shown
    fn wait_window(&mut self, ms: u32) -> bool;
    /// end the process `start_blink` made (its window never came)
    fn stop_started(&mut self);
    fn start_native(&mut self) -> bool;
}

/// The launcher's whole decision after `plan`. Order matters: trust first (an untrusted
/// launcher neither messages nor starts anything), then a running Blink, and only a
/// Blink that is *absent* is cold-started; a hung one cannot be (the single-instance check
/// would hand over to it), and the native Task Manager is the answer to every failure.
pub fn launch(plan: &Plan, h: &mut dyn LaunchHost) -> Done {
    fn native(h: &mut dyn LaunchHost, why: NativeWhy) -> Done {
        Done { ok: h.start_native(), via: Via::Native(why) }
    }
    match plan {
        Plan::Native(Why::Shift) => native(h, NativeWhy::Shift),
        Plan::Native(Why::Disabled) => native(h, NativeWhy::Disabled),
        // the caller handles these before; starting nothing is the safe reading
        Plan::Abort | Plan::SelfTest => Done { via: Via::Native(NativeWhy::Disabled), ok: false },
        Plan::Show => {
            if !h.on_interactive_desktop() {
                return native(h, NativeWhy::SecureDesktop);
            }
            if h.trusted().is_err() {
                return native(h, NativeWhy::Untrusted);
            }
            match h.show_running() {
                Seen::Shown => return Done { via: Via::Shown, ok: true },
                Seen::Hung => return native(h, NativeWhy::Hung),
                Seen::Absent => {}
            }
            // never start a second Blink while another launcher may be starting one: the lock
            // wait timed out, so look once more and otherwise leave it to the native Task Manager
            if !h.lock_held() {
                return match h.show_running() {
                    Seen::Shown => Done { via: Via::Shown, ok: true },
                    _ => native(h, NativeWhy::Busy),
                };
            }
            if h.start_blink().is_err() {
                return native(h, NativeWhy::StartFailed);
            }
            if h.wait_window(COLD_WAIT_MS) {
                return Done { via: Via::ColdStarted, ok: true };
            }
            h.stop_started();
            native(h, NativeWhy::NoWindow)
        }
    }
}

pub fn exit_code(d: &Done) -> i32 {
    if d.ok {
        EXIT_OK
    } else {
        EXIT_NATIVE_FAILED
    }
}

/// A launcher-named process whose parent is this very protected image was started by the
/// launcher's cold start: it runs as the app. Nothing in argv or the environment can say
/// so (both are the caller's to write); a stranger who starts the protected exe by hand
/// gets the launcher path, and a parent he spoofs would need a handle to an elevated process.
pub fn cold_child(parent_image: Option<&str>, own_exe: &str) -> bool {
    parent_image.is_some_and(|p| !p.trim().is_empty() && norm(p) == norm(own_exe))
}

/// Caller variables a cold-started Blink may keep: none of them names a path, a program or
/// a library to load (they are hardware / account labels). Everything else the caller set
/// is dropped (`ProgramFiles`, `PATH`, `COMSPEC`, `PSModulePath`, `_NT_SYMBOL_PATH`, `JAVA_TOOL_OPTIONS`, ...).
pub const PASS_THROUGH: &[&str] = &["NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "PROCESSOR_IDENTIFIER", "PROCESSOR_LEVEL", "PROCESSOR_REVISION", "OS", "USERNAME", "COMPUTERNAME", "USERDOMAIN"];

/// Folders of the cold-started Blink's environment: known folders and the Windows folder,
/// never the environment the caller handed the elevated launcher.
pub struct SystemFolders<'a> {
    pub local: &'a str,
    pub roaming: &'a str,
    pub profile: &'a str,
    pub root: &'a str,
    pub program_files: &'a str,
    pub program_files_x86: &'a str,
    pub program_data: &'a str,
}

pub fn system_vars(f: &SystemFolders) -> Vec<(String, String)> {
    let kv = |k: &str, v: String| (k.to_string(), v);
    let root = f.root.trim_end_matches('\\');
    let drive: String = root.chars().take(2).collect();
    let home_path = f.profile.get(2..).unwrap_or("").to_string();
    vec![
        kv("LOCALAPPDATA", f.local.into()),
        kv("APPDATA", f.roaming.into()),
        kv("USERPROFILE", f.profile.into()),
        kv("HOMEDRIVE", f.profile.chars().take(2).collect()),
        kv("HOMEPATH", home_path),
        kv("TEMP", format!(r"{}\Temp", f.local)),
        kv("TMP", format!(r"{}\Temp", f.local)),
        kv("SystemRoot", root.into()),
        kv("windir", root.into()),
        kv("SystemDrive", drive),
        kv("ProgramFiles", f.program_files.into()),
        kv("ProgramW6432", f.program_files.into()),
        kv("ProgramFiles(x86)", f.program_files_x86.into()),
        kv("CommonProgramFiles", format!(r"{}\Common Files", f.program_files)),
        kv("ProgramData", f.program_data.into()),
        kv("ALLUSERSPROFILE", f.program_data.into()),
        kv("COMSPEC", format!(r"{root}\System32\cmd.exe")),
        kv("PATHEXT", ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC".into()),
        kv("PATH", format!(r"{root}\System32;{root};{root}\System32\Wbem")),
    ]
}

/// The environment of the cold-started Blink: a WHITELIST. The system's variables
/// (`system_vars`) plus the harmless `PASS_THROUGH` ones of the caller. Every other variable
/// is dropped (`BLINK_*` included): the caller's environment is the medium user's to write
/// (HKCU\Environment) and the process is elevated.
pub fn child_env(parent: &[(String, String)], system: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = parent
        .iter()
        .filter(|(k, _)| PASS_THROUGH.iter().any(|p| p.eq_ignore_ascii_case(k)) && !system.iter().any(|(s, _)| s.eq_ignore_ascii_case(k)))
        .cloned()
        .collect();
    out.extend(system.iter().cloned());
    out
}

/// The launcher may look for / start a Blink only on the interactive desktop. On the
/// Winlogon (secure) desktop (Ctrl+Alt+Del, UAC, lock screen) no Blink window is ever
/// found, and a started one would be invisible and elevated.
pub fn interactive_desktop(name: &str) -> bool {
    name.eq_ignore_ascii_case("Default")
}

/// Result of one try to open the cold-start lock file exclusively.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LockTry {
    /// opened with share mode 0: we hold the lock
    Got,
    /// another cold start holds it (ERROR_SHARING_VIOLATION 32 / ERROR_LOCK_VIOLATION 33): wait and retry
    Busy,
    /// cannot be used at all (folder missing, access denied, ...): no lock, never retried
    Unusable,
}

pub fn lock_try(win32_error: u32) -> LockTry {
    match win32_error {
        0 => LockTry::Got,
        32 | 33 => LockTry::Busy,
        _ => LockTry::Unusable,
    }
}

/// DACL the cold-start lock file is created with: SYSTEM and Administrators only, inheritance
/// blocked (`P`). A medium user (even one that reads the folder) cannot open it at all, so it
/// cannot hold it either. `D:PAI(A;;FA;;;SY)(A;;FA;;;BA)`.
pub const LOCK_SDDL: &str = "D:PAI(A;;FA;;;SY)(A;;FA;;;BA)";

/// What to do with a lock file that already exists, from its owner and access list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LockFile {
    /// owner and every allow entry are SYSTEM / Administrators / TrustedInstaller: use it
    Use,
    /// anyone else can open it (an inherited Users entry, a planted file): delete it and create it again
    Recreate,
}

/// Policy for an existing `.coldstart.lock`: any allow entry for a non-admin SID (read access
/// included: read + share None is enough to hold it), an unknown owner, or no access list at
/// all means it is not ours.
pub fn lock_file_policy(owner: &str, aces: Option<&[Ace]>) -> LockFile {
    let admin = |s: &str| [SID_SYSTEM, SID_ADMINS, SID_TRUSTED_INSTALLER].contains(&s);
    let Some(aces) = aces else { return LockFile::Recreate };
    // an empty DACL grants nobody anything (not even admins: the open fails), so we could never
    // take the lock and the launcher would go native for ever: replace it
    if aces.is_empty() {
        return LockFile::Recreate;
    }
    if !admin(owner) || aces.iter().any(|a| a.allow && !admin(&a.sid)) {
        LockFile::Recreate
    } else {
        LockFile::Use
    }
}

// ---- ACL policy of the launcher file and everything above it ---------------------

pub const SID_SYSTEM: &str = "S-1-5-18";
pub const SID_ADMINS: &str = "S-1-5-32-544";
pub const SID_TRUSTED_INSTALLER: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

#[derive(Clone, Debug)]
pub struct Ace {
    pub sid: String,
    pub mask: u32,
    pub allow: bool,
    pub inherit_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    /// the launcher file
    File,
    /// the folder holding it (and any below Program Files)
    Dir,
    /// Program Files and above: only what could rename or delete the folder below
    Ancestor,
}

const WRITE_DATA: u32 = 0x2;
const APPEND_DATA: u32 = 0x4;
const DELETE_CHILD: u32 = 0x40;
const DELETE: u32 = 0x10000;
const WRITE_DAC: u32 = 0x40000;
const WRITE_OWNER: u32 = 0x80000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;

pub fn level_for(path: &str, file: &str, program_files: &str) -> Level {
    let (p, f, pf) = (norm(path), norm(file), norm(program_files));
    if p == f {
        Level::File
    } else if p.starts_with(&format!("{pf}\\")) {
        Level::Dir
    } else {
        Level::Ancestor
    }
}

/// None: fine. Some(why): a non-admin could change the launcher.
pub fn acl_problem(level: Level, owner: &str, aces: Option<&[Ace]>) -> Option<String> {
    if ![SID_SYSTEM, SID_ADMINS, SID_TRUSTED_INSTALLER].contains(&owner) {
        return Some(format!("owned by {owner}, not an administrator account"));
    }
    let Some(aces) = aces else { return Some("no access list (everyone has full access)".into()) };
    let bad = match level {
        Level::File => WRITE_DATA | APPEND_DATA | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL,
        Level::Dir => WRITE_DATA | APPEND_DATA | DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_WRITE | GENERIC_ALL,
        Level::Ancestor => DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL,
    };
    aces.iter()
        .find(|a| a.allow && !a.inherit_only && a.mask & bad != 0 && ![SID_SYSTEM, SID_ADMINS, SID_TRUSTED_INSTALLER].contains(&a.sid.as_str()))
        .map(|a| format!("{} may change it (access {:#x})", a.sid, a.mask & bad))
}

// ---- tests -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const PF: &str = r"C:\Program Files";
    const LAUNCHER: &str = r"C:\Program Files\Blink\blink-tm.exe";
    const BLINK: &str = r"C:\Users\u\AppData\Local\Programs\Blink\blink.exe";

    #[derive(Default)]
    struct Mem {
        vals: BTreeMap<(String, String), String>,
        keys: Vec<String>,
        /// every mutation, in order: "set key|name=v", "delv key|name", "delk key"
        log: Vec<String>,
        fail_set: Option<String>,
        /// fail a set of this name with this value
        fail_set_val: Option<(String, String)>,
        /// before a set of `name=value`, another tool writes (key, name, value)
        race: Option<(String, (String, String, String))>,
    }
    impl Mem {
        fn ensure(&mut self, key: &str) {
            let k = key.to_lowercase();
            if !self.keys.contains(&k) {
                self.keys.push(k);
            }
        }
    }
    impl Registry for Mem {
        fn get(&self, key: &str, name: &str) -> Result<Option<String>, String> {
            Ok(self.vals.get(&(key.to_lowercase(), name.to_string())).cloned())
        }
        fn set(&mut self, key: &str, name: &str, v: &str) -> Result<(), String> {
            if self.fail_set.as_deref() == Some(name) {
                return Err(format!("injected failure writing {name}"));
            }
            if self.race.as_ref().is_some_and(|(t, _)| t == &format!("{name}={v}")) {
                let (_, (k, n, x)) = self.race.take().unwrap();
                self.ensure(&k);
                self.vals.insert((k.to_lowercase(), n), x);
            }
            if self.fail_set_val.as_ref().is_some_and(|(n, x)| n == name && x == v) {
                return Err(format!("injected failure writing {name}={v}"));
            }
            self.log.push(format!("set {key}|{name}={v}"));
            self.ensure(key);
            self.vals.insert((key.to_lowercase(), name.to_string()), v.to_string());
            Ok(())
        }
        fn delete_value(&mut self, key: &str, name: &str) -> Result<(), String> {
            self.log.push(format!("delv {key}|{name}"));
            self.vals.remove(&(key.to_lowercase(), name.to_string()));
            Ok(())
        }
        fn delete_key(&mut self, key: &str) -> Result<(), String> {
            self.log.push(format!("delk {key}"));
            let k = key.to_lowercase();
            self.vals.retain(|(kk, _), _| !(kk == &k || kk.starts_with(&format!("{k}\\"))));
            self.keys.retain(|kk| !(kk == &k || kk.starts_with(&format!("{k}\\"))));
            Ok(())
        }
        fn is_empty(&self, key: &str) -> bool {
            let k = key.to_lowercase();
            self.keys.contains(&k) && !self.vals.keys().any(|(kk, _)| kk == &k) && !self.keys.iter().any(|kk| kk.starts_with(&format!("{k}\\")))
        }
        fn subkeys(&self, key: &str) -> Vec<String> {
            let p = format!("{}\\", key.to_lowercase());
            self.keys.iter().filter_map(|k| k.strip_prefix(&p)).map(|s| s.split('\\').next().unwrap().to_string()).collect()
        }
    }

    struct Fake {
        trust: Result<(), String>,
        probe: Result<(), String>,
        pre: Result<(), String>,
        state: Result<(), String>,
        installed: bool,
        removed: bool,
        removed_paths: Vec<String>,
        /// the launcher file cannot be removed (it is running)
        stuck: bool,
        // ---- refresh ----
        lock: Result<(), String>,
        src_sha: Result<String, String>,
        file_sha: Result<String, String>,
        stage: Result<(), String>,
        swap: Result<(), String>,
        restore: Result<(), String>,
        /// the launcher file is there (None: the removal bookkeeping decides, as before)
        present: Option<bool>,
        /// answers of the next probes (then `probe`)
        probes: Vec<Result<(), String>>,
        /// every host call of the refresh, in order
        calls: Vec<String>,
        /// a rollback copy is there
        old: bool,
        /// build ordinal of the helper's image
        build: u64,
        /// sha of the file at the launcher path after a swap/restore (None: as the swap/restore implies)
        live: Option<String>,
        /// what the file at the launcher path hashes to after a successful swap (None: the staged image)
        swap_lands: Option<String>,
        /// a failed swap leaves no file at the launcher path
        lose_on_swap: bool,
    }
    impl Fake {
        fn ok() -> Fake {
            Fake {
                trust: Ok(()),
                probe: Ok(()),
                pre: Ok(()),
                state: Ok(()),
                installed: false,
                removed: false,
                removed_paths: Vec::new(),
                stuck: false,
                lock: Ok(()),
                src_sha: Ok("bbbb".into()),
                file_sha: Ok("aaaa".into()),
                stage: Ok(()),
                swap: Ok(()),
                restore: Ok(()),
                present: None,
                probes: Vec::new(),
                calls: Vec::new(),
                old: true,
                build: 1,
                live: None,
                swap_lands: None,
                lose_on_swap: false,
            }
        }
    }
    impl Host for Fake {
        fn precheck(&self, _: &str) -> Result<(), String> {
            self.pre.clone()
        }
        fn launcher_trusted(&self, _: &str) -> Result<(), String> {
            self.trust.clone()
        }
        fn state_trusted(&self) -> Result<(), String> {
            self.state.clone()
        }
        fn install_launcher(&mut self, _: &str) -> Result<(), String> {
            self.installed = true;
            Ok(())
        }
        fn remove_launcher(&mut self, p: &str) -> bool {
            self.removed = true;
            self.removed_paths.push(p.to_string());
            !self.stuck
        }
        fn launcher_exists(&self, _: &str) -> bool {
            self.present.unwrap_or(self.stuck && self.removed)
        }
        fn old_exists(&self, _: &str) -> bool {
            self.old
        }
        fn source_ordinal(&self) -> u64 {
            self.build
        }
        fn probe(&mut self, _: &str, _: Option<&str>) -> Result<(), String> {
            self.calls.push("probe".into());
            if self.probes.is_empty() {
                self.probe.clone()
            } else {
                self.probes.remove(0)
            }
        }
        fn lock_cold_start(&mut self) -> Result<(), String> {
            self.calls.push("lock".into());
            self.lock.clone()
        }
        fn unlock_cold_start(&mut self) {
            self.calls.push("unlock".into());
        }
        fn source_sha(&self) -> Result<String, String> {
            self.src_sha.clone()
        }
        fn launcher_sha(&self, _: &str) -> Result<String, String> {
            match &self.live {
                Some(l) => Ok(l.clone()),
                None => self.file_sha.clone(),
            }
        }
        fn stage_launcher(&mut self, _: &str, sha: &str) -> Result<(), String> {
            self.calls.push(format!("stage {sha}"));
            self.stage.clone()
        }
        fn swap_launcher(&mut self, _: &str) -> Result<(), String> {
            self.calls.push("swap".into());
            if self.swap.is_ok() {
                self.live = Some(self.swap_lands.clone().unwrap_or_else(|| self.src_sha.clone().unwrap()));
                self.present = Some(true);
            } else if self.lose_on_swap {
                self.present = Some(false);
            }
            self.swap.clone()
        }
        fn restore_launcher(&mut self, _: &str) -> Result<(), String> {
            self.calls.push("restore".into());
            if self.restore.is_ok() {
                self.live = None;
                self.present = Some(true);
            }
            self.restore.clone()
        }
    }

    const SUR: &str = "blink-surrogate-tm.exe";
    const SUR_IMG: &str = r"C:\stage\blink-surrogate-tm.exe";

    fn on(reg: &mut Mem, host: &mut Fake) {
        assert_eq!(enable(reg, host, SUR, PF, BLINK, Some(SUR_IMG)), Ok(Outcome::Enabled));
    }

    #[test]
    fn toggle_on_writes_value_and_state_in_order() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        on(&mut r, &mut h);
        let ik = ifeo_key(SUR);
        assert_eq!(r.get(&ik, "Debugger").unwrap().as_deref(), Some(format!("\"{LAUNCHER}\"").as_str()));
        let s = read_state(&r, SUR).unwrap();
        assert!(s.enabled && s.launcher == LAUNCHER && s.blink_path == BLINK && s.native_path.as_deref() == Some(SUR_IMG));
        assert!(h.installed);
        // write order: state Enabled=0, then the Debugger, then Enabled=1 last
        let pos = |needle: &str| r.log.iter().position(|l| l.contains(needle)).unwrap();
        let sk = state_key(SUR);
        assert!(pos(&format!("{sk}|Enabled=0")) < pos("|Debugger=") && pos("|Debugger=") < pos(&format!("{sk}|Enabled=1")));
        assert_eq!(r.log.last().unwrap(), &format!("set {sk}|Enabled=1"));
        // enabling again changes nothing
        let n = r.log.len();
        assert_eq!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Ok(Outcome::AlreadyOn));
        assert_eq!(r.log.len(), n);
    }

    #[test]
    fn toggle_off_removes_everything_and_the_launcher() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        on(&mut r, &mut h);
        assert_eq!(disable(&mut r, &mut h, SUR, PF), Ok(Outcome::Disabled));
        assert!(r.vals.is_empty(), "left behind: {:?}", r.vals);
        assert!(!r.keys.contains(&ifeo_key(SUR).to_lowercase()) && !r.keys.iter().any(|k| k.starts_with(&STATE_KEY.to_lowercase())));
        assert!(h.removed);
        // removal order: the Debugger value goes before the state is cleared
        let pos = |n: &str| r.log.iter().position(|l| l.contains(n)).unwrap();
        assert!(pos("delv") < pos(&format!("delk {}", state_key(SUR))));
        assert_eq!(disable(&mut r, &mut h, SUR, PF), Ok(Outcome::AlreadyOff));
    }

    #[test]
    fn launcher_stays_while_another_target_needs_it() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        on(&mut r, &mut h);
        assert_eq!(enable(&mut r, &mut h, "blink-surrogate-b.exe", PF, BLINK, Some(r"C:\stage\blink-surrogate-b.exe")), Ok(Outcome::Enabled));
        assert_eq!(disable(&mut r, &mut h, SUR, PF), Ok(Outcome::Disabled));
        assert!(!h.removed);
        assert_eq!(disable(&mut r, &mut h, "blink-surrogate-b.exe", PF), Ok(Outcome::Disabled));
        assert!(h.removed);
    }

    #[test]
    fn foreign_debugger_is_never_touched() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        let ik = ifeo_key(SUR);
        r.set(&ik, "Debugger", r"C:\tools\other.exe").unwrap();
        r.log.clear();
        assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Err(Fail::Foreign(_))));
        assert!(r.log.is_empty() && !h.installed);
        assert_eq!(disable(&mut r, &mut h, SUR, PF), Ok(Outcome::ForeignKept));
        assert_eq!(r.get(&ik, "Debugger").unwrap().as_deref(), Some(r"C:\tools\other.exe"));
        assert!(matches!(status(&r, &h, SUR, PF), Status::Foreign(_)));
    }

    #[test]
    fn failed_probe_rolls_back_exactly() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        h.probe = Err("no answer".into());
        let e = enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).unwrap_err();
        assert!(matches!(e, Fail::Probe(_)) && e.exit_code() == EXIT_PROBE);
        assert!(r.vals.is_empty() && r.keys.is_empty() || r.keys.iter().all(|k| !r.vals.keys().any(|(kk, _)| kk == k)), "state left: {:?}", r.vals);
        assert_eq!(r.get(&ifeo_key(SUR), "Debugger").unwrap(), None);
        assert_eq!(status(&r, &h, SUR, PF), Status::Off);
    }

    #[test]
    fn failed_write_rolls_back_too() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        r.fail_set = Some("Enabled".into());
        assert!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).is_err());
        assert_eq!(r.get(&ifeo_key(SUR), "Debugger").unwrap(), None);
        assert!(read_state(&r, SUR).is_none());
    }

    #[test]
    fn untrusted_launcher_writes_nothing() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        h.trust = Err("Users may write".into());
        let e = enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).unwrap_err();
        assert!(matches!(e, Fail::Untrusted(_)));
        assert!(r.log.is_empty());
    }

    #[test]
    fn only_program_files_launcher_path() {
        assert_eq!(expected_launcher(r"C:\Program Files").as_deref(), Some(LAUNCHER));
        assert_eq!(expected_launcher(r"C:\Program Files\").as_deref(), Some(LAUNCHER));
        assert_eq!(expected_launcher(""), None);
        assert_eq!(expected_launcher(r"\\server\share"), None);
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        assert!(matches!(enable(&mut r, &mut h, SUR, "", BLINK, Some(SUR_IMG)), Err(Fail::Refused(_))));
        assert!(r.log.is_empty());
        // the value is exactly the quoted launcher: nothing else ever reads as ours
        assert!(is_ours(&debugger_value(LAUNCHER), LAUNCHER));
        assert!(!is_ours(r"C:\Users\u\blink.exe", LAUNCHER));
        assert!(!is_ours(&format!("\"{LAUNCHER}\" --extra"), LAUNCHER));
        assert!(!is_ours(LAUNCHER, LAUNCHER), "unquoted is not what Hayate writes");
    }

    #[test]
    fn targets_are_whitelisted() {
        for ok in ["taskmgr.exe", "TaskMgr.EXE", "blink-surrogate-tm.exe", "blink-surrogate-1.exe"] {
            assert!(valid_target(ok).is_some(), "{ok}");
        }
        assert!(valid_target("notepad.exe").is_none() && valid_target("cmd.exe").is_none() && valid_target(r"..\taskmgr.exe").is_none());
        assert!(valid_target(r"C:\Windows\System32\taskmgr.exe").is_none() && valid_target("blink-surrogate-.exe").is_none());
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        assert_eq!(enable(&mut r, &mut h, "notepad.exe", PF, BLINK, None), Err(Fail::BadTarget));
        assert_eq!(disable(&mut r, &mut h, "winlogon.exe", PF), Err(Fail::BadTarget));
        assert!(r.log.is_empty());
    }

    #[test]
    fn staging_target_needs_matching_image() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, None), Err(Fail::Refused(_))));
        assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(r"C:\stage\other.exe")), Err(Fail::Refused(_))));
        assert!(matches!(enable(&mut r, &mut h, REAL_TARGET, PF, BLINK, Some(r"C:\x\taskmgr.exe")), Err(Fail::Refused(_))));
        assert!(r.log.is_empty());
    }

    #[test]
    fn real_target_uses_only_its_own_keys() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        assert_eq!(enable(&mut r, &mut h, "TASKMGR.EXE", PF, BLINK, None), Ok(Outcome::Enabled));
        let ok = |l: &String| l.contains(&ifeo_key(REAL_TARGET)) || l.contains(&state_key(REAL_TARGET)) || l == &format!("delk {STATE_KEY}");
        assert!(r.log.iter().all(ok), "{:?}", r.log);
        assert_eq!(disable(&mut r, &mut h, REAL_TARGET, PF), Ok(Outcome::Disabled));
        assert!(r.log.iter().all(ok), "{:?}", r.log);
        assert!(r.vals.is_empty());
    }

    #[test]
    fn status_reports_every_state() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        assert_eq!(status(&r, &h, SUR, PF), Status::Off);
        on(&mut r, &mut h);
        assert_eq!(status(&r, &h, SUR, PF), Status::On);
        h.trust = Err("file missing".into());
        assert!(matches!(status(&r, &h, SUR, PF), Status::Stale(_)));
        h.trust = Ok(());
        r.delete_value(&ifeo_key(SUR), "Debugger").unwrap();
        assert_eq!(status(&r, &h, SUR, PF), Status::Reset);
        // disabling after a reset just tidies the state
        assert_eq!(disable(&mut r, &mut h, SUR, PF), Ok(Outcome::Disabled));
        assert_eq!(status(&r, &h, SUR, PF), Status::Off);
    }

    #[test]
    fn uninstall_turns_it_off_first() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        assert_eq!(uninstall_step(&status(&r, &h, SUR, PF)), UninstallStep::Nothing);
        on(&mut r, &mut h);
        assert_eq!(uninstall_step(&status(&r, &h, SUR, PF)), UninstallStep::TurnOff);
        h.trust = Err("gone".into());
        assert_eq!(uninstall_step(&status(&r, &h, SUR, PF)), UninstallStep::TurnOff);
        // after the uninstall's disable nothing of ours is left
        assert_eq!(disable(&mut r, &mut h, SUR, PF), Ok(Outcome::Disabled));
        assert!(r.vals.is_empty());
        assert_eq!(uninstall_step(&Status::Foreign("x".into())), UninstallStep::Nothing);
    }

    #[test]
    fn uninstall_refuses_a_lookalike_and_never_elevates_a_user_exe() {
        // another tool's value is left alone
        assert_eq!(uninstall_step(&Status::Foreign(r#""C:\Tools\procexp.exe""#.into())), UninstallStep::Nothing);
        // a value that mentions our launcher (extra args, other folder, unquoted) stops the uninstall
        for v in [r#""C:\Program Files\Blink\blink-tm.exe" --x"#, r"C:\Program Files\Blink\blink-tm.exe", r#""D:\Apps\Blink\foo.exe""#, r#""C:\Other\BLINK-TM.EXE""#] {
            assert!(matches!(uninstall_step(&Status::Foreign(v.into())), UninstallStep::Refuse(_)), "{v}");
        }
        assert!(mentions_us("C:/x/Blink/y.exe") && !mentions_us(r"C:\Tools\blinkx.exe"));
        // through the whole decision (real Mem registry): a lookalike Debugger refuses
        let mut r = Mem::default();
        let h = Fake::ok();
        r.set(&ifeo_key(SUR), "Debugger", r#""C:\Program Files\Blink\blink-tm.exe" extra"#).unwrap();
        assert!(matches!(status(&r, &h, SUR, PF), Status::Foreign(_)));
        // gate: refuse stays refuse, turn-off from a per-user exe is refused, from Program Files allowed
        assert!(matches!(gate_plan(UninstallStep::Refuse("m".into()), false), GatePlan::Refuse(_)));
        assert!(matches!(gate_plan(UninstallStep::TurnOff, true), GatePlan::Refuse(m) if m.contains("Settings")));
        assert_eq!(gate_plan(UninstallStep::TurnOff, false), GatePlan::TurnOff);
        assert_eq!(gate_plan(UninstallStep::Nothing, true), GatePlan::Proceed);
    }


    fn seed_state(r: &mut Mem, l: &str) {
        let (ik, sk) = (ifeo_key(SUR), state_key(SUR));
        r.set(&ik, "Debugger", &debugger_value(l)).unwrap();
        for (n, v) in [("LauncherPath", l), ("Enabled", "1"), ("BlinkPath", BLINK), ("NativePath", SUR_IMG)] {
            r.set(&sk, n, v).unwrap();
        }
    }

    #[test]
    fn recorded_launcher_must_be_the_program_files_one() {
        let ik = ifeo_key(SUR);
        // our own state, the real launcher: on, and disable removes value, state and file
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        seed_state(&mut r, LAUNCHER);
        assert_eq!(status(&r, &h, SUR, PF), Status::On);
        assert_eq!(uninstall_step(&status(&r, &h, SUR, PF)), UninstallStep::TurnOff);
        assert_eq!(disable(&mut r, &mut h, SUR, PF), Ok(Outcome::Disabled));
        assert!(r.vals.is_empty());
        assert_eq!(h.removed_paths, vec![LAUNCHER.to_string()]);
        // a recorded path anywhere else (moved Program Files, planted state) is untrusted:
        // nothing is deleted, no value touched, and it is reported
        for bad in [r"D:\Program Files\Blink\blink-tm.exe", r"C:\evil\blink-tm.exe", r"C:\Program Files\Blink\other.exe", r"C:\Program Files\Blink\..\x\blink-tm.exe", r"C:\Users\u\blink-tm.exe"] {
            let (mut r, mut h) = (Mem::default(), Fake::ok());
            seed_state(&mut r, bad);
            let before = r.vals.clone();
            assert!(matches!(disable(&mut r, &mut h, SUR, PF), Err(Fail::Untrusted(_))), "{bad}");
            assert_eq!(r.vals, before, "{bad}: nothing changed");
            assert!(h.removed_paths.is_empty(), "{bad}: no file deleted");
            assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Err(Fail::Untrusted(_))), "{bad}");
            assert_eq!(r.vals, before);
            assert!(!h.installed && h.removed_paths.is_empty());
            assert!(matches!(status(&r, &h, SUR, PF), Status::Stale(_)), "{bad}");
            assert!(!is_ours_any(&debugger_value(bad), LAUNCHER, read_state(&r, SUR).as_ref()));
        }
        // without our state a lookalike value is a stranger's
        let mut r2 = Mem::default();
        r2.set(&ik, "Debugger", &debugger_value(r"D:\Program Files\Blink\blink-tm.exe")).unwrap();
        assert!(matches!(status(&r2, &Fake::ok(), SUR, PF), Status::Foreign(_)));
    }

    #[test]
    fn state_key_that_others_can_write_is_untrusted() {
        let mut r = Mem::default();
        seed_state(&mut r, LAUNCHER);
        let mut h = Fake::ok();
        h.state = Err("S-1-5-32-545 may change it".into());
        let (before, logged) = (r.vals.clone(), r.log.len());
        assert!(matches!(disable(&mut r, &mut h, SUR, PF), Err(Fail::Untrusted(_))));
        assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Err(Fail::Untrusted(_))));
        assert_eq!(r.vals, before);
        assert_eq!(r.log.len(), logged, "no registry write");
        assert!(!h.removed && !h.installed);
        assert!(matches!(status(&r, &h, SUR, PF), Status::Stale(_)));
    }

    #[test]
    fn rollback_keeps_the_launcher_a_restored_value_points_at() {
        // our earlier value is there but its state is gone; enable fails: the value is put back,
        // so blink-tm.exe must stay (deleting it would leave no Task Manager)
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        let ik = ifeo_key(SUR);
        r.set(&ik, "Debugger", &debugger_value(LAUNCHER)).unwrap();
        h.probe = Err("no".into());
        assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Err(Fail::Probe(_))));
        assert_eq!(r.get(&ik, "Debugger").unwrap().as_deref(), Some(debugger_value(LAUNCHER).as_str()));
        assert!(h.installed && !h.removed && h.removed_paths.is_empty(), "launcher kept");
        // same when the copy fails
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        r.set(&ik, "Debugger", &debugger_value(LAUNCHER)).unwrap();
        h.trust = Err("x".into());
        assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Err(Fail::Untrusted(_))));
        assert!(!h.removed);
        // a first-time failure (no earlier value) still cleans up
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        h.probe = Err("no".into());
        assert!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).is_err());
        assert!(h.removed);
    }

    #[test]
    fn rollback_restores_every_state_value() {
        let (ik, sk) = (ifeo_key(SUR), state_key(SUR));
        let mut r = Mem::default();
        r.set(&ik, "Debugger", &debugger_value(LAUNCHER)).unwrap();
        // an older state: no NativePath, odd schema, flag 1
        for (n, v) in [("SchemaVersion", "0"), ("Enabled", "0"), ("LauncherPath", LAUNCHER), ("BlinkPath", r"C:\old\blink.exe"), ("PreviousDebuggerExisted", "1")] {
            r.set(&sk, n, v).unwrap();
        }
        let before: BTreeMap<_, _> = r.vals.iter().filter(|((k, _), _)| k.starts_with(&sk.to_lowercase())).map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut h = Fake::ok();
        h.probe = Err("no".into());
        assert!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).is_err());
        let after: BTreeMap<_, _> = r.vals.iter().filter(|((k, _), _)| k.starts_with(&sk.to_lowercase())).map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(after, before, "state identical, NativePath (written by enable) gone again");
        // and a state that had NativePath gets it back
        let mut r = Mem::default();
        r.set(&ik, "Debugger", &debugger_value(LAUNCHER)).unwrap();
        for (n, v) in [("SchemaVersion", "1"), ("Enabled", "0"), ("LauncherPath", LAUNCHER), ("BlinkPath", r"C:\old\blink.exe"), ("PreviousDebuggerExisted", "0"), ("NativePath", r"C:\old\blink-surrogate-tm.exe")] {
            r.set(&sk, n, v).unwrap();
        }
        let before = r.vals.clone();
        let mut h = Fake::ok();
        h.probe = Err("no".into());
        assert!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).is_err());
        assert_eq!(r.vals, before);
    }

    #[test]
    fn recovery_hint_keeps_the_backslash() {
        assert!(RECOVERY_HINT.contains(r"IFEO\taskmgr.exe"), "{RECOVERY_HINT}");
        assert!(!RECOVERY_HINT.contains('\t'));
    }

    #[test]
    fn enable_over_our_old_value_restores_it_on_failure() {
        let old_l = LAUNCHER;
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        let (ik, sk) = (ifeo_key(SUR), state_key(SUR));
        r.set(&ik, "Debugger", &debugger_value(old_l)).unwrap();
        for (n, v) in [("LauncherPath", old_l), ("Enabled", "0"), ("BlinkPath", BLINK)] {
            r.set(&sk, n, v).unwrap();
        }
        h.probe = Err("no".into());
        assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Err(Fail::Probe(_))));
        assert_eq!(r.get(&ik, "Debugger").unwrap().as_deref(), Some(debugger_value(old_l).as_str()), "our previous value is back");
        assert_eq!(read_state(&r, SUR).unwrap().launcher, old_l);
        assert!(!h.removed, "state remains, so does the launcher");
    }

    #[test]
    fn rollback_never_deletes_a_value_that_appeared_meanwhile() {
        // another tool writes a Debugger after the first check but before ours
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        let ik = ifeo_key(SUR);
        r.race = Some(("PreviousDebuggerExisted=0".into(), (ik.clone(), "Debugger".into(), r"C:\tools\other.exe".into())));
        let e = enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).unwrap_err();
        assert!(matches!(e, Fail::Foreign(_)));
        assert_eq!(r.get(&ik, "Debugger").unwrap().as_deref(), Some(r"C:\tools\other.exe"), "theirs is untouched");
        assert!(read_state(&r, SUR).is_none(), "our state is gone");
        assert!(r.log.iter().all(|l| !l.starts_with("delv")), "{:?}", r.log);
        // another tool overwrites after our write, then Enabled=1 fails:
        // the rollback sees a value that is not the one it wrote and leaves it
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        r.race = Some(("Enabled=1".into(), (ik.clone(), "Debugger".into(), r"C:\tools\other.exe".into())));
        r.fail_set_val = Some(("Enabled".into(), "1".into()));
        assert!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).is_err());
        assert_eq!(r.get(&ik, "Debugger").unwrap().as_deref(), Some(r"C:\tools\other.exe"));
        assert!(r.log.iter().all(|l| !l.starts_with("delv")), "{:?}", r.log);
    }

    #[test]
    fn previous_debugger_flag_is_honest() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        on(&mut r, &mut h);
        assert_eq!(r.get(&state_key(SUR), "PreviousDebuggerExisted").unwrap().as_deref(), Some("0"));
        // re-enable over our own earlier, unfinished value records that one existed
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        let (ik, sk) = (ifeo_key(SUR), state_key(SUR));
        r.set(&ik, "Debugger", &debugger_value(LAUNCHER)).unwrap();
        r.set(&sk, "LauncherPath", LAUNCHER).unwrap();
        r.set(&sk, "Enabled", "0").unwrap();
        assert_eq!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Ok(Outcome::Enabled));
        assert_eq!(r.get(&sk, "PreviousDebuggerExisted").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn folder_chain_is_checked_before_anything_is_copied() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        h.pre = Err("Users may write C:\\Program Files\\Blink".into());
        assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Err(Fail::Untrusted(_))));
        assert!(!h.installed && !h.removed && r.log.is_empty());
    }

    #[test]
    fn failed_enable_removes_the_copied_launcher() {
        // probe fails: file, .new/.old and the empty folder go (Host::remove_launcher)
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        h.probe = Err("no answer".into());
        assert!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).is_err());
        assert!(h.installed && h.removed_paths == vec![LAUNCHER.to_string()]);
        // post-copy trust failure: same cleanup
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        h.trust = Err("writable".into());
        assert!(matches!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)), Err(Fail::Untrusted(_))));
        assert!(h.removed && r.log.is_empty());
        // another redirect still needs the launcher: it stays
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        assert_eq!(enable(&mut r, &mut h, "blink-surrogate-b.exe", PF, BLINK, Some(r"C:\stage\blink-surrogate-b.exe")), Ok(Outcome::Enabled));
        h.probe = Err("no".into());
        assert!(enable(&mut r, &mut h, SUR, PF, BLINK, Some(SUR_IMG)).is_err());
        assert!(!h.removed);
    }

    #[test]
    fn release_builds_take_the_real_target_only() {
        for (t, img) in [(SUR, None), (SUR, Some(SUR_IMG)), (REAL_TARGET, Some(SUR_IMG))] {
            assert!(!helper_args_allowed(t, img, false), "{t} {img:?}");
            assert!(helper_args_allowed(t, img, true));
        }
        assert!(helper_args_allowed(REAL_TARGET, None, false));
    }

    #[test]
    fn elevated_paths_do_not_log_to_the_user_dir() {
        assert!(log_allowed(false));
        assert!(!log_allowed(true));
    }

    #[test]
    fn disable_and_uninstall_also_clean_staging_state_in_debug_builds() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        on(&mut r, &mut h);
        assert_eq!(staging_targets(&r), vec![SUR.to_string()]);
        assert_eq!(uninstall_needed(&r, &h, PF, true), UninstallStep::TurnOff);
        assert_eq!(uninstall_needed(&r, &h, PF, false), UninstallStep::Nothing, "release looks at the real target only");
        // release: leaves staging alone
        assert_eq!(disable_all(&mut r, &mut h, PF, false), Ok(Outcome::AlreadyOff));
        assert!(read_state(&r, SUR).is_some());
        // debug: everything goes, launcher too
        assert_eq!(disable_all(&mut r, &mut h, PF, true), Ok(Outcome::Disabled));
        assert!(r.vals.is_empty() && h.removed, "{:?}", r.vals);
        assert_eq!(uninstall_needed(&r, &h, PF, true), UninstallStep::Nothing);
    }

    fn ctx(args: &[&str], shift: bool, ctrl: bool, guard: bool) -> LaunchCtx {
        LaunchCtx { args: args.iter().map(|s| s.to_string()).collect(), shift, ctrl, guard_set: guard }
    }
    fn st(enabled: bool) -> State {
        State { enabled, launcher: LAUNCHER.into(), blink_path: BLINK.into(), native_path: None, ..Default::default() }
    }

    #[test]
    fn launcher_bypass_rules() {
        let s = st(true);
        assert_eq!(plan(&ctx(&["x"], false, false, false), Some(&s)), Plan::Show);
        assert_eq!(plan(&ctx(&["x"], true, false, false), Some(&s)), Plan::Native(Why::Shift));
        // Ctrl+Shift+Esc: Shift is still down but Ctrl too: not an escape
        assert_eq!(plan(&ctx(&["x"], true, true, false), Some(&s)), Plan::Show);
        assert_eq!(plan(&ctx(&["x"], false, true, false), Some(&s)), Plan::Show);
        // switched off / no state: native, never Blink
        assert_eq!(plan(&ctx(&["x"], false, false, false), Some(&st(false))), Plan::Native(Why::Disabled));
        assert_eq!(plan(&ctx(&["x"], false, false, false), None), Plan::Native(Why::Disabled));
        // the probe's dry run wins over everything but the loop guard
        assert_eq!(plan(&ctx(&["x", SELF_TEST_ARG, "n"], true, false, false), None), Plan::SelfTest);
    }

    #[test]
    fn recursion_guard() {
        // a launcher inside a launcher starts nothing, whatever else is set
        for (shift, ctrl) in [(false, false), (true, false), (true, true)] {
            assert_eq!(plan(&ctx(&["x"], shift, ctrl, true), Some(&st(true))), Plan::Abort);
        }
        assert_eq!(plan(&ctx(&[SELF_TEST_ARG], false, false, true), Some(&st(true))), Plan::Abort);
        // the native start skips IFEO: DEBUG_PROCESS is in its creation flags
        assert_ne!(NATIVE_FLAGS & DEBUG_PROCESS, 0);
        assert_ne!(NATIVE_FLAGS & DEBUG_ONLY_THIS_PROCESS, 0);
        // the native path ignores argv and can never be the launcher
        assert_eq!(native_path(REAL_TARGET, r"C:\Windows", None, LAUNCHER).as_deref(), Some(r"C:\Windows\System32\taskmgr.exe"));
        assert_eq!(native_path(SUR, r"C:\Windows", Some(LAUNCHER), LAUNCHER), None);
        assert_eq!(native_path(SUR, r"C:\Windows", Some(r"C:\stage\other.exe"), LAUNCHER), None);
        assert_eq!(native_path(SUR, r"C:\Windows", None, LAUNCHER), None);
        assert_eq!(native_path(SUR, r"C:\Windows", Some(SUR_IMG), LAUNCHER).as_deref(), Some(SUR_IMG));
    }

    #[test]
    fn argv_is_only_a_key() {
        let a = |s: &str| vec![s.to_string()];
        assert_eq!(target_from_args(&a(r"C:\Windows\System32\taskmgr.exe")), "taskmgr.exe");
        assert_eq!(target_from_args(&a("\"C:\\stage\\Blink-Surrogate-TM.exe\"")), SUR);
        // anything else falls to the real target; it can never name a program to run
        assert_eq!(target_from_args(&a(r"C:\Users\u\evil.exe")), "taskmgr.exe");
        assert_eq!(target_from_args(&[]), "taskmgr.exe");
        assert!(blink_window_ok(BLINK, &BLINK.to_uppercase(), LAUNCHER));
        assert!(!blink_window_ok(r"C:\Users\u\evil\blink.exe", BLINK, LAUNCHER));
        assert!(!blink_window_ok(BLINK, "", ""));
        // a Blink the launcher started itself runs as the launcher image
        assert!(blink_window_ok(LAUNCHER, BLINK, LAUNCHER) && blink_window_ok(&LAUNCHER.to_uppercase(), "", LAUNCHER));
        assert!(!blink_window_ok(LAUNCHER, BLINK, ""));
    }

    // ---- cold start decision --------------------------------------------------------

    struct Machine {
        trust: Result<(), String>,
        seen: Seen,
        start: Result<(), String>,
        window: bool,
        native_ok: bool,
        calls: Vec<&'static str>,
        waited: Option<u32>,
        desktop: bool,
        lock: bool,
    }
    impl Machine {
        fn new() -> Machine {
            Machine { trust: Ok(()), seen: Seen::Absent, start: Ok(()), window: true, native_ok: true, calls: Vec::new(), waited: None, desktop: true, lock: true }
        }
    }
    impl LaunchHost for Machine {
        fn on_interactive_desktop(&self) -> bool {
            self.desktop
        }
        fn lock_held(&self) -> bool {
            self.lock
        }
        fn trusted(&self) -> Result<(), String> {
            self.trust.clone()
        }
        fn show_running(&mut self) -> Seen {
            self.calls.push("show");
            self.seen.clone()
        }
        fn start_blink(&mut self) -> Result<(), String> {
            self.calls.push("start");
            self.start.clone()
        }
        fn wait_window(&mut self, ms: u32) -> bool {
            self.calls.push("wait");
            self.waited = Some(ms);
            self.window
        }
        fn stop_started(&mut self) {
            self.calls.push("stop");
        }
        fn start_native(&mut self) -> bool {
            self.calls.push("native");
            self.native_ok
        }
    }

    #[test]
    fn running_blink_is_shown_nothing_started() {
        let mut m = Machine::new();
        m.seen = Seen::Shown;
        assert_eq!(launch(&Plan::Show, &mut m), Done { via: Via::Shown, ok: true });
        assert_eq!(m.calls, ["show"]);
    }

    #[test]
    fn absent_blink_is_cold_started_and_waited_for() {
        let mut m = Machine::new();
        assert_eq!(launch(&Plan::Show, &mut m), Done { via: Via::ColdStarted, ok: true });
        assert_eq!(m.calls, ["show", "start", "wait"]);
        assert_eq!(m.waited, Some(COLD_WAIT_MS));
        assert!(COLD_WAIT_MS <= 3000);
    }

    #[test]
    fn hung_blink_goes_native_without_a_second_start() {
        let mut m = Machine::new();
        m.seen = Seen::Hung;
        assert_eq!(launch(&Plan::Show, &mut m), Done { via: Via::Native(NativeWhy::Hung), ok: true });
        assert_eq!(m.calls, ["show", "native"]);
    }

    #[test]
    fn shift_and_disabled_never_look_for_blink() {
        for (plan, why) in [(Plan::Native(Why::Shift), NativeWhy::Shift), (Plan::Native(Why::Disabled), NativeWhy::Disabled)] {
            let mut m = Machine::new();
            assert_eq!(launch(&plan, &mut m), Done { via: Via::Native(why), ok: true });
            assert_eq!(m.calls, ["native"]);
        }
    }

    #[test]
    fn untrusted_launcher_starts_and_messages_nothing_but_native() {
        let mut m = Machine::new();
        m.trust = Err(r"C:\Program Files\Blink: BU may change it".into());
        m.seen = Seen::Shown;
        assert_eq!(launch(&Plan::Show, &mut m), Done { via: Via::Native(NativeWhy::Untrusted), ok: true });
        assert_eq!(m.calls, ["native"]);
    }

    #[test]
    fn failed_start_falls_back_to_native() {
        let mut m = Machine::new();
        m.start = Err("the launcher changed since the check".into());
        assert_eq!(launch(&Plan::Show, &mut m), Done { via: Via::Native(NativeWhy::StartFailed), ok: true });
        assert_eq!(m.calls, ["show", "start", "native"], "no wait, no stop: nothing was started");
    }

    #[test]
    fn no_window_in_time_ends_the_started_blink_then_native() {
        let mut m = Machine::new();
        m.window = false;
        assert_eq!(launch(&Plan::Show, &mut m), Done { via: Via::Native(NativeWhy::NoWindow), ok: true });
        assert_eq!(m.calls, ["show", "start", "wait", "stop", "native"], "stopped before native, so no second window appears later");
    }

    #[test]
    fn native_failure_is_the_only_failed_exit() {
        let mut m = Machine::new();
        m.start = Err("x".into());
        m.native_ok = false;
        let d = launch(&Plan::Show, &mut m);
        assert!(!d.ok);
        assert_eq!(exit_code(&d), EXIT_NATIVE_FAILED);
        assert_eq!(exit_code(&Done { via: Via::ColdStarted, ok: true }), EXIT_OK);
        // one attempt per request: no retry loop
        assert_eq!(m.calls.iter().filter(|c| **c == "start").count(), 1);
    }

    #[test]
    fn abort_and_self_test_start_nothing() {
        for p in [Plan::Abort, Plan::SelfTest] {
            let mut m = Machine::new();
            assert!(!launch(&p, &mut m).ok);
            assert!(m.calls.is_empty());
        }
    }

    #[test]
    fn cold_child_is_recognised_by_its_parent_image_only() {
        assert!(cold_child(Some(LAUNCHER), LAUNCHER));
        assert!(cold_child(Some(&LAUNCHER.to_uppercase()), LAUNCHER));
        assert!(!cold_child(None, LAUNCHER));
        assert!(!cold_child(Some(""), LAUNCHER));
        assert!(!cold_child(Some(r"C:\Windows\explorer.exe"), LAUNCHER));
        assert!(!cold_child(Some(BLINK), LAUNCHER));
    }

    fn folders() -> SystemFolders<'static> {
        SystemFolders { local: r"C:\Users\u\AppData\Local", roaming: r"C:\Users\u\AppData\Roaming", profile: r"C:\Users\u", root: r"C:\Windows", program_files: r"C:\Program Files", program_files_x86: r"C:\Program Files (x86)", program_data: r"C:\ProgramData" }
    }

    #[test]
    fn child_env_is_a_whitelist_and_every_folder_comes_from_the_system() {
        let p = |a: &[(&str, &str)]| a.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let parent = p(&[
            ("BLINK_DATA_DIR", r"C:\evil"),
            ("blink_multi", "1"),
            (GUARD_ENV, "1"),
            ("LocalAppData", r"C:\evil"),
            ("PATH", r"C:\evil"),
            ("ProgramFiles", r"C:\evil\pf"),
            ("programw6432", r"C:\evil\pf"),
            ("ProgramFiles(x86)", r"C:\evil\x86"),
            ("ProgramData", r"C:\evil\pd"),
            ("ALLUSERSPROFILE", r"C:\evil"),
            ("COMSPEC", r"C:\evil\cmd.exe"),
            ("PATHEXT", ".EVIL"),
            ("PSModulePath", r"C:\evil"),
            ("_NT_SYMBOL_PATH", r"C:\evil"),
            ("JAVA_TOOL_OPTIONS", "-javaagent:x"),
            ("windir", r"C:\evil"),
            ("SystemDrive", "Z:"),
            ("USERNAME", "u"),
            ("NUMBER_OF_PROCESSORS", "24"),
            ("TEMP", r"C:\evil\t"),
        ]);
        let env = child_env(&parent, &system_vars(&folders()));
        let get = |k: &str| env.iter().filter(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str()).collect::<Vec<_>>();
        assert!(env.iter().all(|(k, _)| !k.to_ascii_uppercase().starts_with("BLINK_")), "no BLINK_* survives");
        assert_eq!(get("LOCALAPPDATA"), [r"C:\Users\u\AppData\Local"], "the caller's spelling is replaced, not duplicated");
        assert_eq!(get("ProgramFiles"), [r"C:\Program Files"]);
        assert_eq!(get("ProgramW6432"), [r"C:\Program Files"]);
        assert_eq!(get("ProgramFiles(x86)"), [r"C:\Program Files (x86)"]);
        assert_eq!(get("ProgramData"), [r"C:\ProgramData"]);
        assert_eq!(get("ALLUSERSPROFILE"), [r"C:\ProgramData"]);
        assert_eq!(get("COMSPEC"), [r"C:\Windows\System32\cmd.exe"]);
        assert_eq!(get("PATHEXT").len(), 1);
        assert!(!get("PATHEXT")[0].contains("EVIL"));
        assert_eq!(get("PATH"), [r"C:\Windows\System32;C:\Windows;C:\Windows\System32\Wbem"]);
        assert_eq!(get("windir"), [r"C:\Windows"]);
        assert_eq!(get("SystemDrive"), ["C:"]);
        assert_eq!(get("TEMP"), [r"C:\Users\u\AppData\Local\Temp"]);
        assert_eq!(get("USERNAME"), ["u"], "harmless labels pass through");
        assert_eq!(get("NUMBER_OF_PROCESSORS"), ["24"]);
        for gone in ["PSModulePath", "_NT_SYMBOL_PATH", "JAVA_TOOL_OPTIONS"] {
            assert!(get(gone).is_empty(), "{gone} is dropped");
        }
        // every surviving name is a system folder variable or on the pass-through list
        let sys = system_vars(&folders());
        assert!(env.iter().all(|(k, _)| sys.iter().any(|(s, _)| s.eq_ignore_ascii_case(k)) || PASS_THROUGH.iter().any(|p| p.eq_ignore_ascii_case(k))));
        assert!(env.iter().all(|(_, v)| !v.contains("evil") && !v.contains("javaagent")), "nothing of the caller's paths survives");
    }

    #[test]
    fn only_the_default_desktop_is_interactive() {
        assert!(interactive_desktop("Default"));
        assert!(interactive_desktop("default"));
        for n in ["Winlogon", "Screen-saver", "Disconnect", "", "Default2"] {
            assert!(!interactive_desktop(n), "{n}");
        }
    }

    #[test]
    fn lock_try_tells_held_from_unusable() {
        assert_eq!(lock_try(0), LockTry::Got);
        assert_eq!(lock_try(32), LockTry::Busy, "sharing violation = another start holds it");
        assert_eq!(lock_try(33), LockTry::Busy);
        for e in [2u32, 3, 5, 123] {
            assert_eq!(lock_try(e), LockTry::Unusable, "{e}: not found / denied is not a wait");
        }
    }

    #[test]
    fn lock_file_policy_rejects_any_non_admin_entry() {
        let ace = |sid: &str, mask: u32, allow: bool| Ace { sid: sid.into(), mask, allow, inherit_only: false };
        let admin_only = [ace(SID_SYSTEM, 0x1F01FF, true), ace(SID_ADMINS, 0x1F01FF, true)];
        assert_eq!(lock_file_policy(SID_ADMINS, Some(&admin_only)), LockFile::Use);
        assert_eq!(lock_file_policy(SID_SYSTEM, Some(&admin_only)), LockFile::Use);
        // the test machine finding: an inherited Users read/execute entry is enough to open and hold it
        let mut inherited = admin_only.to_vec();
        inherited.push(ace("S-1-5-32-545", 0x1200A9, true));
        assert_eq!(lock_file_policy(SID_ADMINS, Some(&inherited)), LockFile::Recreate);
        let everyone = [ace(SID_ADMINS, 0x1F01FF, true), ace("S-1-1-0", 0x120089, true)];
        assert_eq!(lock_file_policy(SID_ADMINS, Some(&everyone)), LockFile::Recreate);
        // a deny entry for Users is harmless; a user as owner or no access list is not ours
        let deny = [ace(SID_ADMINS, 0x1F01FF, true), ace("S-1-5-32-545", 1, false)];
        assert_eq!(lock_file_policy(SID_ADMINS, Some(&deny)), LockFile::Use);
        assert_eq!(lock_file_policy("S-1-5-21-1-2-3-1001", Some(&admin_only)), LockFile::Recreate);
        assert_eq!(lock_file_policy(SID_ADMINS, None), LockFile::Recreate);
        assert_eq!(lock_file_policy(SID_ADMINS, Some(&[])), LockFile::Recreate, "an empty DACL denies everybody, admins included: open fails, so replace it");
        // the SDDL we create it with names exactly SYSTEM and Administrators, protected
        assert!(LOCK_SDDL.starts_with("D:P"));
        assert_eq!(LOCK_SDDL.matches("(A;").count(), 2);
        assert!(LOCK_SDDL.contains(";;;SY)") && LOCK_SDDL.contains(";;;BA)"));
    }

    #[test]
    fn secure_desktop_goes_straight_to_native() {
        let mut m = Machine::new();
        m.desktop = false;
        assert_eq!(launch(&Plan::Show, &mut m), Done { via: Via::Native(NativeWhy::SecureDesktop), ok: true });
        assert_eq!(m.calls, ["native"], "no window search, no start, no wait");
    }

    #[test]
    fn lock_timeout_never_starts_blink() {
        let mut m = Machine::new();
        m.lock = false;
        assert_eq!(launch(&Plan::Show, &mut m), Done { via: Via::Native(NativeWhy::Busy), ok: true });
        assert_eq!(m.calls, ["show", "show", "native"], "re-checked once, never started");
        let mut m = Machine::new();
        m.lock = false;
        m.seen = Seen::Absent;
        assert!(!m.calls.contains(&"start"));
    }

    #[test]
    fn running_launcher_is_reported_not_removed() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        on(&mut r, &mut h);
        h.stuck = true;
        assert_eq!(disable(&mut r, &mut h, SUR, PF), Ok(Outcome::Disabled));
        assert!(h.removed);
        assert_eq!(status(&r, &h, SUR, PF), Status::PendingRemoval, "off, but says the launcher is still there");
        assert_eq!(uninstall_step(&Status::PendingRemoval), UninstallStep::Nothing);
        h.stuck = false;
        assert_eq!(status(&r, &h, SUR, PF), Status::Off);
    }

    #[test]
    fn refused_data_dir_cannot_be_opened() {
        // the stand-in for a data dir behind a link: nothing can be created under it
        let p = std::path::Path::new(crate::osutil::REFUSED_DATA_DIR).join("x.ini");
        assert!(std::fs::OpenOptions::new().create(true).append(true).open(p).is_err());
    }

    fn ace(sid: &str, mask: u32) -> Ace {
        Ace { sid: sid.into(), mask, allow: true, inherit_only: false }
    }
    const USERS: &str = "S-1-5-32-545";
    const RX: u32 = 0x1200a9;

    #[test]
    fn acl_policy() {
        let good = [ace(SID_SYSTEM, 0x1f01ff), ace(SID_ADMINS, 0x1f01ff), ace(USERS, RX)];
        for l in [Level::File, Level::Dir, Level::Ancestor] {
            assert_eq!(acl_problem(l, SID_TRUSTED_INSTALLER, Some(&good)), None, "{l:?}");
            assert_eq!(acl_problem(l, SID_ADMINS, Some(&good)), None);
        }
        // user-owned or world-writable is never fine
        assert!(acl_problem(Level::File, "S-1-5-21-1-2-3-1001", Some(&good)).is_some());
        assert!(acl_problem(Level::File, SID_ADMINS, None).is_some());
        assert!(acl_problem(Level::File, SID_ADMINS, Some(&[ace(USERS, WRITE_DATA)])).is_some());
        assert!(acl_problem(Level::File, SID_ADMINS, Some(&[ace("S-1-1-0", GENERIC_ALL)])).is_some());
        assert!(acl_problem(Level::File, SID_ADMINS, Some(&[ace(USERS, DELETE)])).is_some());
        assert!(acl_problem(Level::Dir, SID_ADMINS, Some(&[ace(USERS, WRITE_DATA)])).is_some(), "planting a DLL next to the launcher");
        assert!(acl_problem(Level::Dir, SID_ADMINS, Some(&[ace(USERS, DELETE_CHILD)])).is_some());
        assert!(acl_problem(Level::Ancestor, SID_ADMINS, Some(&[ace(USERS, DELETE_CHILD)])).is_some());
        assert!(acl_problem(Level::Ancestor, SID_ADMINS, Some(&[ace(USERS, WRITE_DAC)])).is_some());
        // C:\ lets users create subfolders: that does not let them replace Program Files
        assert_eq!(acl_problem(Level::Ancestor, SID_TRUSTED_INSTALLER, Some(&[ace(USERS, APPEND_DATA | WRITE_DATA)])), None);
        // inherit-only entries and denies do not widen anything
        let mut io = ace(USERS, GENERIC_ALL);
        io.inherit_only = true;
        assert_eq!(acl_problem(Level::File, SID_ADMINS, Some(&[io])), None);
        let mut deny = ace(USERS, GENERIC_ALL);
        deny.allow = false;
        assert_eq!(acl_problem(Level::File, SID_ADMINS, Some(&[deny])), None);
    }

    #[test]
    fn acl_levels() {
        assert_eq!(level_for(LAUNCHER, LAUNCHER, PF), Level::File);
        assert_eq!(level_for(r"C:\Program Files\Blink", LAUNCHER, PF), Level::Dir);
        assert_eq!(level_for(PF, LAUNCHER, PF), Level::Ancestor);
        assert_eq!(level_for(r"C:\", LAUNCHER, PF), Level::Ancestor);
    }

    #[test]
    fn exit_codes_map_to_text() {
        assert_eq!(Fail::Foreign("x".into()).exit_code(), EXIT_FOREIGN);
        assert_eq!(Fail::Untrusted("x".into()).exit_code(), EXIT_UNTRUSTED);
        assert!(exit_message(EXIT_FOREIGN).contains("another tool"));
        assert!(exit_message(99).contains("99"));
    }

    #[test]
    fn the_helper_gate_lets_one_run_in_and_reopens_after_it() {
        let g = Gate::new();
        assert!(!g.busy());
        assert!(g.enter());
        assert!(g.busy());
        assert!(!g.enter(), "a second click while the helper works does nothing");
        g.leave();
        assert!(!g.busy());
        assert!(g.enter());
    }

    // ---- launcher refresh -------------------------------------------------------------

    /// the redirect is on, the launcher is an older build (`aaaa`), the helper's image is `bbbb`
    fn on_stale() -> (Mem, Fake) {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        on(&mut r, &mut h);
        h.present = Some(true);
        h.calls.clear();
        r.log.clear();
        h.build = 2; // enabled by build 1, this Hayate is build 2
        (r, h)
    }

    fn run_refresh(r: &mut Mem, h: &mut Fake) -> Result<Refreshed, Fail> {
        refresh(r, h, SUR, PF, BLINK)
    }

    #[test]
    fn stale_vs_equal_vs_unknown() {
        assert_eq!(freshness(Some("aa"), Some("bb"), Some(1), Some(2)), Fresh::Stale);
        assert_eq!(freshness(Some("AbC"), Some("aBc"), Some(1), Some(2)), Fresh::Current);
        assert_eq!(freshness(None, Some("bb"), None, Some(2)), Fresh::Unknown);
        assert_eq!(freshness(Some("aa"), None, None, Some(2)), Fresh::Unknown);
        assert_eq!(freshness(Some(""), Some("bb"), None, Some(2)), Fresh::Unknown);
    }

    #[test]
    fn an_older_or_equal_build_never_downgrades_the_launcher() {
        assert_eq!(freshness(Some("aa"), Some("bb"), Some(5), Some(5)), Fresh::NotNewer, "same build number, different file: not newer");
        assert_eq!(freshness(Some("aa"), Some("bb"), Some(9), Some(5)), Fresh::NotNewer, "a rolled-back Hayate");
        assert_eq!(freshness(Some("aa"), Some("bb"), Some(4), Some(5)), Fresh::Stale);
        assert_eq!(freshness(Some("aa"), Some("bb"), None, Some(5)), Fresh::Stale, "a launcher without a record counts as older");
        assert_eq!(freshness(Some("aa"), Some("bb"), Some(5), None), Fresh::Stale, "the cold-started launcher has no ordinal to compare");
        assert!(!refresh_offer(&Status::On, &Fresh::NotNewer, true, false, false));
    }

    #[test]
    fn a_blink_that_is_not_newer_refuses_to_swap() {
        let (mut r, mut h) = on_stale();
        h.build = 1;
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Refused(_)) && e.to_string().contains("not newer"), "{e}");
        assert_eq!(h.calls, ["lock", "unlock"]);
        h.build = 0;
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Refused(_))));
    }

    #[test]
    fn enable_and_refresh_record_the_build_ordinal() {
        let (mut r, mut h) = on_stale();
        assert_eq!(read_state(&r, SUR).unwrap().build, Some(1));
        assert_eq!(run_refresh(&mut r, &mut h), Ok(Refreshed::Done));
        assert_eq!(read_state(&r, SUR).unwrap().build, Some(2));
    }

    #[test]
    fn the_file_at_the_launcher_path_must_be_the_staged_image() {
        let (mut r, mut h) = on_stale();
        h.swap_lands = Some("cccc".into());
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Kept(ref m) if m.contains("not the expected image")), "{e}");
        assert_eq!(h.calls, ["lock", "stage bbbb", "swap", "probe", "restore", "probe", "unlock"]);
        assert_eq!(read_state(&r, SUR).unwrap().build, Some(1), "no new ordinal for a launcher that was rolled back");
    }

    #[test]
    fn repair_offer_needs_our_value_a_missing_launcher_and_the_old_copy() {
        let broken = Status::Stale(format!("{LAUNCHER} is missing"));
        assert!(repair_offer(&broken, true, false, true, true, false, false));
        assert!(!repair_offer(&broken, true, true, true, true, false, false), "the file is there");
        assert!(!repair_offer(&broken, true, false, false, true, false, false), "nothing to put back: switch off and on");
        assert!(!repair_offer(&Status::On, true, false, true, true, false, false));
        assert!(!repair_offer(&broken, true, false, true, false, false, false), "portable copy");
        assert!(!repair_offer(&broken, true, false, true, true, true, false), "the cold-started Hayate is the launcher");
        assert!(!repair_offer(&broken, true, false, true, true, false, true), "a helper run is under way");
        assert!(!repair_offer(&broken, false, false, true, true, false, false), "the redirect is not on: nothing to repair");
    }

    #[test]
    fn the_offer_needs_on_stale_installed_and_not_the_launcher_image() {
        let (on, stale) = (Status::On, Fresh::Stale);
        assert!(refresh_offer(&on, &stale, true, false, false));
        assert!(!refresh_offer(&on, &Fresh::Current, true, false, false));
        assert!(!refresh_offer(&on, &Fresh::Unknown, true, false, false));
        assert!(!refresh_offer(&Status::Off, &stale, true, false, false));
        assert!(!refresh_offer(&Status::Stale("x".into()), &stale, true, false, false), "a broken redirect is repaired by switching off and on");
        assert!(!refresh_offer(&Status::Foreign("x".into()), &stale, true, false, false));
        assert!(!refresh_offer(&on, &stale, false, false, false), "portable copy");
        assert!(!refresh_offer(&on, &stale, true, true, false), "the cold-started Hayate is the launcher: it never refreshes itself");
        assert!(!refresh_offer(&on, &stale, true, false, true), "a helper run is under way");
    }

    #[test]
    fn auto_refresh_only_when_elevated_offered_and_not_tried_for_this_pair() {
        let pair = pair_key("AAAA", "bbbb");
        assert_eq!(pair, "aaaa:bbbb");
        assert!(auto_refresh_due(true, true, None, &pair, false));
        assert!(!auto_refresh_due(true, false, None, &pair, false), "unelevated: the Settings button only (it asks UAC)");
        assert!(!auto_refresh_due(false, true, None, &pair, false), "nothing offered");
        assert!(!auto_refresh_due(true, true, Some(&pair), &pair, false), "tried once for this pair: a failing refresh never loops");
        assert!(auto_refresh_due(true, true, Some("x:y"), &pair, false), "a new build is a new pair");
        assert!(auto_refresh_due(true, true, Some(&pair), &pair, true), "an elevated self-update just finished");
        assert!(!auto_refresh_due(false, true, Some(&pair), &pair, true));
    }

    #[test]
    fn refresh_swaps_in_order_and_writes_no_registry_value() {
        let (mut r, mut h) = on_stale();
        assert_eq!(run_refresh(&mut r, &mut h), Ok(Refreshed::Done));
        assert_eq!(h.calls, ["lock", "stage bbbb", "swap", "probe", "unlock"]);
        assert_eq!(r.log.len(), 1, "value and state already point at the launcher path: only the build record moves: {:?}", r.log);
        assert!(r.log[0].ends_with("|BuildOrdinal=2"), "{:?}", r.log);
        assert!(!h.removed, "the launcher is replaced, never removed");
        assert!(matches!(status(&r, &h, SUR, PF), Status::On));
    }

    #[test]
    fn refresh_of_a_current_launcher_touches_nothing() {
        let (mut r, mut h) = on_stale();
        h.file_sha = Ok("BBBB".into());
        assert_eq!(run_refresh(&mut r, &mut h), Ok(Refreshed::Current));
        assert_eq!(h.calls, ["lock", "unlock"]);
    }

    #[test]
    fn refresh_follows_a_moved_blink_path_best_effort() {
        let (mut r, mut h) = on_stale();
        let other = r"C:\Program Files\Blink\blink.exe";
        assert_eq!(refresh(&mut r, &mut h, SUR, PF, other), Ok(Refreshed::Done));
        assert_eq!(read_state(&r, SUR).unwrap().blink_path, other);
    }

    #[test]
    fn stage_failure_changes_nothing() {
        let (mut r, mut h) = on_stale();
        h.stage = Err("disk full".into());
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Io(_)) && e.exit_code() == EXIT_IO && e.to_string().contains("Nothing was changed"));
        assert_eq!(h.calls, ["lock", "stage bbbb", "unlock"]);
        assert!(r.log.is_empty());
    }

    #[test]
    fn swap_failure_with_the_old_file_in_place_keeps_it() {
        let (mut r, mut h) = on_stale();
        h.swap = Err("the old copy is in use".into());
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Io(_)));
        assert_eq!(h.calls, ["lock", "stage bbbb", "swap", "unlock"], "no restore, no probe: the old launcher never moved");
        assert!(r.log.is_empty());
    }

    #[test]
    fn swap_failure_that_lost_the_file_restores_the_old_one_and_probes_again() {
        let (mut r, mut h) = on_stale();
        h.swap = Err("rename failed".into());
        h.lose_on_swap = true;
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Kept(_)) && e.exit_code() == EXIT_KEPT);
        assert_eq!(h.calls, ["lock", "stage bbbb", "swap", "restore", "probe", "unlock"]);
        assert!(r.get(&ifeo_key(SUR), "Debugger").unwrap().is_some(), "the value stays, pointing at the restored launcher");
        assert!(!h.removed);
    }

    #[test]
    fn failed_probe_restores_the_old_launcher_and_probes_it_again() {
        let (mut r, mut h) = on_stale();
        h.probes = vec![Err("the new launcher did not answer".into()), Ok(())];
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Kept(_)) && e.to_string().contains("put back and works"));
        assert_eq!(h.calls, ["lock", "stage bbbb", "swap", "probe", "restore", "probe", "unlock"]);
        assert!(r.log.is_empty(), "registry untouched");
        assert!(!h.removed);
        assert!(matches!(status(&r, &h, SUR, PF), Status::On));
    }

    #[test]
    fn restore_failure_removes_the_value_and_says_so() {
        let (mut r, mut h) = on_stale();
        h.probes = vec![Err("no answer".into())];
        h.restore = Err("the old copy is gone".into());
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Removed(_)) && e.exit_code() == EXIT_REMOVED && e.to_string().contains("opens again"));
        assert_eq!(r.get(&ifeo_key(SUR), "Debugger").unwrap(), None, "Task Manager must never point at a broken launcher");
        assert!(read_state(&r, SUR).is_none());
        assert!(h.removed, "the leftover file goes too");
        h.present = Some(false);
        assert_eq!(status(&r, &h, SUR, PF), Status::Off);
    }

    #[test]
    fn the_restored_launcher_failing_its_probe_removes_the_value_too() {
        let (mut r, mut h) = on_stale();
        h.probes = vec![Err("new: no answer".into()), Err("old: no answer".into())];
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Removed(ref m) if m.contains("restored launcher failed too")), "{e}");
        assert_eq!(r.get(&ifeo_key(SUR), "Debugger").unwrap(), None);
        assert!(read_state(&r, SUR).is_none());
    }

    #[test]
    fn the_lock_is_released_before_the_redirect_is_removed() {
        let (mut r, mut h) = on_stale();
        h.probes = vec![Err("x".into())];
        h.restore = Err("y".into());
        let _ = run_refresh(&mut r, &mut h);
        assert_eq!(h.calls.last().map(String::as_str), Some("unlock"));
        assert_eq!(h.calls.iter().filter(|c| *c == "lock").count(), 1);
        assert!(h.removed);
    }

    #[test]
    fn a_cold_start_holding_the_lock_blocks_the_refresh() {
        let (mut r, mut h) = on_stale();
        h.lock = Err("another start holds the lock".into());
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Busy(_)) && e.exit_code() == EXIT_BUSY);
        assert_eq!(h.calls, ["lock"], "no stage, no swap, no unlock of a lock never taken");
        assert!(r.log.is_empty());
    }

    #[test]
    fn an_untrusted_state_or_launcher_refuses_and_changes_nothing() {
        let (mut r, mut h) = on_stale();
        h.state = Err("the state key can be written by Users".into());
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Untrusted(_)) && e.exit_code() == EXIT_UNTRUSTED);
        assert!(h.calls.is_empty() && r.log.is_empty());
        h.state = Ok(());
        h.trust = Err("S-1-5-32-545 may change it".into());
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Untrusted(_))));
        assert!(h.calls == ["lock", "unlock"] && r.log.is_empty(), "untrusted: nothing is copied or moved");
        h.trust = Ok(());
        h.pre = Err("folder is a junction".into());
        h.calls.clear();
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Untrusted(_))));
        assert_eq!(h.calls, ["lock", "unlock"], "the folder chain is checked before a byte is copied");
    }

    #[test]
    fn only_a_redirect_that_is_on_and_ours_is_refreshed() {
        let (mut r, mut h) = (Mem::default(), Fake::ok());
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Refused(_))), "off");
        on(&mut r, &mut h);
        h.calls.clear();
        r.set(&state_key(SUR), "Enabled", "0").unwrap();
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Refused(_))), "enable did not finish");
        r.set(&state_key(SUR), "Enabled", "1").unwrap();
        r.set(&ifeo_key(SUR), "Debugger", r"C:\tools\other.exe").unwrap();
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Foreign(_))), "someone else's value is never touched");
        r.delete_value(&ifeo_key(SUR), "Debugger").unwrap();
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Refused(_))), "Windows reset it: never silently re-added");
        assert!(h.calls.is_empty());
        assert!(matches!(refresh(&mut r, &mut h, "notepad.exe", PF, BLINK), Err(Fail::BadTarget)));
    }

    #[test]
    fn refresh_exit_codes_have_text() {
        for c in [EXIT_KEPT, EXIT_REMOVED, EXIT_BUSY] {
            assert!(!exit_message(c).contains("failed (code"), "{c}");
        }
        assert_ne!(EXIT_KEPT, EXIT_REMOVED);
        assert_ne!(EXIT_BUSY, EXIT_KEPT);
    }

    // ---- repair ---------------------------------------------------------------------------

    #[test]
    fn a_missing_launcher_with_the_old_copy_is_put_back_and_probed() {
        let (mut r, mut h) = on_stale();
        h.present = Some(false);
        assert_eq!(run_refresh(&mut r, &mut h), Ok(Refreshed::Repaired));
        assert_eq!(h.calls, ["lock", "restore", "probe", "unlock"]);
        assert!(r.log.is_empty(), "registry untouched: {:?}", r.log);
        assert!(r.get(&ifeo_key(SUR), "Debugger").unwrap().is_some());
    }

    #[test]
    fn a_missing_launcher_without_an_old_copy_removes_the_value() {
        let (mut r, mut h) = on_stale();
        h.present = Some(false);
        h.old = false;
        let e = run_refresh(&mut r, &mut h).unwrap_err();
        assert!(matches!(e, Fail::Removed(ref m) if m.contains("no previous copy")), "{e}");
        assert_eq!(r.get(&ifeo_key(SUR), "Debugger").unwrap(), None, "never a value pointing at a missing file");
        assert!(!h.calls.contains(&"restore".to_string()));
    }

    #[test]
    fn a_repair_that_fails_its_restore_or_probe_removes_the_value() {
        let (mut r, mut h) = on_stale();
        h.present = Some(false);
        h.restore = Err("access denied".into());
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Removed(_))));
        assert_eq!(r.get(&ifeo_key(SUR), "Debugger").unwrap(), None);
        let (mut r, mut h) = on_stale();
        h.present = Some(false);
        h.probes = vec![Err("no answer".into())];
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Removed(ref m)) if m.contains("repaired launcher")));
        assert_eq!(r.get(&ifeo_key(SUR), "Debugger").unwrap(), None);
    }

    #[test]
    fn a_repair_checks_the_folder_chain_first() {
        let (mut r, mut h) = on_stale();
        h.present = Some(false);
        h.pre = Err("folder is a junction".into());
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Untrusted(_))));
        assert_eq!(h.calls, ["lock", "unlock"]);
    }

    #[test]
    fn the_missing_launcher_is_decided_under_the_lock() {
        // present before the lock, gone once it is held (a concurrent step moved it): repaired, not swapped
        struct Late(Fake, bool);
        impl Host for Late {
            fn precheck(&self, p: &str) -> Result<(), String> { self.0.precheck(p) }
            fn launcher_trusted(&self, p: &str) -> Result<(), String> { self.0.launcher_trusted(p) }
            fn state_trusted(&self) -> Result<(), String> { self.0.state_trusted() }
            fn install_launcher(&mut self, p: &str) -> Result<(), String> { self.0.install_launcher(p) }
            fn remove_launcher(&mut self, p: &str) -> bool { self.0.remove_launcher(p) }
            fn launcher_exists(&self, _: &str) -> bool { !self.1 }
            fn old_exists(&self, p: &str) -> bool { self.0.old_exists(p) }
            fn source_ordinal(&self) -> u64 { self.0.source_ordinal() }
            fn probe(&mut self, t: &str, n: Option<&str>) -> Result<(), String> { self.0.probe(t, n) }
            fn lock_cold_start(&mut self) -> Result<(), String> { self.1 = true; self.0.lock_cold_start() }
            fn unlock_cold_start(&mut self) { self.0.unlock_cold_start() }
            fn source_sha(&self) -> Result<String, String> { self.0.source_sha() }
            fn launcher_sha(&self, p: &str) -> Result<String, String> { self.0.launcher_sha(p) }
            fn stage_launcher(&mut self, p: &str, s: &str) -> Result<(), String> { self.0.stage_launcher(p, s) }
            fn swap_launcher(&mut self, p: &str) -> Result<(), String> { self.0.swap_launcher(p) }
            fn restore_launcher(&mut self, p: &str) -> Result<(), String> { self.0.restore_launcher(p) }
        }
        let (mut r, h) = on_stale();
        let mut l = Late(h, false);
        assert_eq!(refresh(&mut r, &mut l, SUR, PF, BLINK), Ok(Refreshed::Repaired));
        assert_eq!(l.0.calls, ["lock", "restore", "probe", "unlock"], "no stage, no swap: the file was missing under the lock");
    }

    #[test]
    fn a_disable_that_ran_first_is_never_written_to_again() {
        // the redirect is gone by the time the refresh would record its build: no key is recreated
        let (mut r, mut h) = on_stale();
        r.set(&state_key(SUR), "Enabled", "0").unwrap();
        r.log.clear();
        assert_eq!(read_state(&r, SUR).map(|s| s.enabled), Some(false));
        // refresh refuses a redirect that is not on, and writes nothing
        assert!(matches!(run_refresh(&mut r, &mut h), Err(Fail::Refused(_))));
        assert!(r.log.is_empty());
    }

    #[test]
    fn helper_text_is_one_clean_line() {
        assert_eq!(clean_text("a\r\nb\x07  c"), "a b c");
        assert_eq!(clean_text(&"x".repeat(900)).chars().count(), 400);
        assert_eq!(clean_text("\u{1b}[31mred"), "[31mred");
    }

    // ---- the swap protocol, with a crash at every point ------------------------------------


    const P: &str = r"C:\Program Files\Blink\blink-tm.exe";
    const N: &str = r"C:\Program Files\Blink\blink-tm.new.1.exe";
    const R: &str = r"C:\Program Files\Blink\blink-tm.new.2.exe";
    const O: &str = r"C:\Program Files\Blink\blink-tm.old.exe";

    #[derive(Clone, Default)]
    struct Disk {
        files: BTreeMap<String, String>,
        /// paths whose file is a running image: cannot be replaced or deleted, can be renamed
        running: Vec<String>,
        ops: usize,
        crash_at: Option<usize>,
        dead: bool,
        replace_fails: usize,
        protected: Vec<String>,
        reboot: Vec<String>,
        pauses: usize,
    }
    impl Disk {
        fn with(files: &[(&str, &str)]) -> Disk {
            Disk { files: files.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(), ..Default::default() }
        }
        /// every mutating operation passes here: the process may die before it
        fn tick(&mut self) -> Result<(), String> {
            if self.dead {
                return Err("dead".into());
            }
            if self.crash_at == Some(self.ops) {
                self.dead = true;
                return Err("crash".into());
            }
            self.ops += 1;
            Ok(())
        }
        fn at(&self, p: &str) -> Option<&str> {
            self.files.get(p).map(String::as_str)
        }
        /// the machine after a crash: all running images are gone, nothing is dead any more
        fn rebooted(&self) -> Disk {
            Disk { files: self.files.clone(), ..Default::default() }
        }
    }
    impl Fs for Disk {
        fn exists(&self, p: &str) -> bool {
            self.files.contains_key(p)
        }
        fn copy(&mut self, from: &str, to: &str) -> Result<(), String> {
            self.tick()?;
            let c = self.files.get(from).cloned().ok_or("no source")?;
            if self.files.contains_key(to) {
                return Err("exists".into());
            }
            self.files.insert(to.into(), c);
            Ok(())
        }
        fn replace(&mut self, from: &str, to: &str) -> Result<(), String> {
            self.tick()?;
            if self.replace_fails > 0 {
                self.replace_fails -= 1;
                return Err("sharing violation".into());
            }
            if self.running.iter().any(|r| r == to) {
                return Err("access denied: in use".into());
            }
            let c = self.files.remove(from).ok_or("no source")?;
            self.files.insert(to.into(), c);
            Ok(())
        }
        fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
            self.tick()?;
            if self.files.contains_key(to) {
                return Err("exists".into());
            }
            let c = self.files.remove(from).ok_or("no source")?;
            self.files.insert(to.into(), c);
            for r in self.running.iter_mut() {
                if r == from {
                    *r = to.to_string();
                }
            }
            Ok(())
        }
        fn remove(&mut self, p: &str) -> Result<(), String> {
            self.tick()?;
            if self.running.iter().any(|r| r == p) {
                return Err("access denied: in use".into());
            }
            self.files.remove(p);
            Ok(())
        }
        fn protect(&mut self, p: &str) {
            self.protected.push(p.into());
        }
        fn delete_at_reboot(&mut self, p: &str) {
            self.reboot.push(p.into());
        }
        fn pause(&mut self) {
            self.pauses += 1;
        }
    }

    #[test]
    fn the_swap_never_loses_the_launcher_at_any_crash_point() {
        // finish without a crash first: how many operations it takes
        let mut d = Disk::with(&[(P, "A"), (N, "B")]);
        swap_files(&mut d, P, N, O).unwrap();
        let total = d.ops;
        assert_eq!((d.at(P), d.at(O), d.at(N)), (Some("B"), Some("A"), None));
        assert!(d.protected.contains(&O.to_string()), "the rollback copy is admin-only");
        for k in 0..=total + 1 {
            let mut d = Disk::with(&[(P, "A"), (N, "B")]);
            d.crash_at = Some(k);
            let _ = swap_files(&mut d, P, N, O);
            let launcher = d.at(P).unwrap_or_else(|| panic!("crash point {k}: the launcher file is missing"));
            assert!(launcher == "A" || launcher == "B", "crash point {k}: {launcher}");
            if launcher == "B" {
                assert_eq!(d.at(O), Some("A"), "crash point {k}: the new launcher is in, the rollback copy must be there");
            }
        }
    }

    #[test]
    fn a_running_launcher_leaves_only_a_repairable_gap() {
        let mut d = Disk::with(&[(P, "A"), (N, "B")]);
        d.running = vec![P.into()];
        swap_files(&mut d, P, N, O).unwrap();
        let total = d.ops;
        assert_eq!((d.at(P), d.at(O)), (Some("B"), Some("A")));
        assert!(d.reboot.contains(&O.to_string()), "the running image, renamed, goes at reboot");
        assert_eq!(d.running, vec![O.to_string()], "the running image follows its rename");
        for k in 0..=total + 1 {
            let mut d = Disk::with(&[(P, "A"), (N, "B")]);
            d.running = vec![P.into()];
            d.crash_at = Some(k);
            let _ = swap_files(&mut d, P, N, O);
            if d.at(P).is_none() {
                assert_eq!(d.at(O), Some("A"), "crash point {k}: no launcher, so the old image must be there to repair from");
                // the repair after the crash (the machine, or at least the helper, is alive again)
                let mut after = d.rebooted();
                restore_files(&mut after, P, R, O).unwrap_or_else(|e| panic!("crash point {k}: {e}"));
                assert_eq!(after.at(P), Some("A"), "crash point {k}");
            } else {
                assert!(matches!(d.at(P), Some("A") | Some("B")), "crash point {k}");
            }
        }
    }

    #[test]
    fn the_restore_never_removes_the_launcher_first() {
        let mut d = Disk::with(&[(P, "B"), (O, "A")]);
        restore_files(&mut d, P, R, O).unwrap();
        let total = d.ops;
        assert_eq!((d.at(P), d.at(O), d.at(R)), (Some("A"), Some("A"), None), "a copy goes in, the rollback copy stays");
        for k in 0..=total + 1 {
            let mut d = Disk::with(&[(P, "B"), (O, "A")]);
            d.crash_at = Some(k);
            let _ = restore_files(&mut d, P, R, O);
            assert!(matches!(d.at(P), Some("A") | Some("B")), "crash point {k}: the launcher file is missing");
            assert_eq!(d.at(O), Some("A"), "crash point {k}");
        }
    }

    #[test]
    fn the_restore_retries_the_replace_a_few_times() {
        let mut d = Disk::with(&[(P, "B"), (O, "A")]);
        d.replace_fails = 2;
        restore_files(&mut d, P, R, O).unwrap();
        assert_eq!((d.at(P), d.pauses), (Some("A"), 2));
        let mut d = Disk::with(&[(P, "B"), (O, "A")]);
        d.replace_fails = 99;
        let e = restore_files(&mut d, P, R, O).unwrap_err();
        assert!(e.contains("Cannot put the previous launcher back"), "{e}");
        assert_eq!((d.at(P), d.at(R)), (Some("B"), None), "nothing half done is left behind");
        assert_eq!(d.pauses, 4);
        let mut d = Disk::with(&[(P, "B")]);
        assert!(restore_files(&mut d, P, R, O).unwrap_err().contains("no previous launcher"));
    }

    #[test]
    fn a_running_rollback_copy_stops_the_swap_before_anything_moves() {
        let mut d = Disk::with(&[(P, "A"), (N, "B"), (O, "X")]);
        d.running = vec![O.into()];
        let e = swap_files(&mut d, P, N, O).unwrap_err();
        assert!(e.contains("still running"), "{e}");
        assert_eq!((d.at(P), d.at(N), d.at(O)), (Some("A"), None, Some("X")));
    }

    #[test]
    fn a_missing_launcher_is_simply_placed() {
        let mut d = Disk::with(&[(N, "B")]);
        swap_files(&mut d, P, N, O).unwrap();
        assert_eq!((d.at(P), d.at(N)), (Some("B"), None));
        let mut d = Disk::with(&[(P, "A")]);
        assert!(swap_files(&mut d, P, N, O).is_err(), "no staged copy: nothing happens");
        assert_eq!(d.at(P), Some("A"));
    }

    #[test]
    fn io_and_registry_failures_have_their_own_exit_codes_and_texts() {
        assert_eq!(Fail::Io("x".into()).exit_code(), EXIT_IO);
        assert_eq!(Fail::Registry("x".into()).exit_code(), EXIT_REGISTRY);
        let all = [EXIT_OK, EXIT_NOT_ELEVATED, EXIT_REFUSED, EXIT_FOREIGN, EXIT_UNTRUSTED, EXIT_PROBE, EXIT_KEPT, EXIT_REMOVED, EXIT_BUSY, EXIT_IO, EXIT_REGISTRY, EXIT_FAILED];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        for c in [EXIT_IO, EXIT_REGISTRY, 99] {
            assert!(!exit_message(c).contains("blink.log"), "the elevated helper writes no blink.log: {c}");
        }
        assert!(!exit_message(EXIT_IO).contains("code"));
        assert!(!exit_message(EXIT_REGISTRY).contains("code"));
    }
}
