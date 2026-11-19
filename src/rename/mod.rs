//! The one-time `Blink` -> `Hayate` migration (contract: docs/rename-full.md).
//!
//! `early()` runs from `main` right after the elevated data-dir lock and before
//! `update::env_from_args()`, so nothing has read a data dir yet. Every step
//! checks "already done" first, is logged to `hayate.log`, and is retried on the
//! next start when it cannot run (no elevation, file busy, access denied). No
//! step raises UAC, ever.
//!
//! 0. copy `%LOCALAPPDATA%\Blink` -> `Hayate` (`Blink-multi` -> `Hayate-multi`),
//!    missing files only, never an overwrite;
//! 1. move the installed `blink.exe` to `<new dir>\hayate.exe`, relaunch it with
//!    `--rename-finish`, exit from the old path;
//! 2. `HKLM\SOFTWARE\Blink` -> `SOFTWARE\Hayate`, `BlinkPath` -> `HayatePath`;
//! 3. `Startup\Blink.lnk` -> `Hayate.lnk` + its 12-byte `StartupApproved` value;
//! 4. retarget `Hayate*.lnk`, taskbar pins and the toast `LocalServer32`;
//! 5. move the Task Manager redirect (`blink-tm.exe` -> `hayate-tm.exe`);
//! 6. `rename.done` with the version, time and the legacy paths this run saw.
//!
//! `finish()` is the second half of step 1: wait for the old pid, verify the
//! new exe, rewrite the shortcuts, remove `blink.exe` and the empty legacy
//! folder, and delete `rename.pending`.

pub mod logic;

use logic::*;
use std::os::windows::fs::MetadataExt;
use std::path::Path;

use crate::osutil::{self, log, pcw, wide};
use crate::setup::logic::{Hive, Where};
use crate::taskmgr::logic::Registry as TaskRegistry;
use crate::update::logic::{norm_path, same_dir};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

/// A string registry value as the bytes `RegSetValueExW` wants.
fn str_value(name: &str, s: &str) -> RegValue {
    let mut data = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect::<Vec<u8>>();
    data.extend([0, 0]);
    RegValue { name: name.into(), kind: RegKind::Str, data }
}

// ---- the real registry (HKCU/HKLM, 64-bit view) ---------------------------------

pub struct RealReg;

impl RealReg {
    fn root(h: Hive) -> HKEY {
        if h == Hive::User {
            HKEY_CURRENT_USER
        } else {
            HKEY_LOCAL_MACHINE
        }
    }

    fn hive_name(h: Hive) -> &'static str {
        if h == Hive::User {
            "HKCU"
        } else {
            "HKLM"
        }
    }

    fn open(&self, h: Hive, key: &str, write: bool) -> Option<HKEY> {
        let k = wide(key);
        let mut hk = HKEY::default();
        let sam = KEY_WOW64_64KEY | if write { KEY_READ | KEY_WRITE } else { KEY_READ };
        unsafe { RegOpenKeyExW(Self::root(h), pcw(&k), None, sam, &mut hk) }.is_ok().then_some(hk)
    }

    fn create(&self, h: Hive, key: &str) -> Result<HKEY, String> {
        let k = wide(key);
        let mut hk = HKEY::default();
        let r = unsafe { RegCreateKeyExW(Self::root(h), pcw(&k), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WOW64_64KEY | KEY_READ | KEY_WRITE, None, &mut hk, None) };
        if r.is_ok() {
            Ok(hk)
        } else {
            Err(format!(r"cannot open {}\{key} for writing: {}", Self::hive_name(h), r.to_hresult().message()))
        }
    }
}

impl Reg for RealReg {
    fn key_exists(&self, h: Hive, key: &str) -> bool {
        let Some(hk) = self.open(h, key, false) else { return false };
        unsafe { let _ = RegCloseKey(hk); };
        true
    }

    fn subkeys(&self, h: Hive, key: &str) -> Vec<String> {
        let Some(hk) = self.open(h, key, false) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0.. {
            let mut buf = [0u16; 260];
            let mut n = buf.len() as u32;
            if unsafe { RegEnumKeyExW(hk, i, Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut n, None, Some(windows::core::PWSTR::null()), None, None) }.is_err() {
                break;
            }
            out.push(osutil::from_wide(&buf[..n as usize]));
        }
        unsafe { let _ = RegCloseKey(hk); };
        out
    }

    fn values(&self, h: Hive, key: &str) -> Vec<RegValue> {
        let Some(hk) = self.open(h, key, false) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0.. {
            let mut name = vec![0u16; 512];
            let mut n = name.len() as u32;
            let mut ty = 0u32;
            let mut data = vec![0u8; 64 * 1024];
            let mut n = name.len() as u32;
            let mut r;
            loop {
                n = name.len() as u32;
                let mut len = data.len() as u32;
                r = unsafe { RegEnumValueW(hk, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut n, None, Some(&mut ty), Some(data.as_mut_ptr()), Some(&mut len)) };
                if r == ERROR_MORE_DATA && data.len() < 4 * 1024 * 1024 {
                    // one huge value: grow and read the same index again
                    data.resize((len as usize).max(data.len() * 2).min(4 * 1024 * 1024), 0);
                    continue;
                }
                if r.is_err() {
                    break;
                }
                data.truncate(len as usize);
                break;
            }
            if r.is_err() {
                break;
            }
            let kind = match REG_VALUE_TYPE(ty) {
                REG_SZ | REG_EXPAND_SZ => RegKind::Str,
                REG_DWORD => RegKind::Dword,
                _ => RegKind::Binary,
            };
            out.push(RegValue { name: osutil::from_wide(&name[..n as usize]), kind, data });
        }
        unsafe { let _ = RegCloseKey(hk); };
        out
    }

    fn get_str(&self, h: Hive, key: &str, name: &str) -> Option<String> {
        let v = self.values(h, key).into_iter().find(|v| v.name.eq_ignore_ascii_case(name))?;
        if v.kind != RegKind::Str {
            return None;
        }
        let units: Vec<u16> = v.data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(osutil::from_wide(&units))
    }

    fn get_binary(&self, h: Hive, key: &str, name: &str) -> Option<Vec<u8>> {
        self.values(h, key).into_iter().find(|v| v.name.eq_ignore_ascii_case(name)).map(|v| v.data)
    }

    fn set(&mut self, h: Hive, key: &str, value: &RegValue) -> Result<(), String> {
        let hk = self.create(h, key)?;
        let n = wide(&value.name);
        let ty = match value.kind {
            RegKind::Str => REG_SZ,
            RegKind::Dword => REG_DWORD,
            RegKind::Binary => REG_BINARY,
        };
        let r = unsafe { RegSetValueExW(hk, pcw(&n), None, ty, Some(&value.data)) };
        unsafe { let _ = RegCloseKey(hk); };
        if r.is_ok() {
            Ok(())
        } else {
            Err(format!(r"cannot write {}\{key} {}: {}", Self::hive_name(h), value.name, r.to_hresult().message()))
        }
    }

    fn delete_value(&mut self, h: Hive, key: &str, name: &str) -> Result<(), String> {
        let Some(hk) = self.open(h, key, true) else { return Ok(()) };
        let n = wide(name);
        let r = unsafe { RegDeleteValueW(hk, pcw(&n)) };
        unsafe { let _ = RegCloseKey(hk); };
        if r.is_ok() || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!(r"cannot delete {}\{key} {name}: {}", Self::hive_name(h), r.to_hresult().message()))
        }
    }

    fn delete_key(&mut self, h: Hive, key: &str) -> Result<(), String> {
        let (parent, leaf) = key.rsplit_once('\\').ok_or("bad key")?;
        let Some(p) = self.open(h, parent, true) else { return Ok(()) };
        let l = wide(leaf);
        let mut sub = HKEY::default();
        let ok = unsafe { RegOpenKeyExW(p, pcw(&l), None, KEY_WOW64_64KEY | KEY_READ | KEY_WRITE, &mut sub) }.is_ok();
        let mut res = Ok(());
        if ok {
            let del = unsafe { RegDeleteTreeW(sub, PCWSTR::null()) };
            unsafe { let _ = RegCloseKey(sub); };
            if del.is_err() {
                res = Err(format!(r"cannot delete {}\{key}: {}", Self::hive_name(h), del.to_hresult().message()));
            }
            let d2 = unsafe { RegDeleteKeyExW(p, pcw(&l), KEY_WOW64_64KEY.0, None) };
            if res.is_ok() && d2.is_err() && d2 != ERROR_FILE_NOT_FOUND {
                res = Err(format!(r"cannot delete {}\{key}: {}", Self::hive_name(h), d2.to_hresult().message()));
            }
        }
        unsafe { let _ = RegCloseKey(p); };
        res
    }

    fn delete_empty_key(&mut self, h: Hive, key: &str) -> Result<(), String> {
        let (parent, leaf) = key.rsplit_once('\\').ok_or("bad key")?;
        let Some(p) = self.open(h, parent, true) else { return Ok(()) };
        let l = wide(leaf);
        let r = unsafe { RegDeleteKeyExW(p, pcw(&l), KEY_WOW64_64KEY.0, None) };
        unsafe { let _ = RegCloseKey(p); };
        if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else if r == ERROR_KEY_HAS_CHILDREN {
            Err(format!(r"{}\{key} is not empty; kept", Self::hive_name(h)))
        } else {
            Err(format!(r"cannot delete {}\{key}: {}", Self::hive_name(h), r.to_hresult().message()))
        }
    }

    fn rename_key(&mut self, h: Hive, from: &str, to: &str) -> Result<bool, String> {
        let (parent, leaf) = from.rsplit_once('\\').ok_or("bad key")?;
        // RegRenameKey takes two names below the open parent, not a path
        let (to_parent, new_name) = to.rsplit_once('\\').ok_or("bad key")?;
        if !same_dir(parent, to_parent) {
            return Ok(false);
        }
        let Some(p) = self.open(h, parent, true) else { return Ok(false) };
        let (l, n) = (wide(leaf), wide(new_name));
        // KEY_WRITE on the parent + DELETE on the source is what RegRenameKey needs
        let r = unsafe { RegRenameKey(p, pcw(&l), pcw(&n)) };
        unsafe { let _ = RegCloseKey(p); };
        if r == ERROR_SUCCESS {
            Ok(true)
        } else {
            // merge instead of rename when the platform refuses; the caller copies and deletes
            log(&format!("rename: RegRenameKey {from} -> {to} failed ({}); merging instead", r.0));
            Ok(false)
        }
    }
}

// ---- entry point -----------------------------------------------------------------

/// From `main`, before `update::env_from_args()` and before anything reads the
/// data dir. Never returns a value: a needed restart ends this process.
pub fn early() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(marker) = finish_arg(&args) {
        // a finished move continues as a normal instance
        finish(Path::new(&marker));
    }
    if cli_mode(&args) {
        return;
    }
    let Some(la) = setup_local_app_data() else { return };
    // test instances: only their own multi data dir, nothing installed is touched
    if osutil::multi() {
        if osutil::env_var_os("DATA_DIR").is_none() {
            copy_data(false, true);
        }
        return;
    }
    if osutil::env_var_os("DATA_DIR").is_none() && !args_have_data_dir(&args) && !copy_data(false, false) {
        return;
    }
    let s = crate::setup::status();
    if s.at == Where::Portable {
        return;
    }
    let done = osutil::data_dir().join(DONE);
    let pending = osutil::data_dir().join(PENDING);
    if done.is_file() {
        cleanup_after_migration(&s);
        return;
    }
    if !pending.is_file() && !has_legacy_state() {
        // a fresh install: nothing to say, nothing to write
        return;
    }
    // a cut-short move: the new exe is here and valid, so only the finish half is missing
    recover_pending(&s.exe);
    if done.is_file() {
        cleanup_after_migration(&s);
        return;
    }
    let legacy_dir = match s.at {
        Where::User => crate::setup::legacy_user_dir(),
        Where::Machine => crate::setup::legacy_machine_dir(),
        Where::Portable => String::new(),
    };
    let new_dir = match s.at {
        Where::User => s.install_dir.clone(),
        Where::Machine => crate::setup::program_files().map(|p| format!(r"{p}\Hayate")).unwrap_or_default(),
        Where::Portable => String::new(),
    };
    let legacy_exe = if legacy_dir.is_empty() { String::new() } else { format!(r"{legacy_dir}\{}", LEGACY_EXE) };
    let mut seen: Vec<String> = Vec::new();
    for p in [&legacy_dir, &legacy_exe] {
        // only what was actually there: a fresh install must not record paths it invented
        if !p.is_empty() && Path::new(p).exists() {
            seen.push(p.clone());
        }
    }
    // 1. install-dir move
    // The Task Manager redirect can only be moved from an elevated start, and it
    // cold-starts the exe the legacy state names (`BlinkPath`). Moving the exe
    // without that step would leave Ctrl+Shift+Esc pointing at a deleted file, so
    // the whole move waits for an elevated start.
    if !osutil::is_elevated() && legacy_redirect_on() {
        log(&format!("rename: the Task Manager redirect still starts the pre-rename launcher; the move into {new_dir} waits for an elevated start"));
        // This process is the bridged update's trial instance: the pre-rename updater
        // watches its own state file in the old data dir and rolls back without it,
        // so confirm the build it started (this one) before staying.
        if let Some(trial) = trial_sha(&args) {
            if crate::update::sha256_file(Path::new(&s.exe)).is_ok_and(|h| h.eq_ignore_ascii_case(&trial)) {
                report_trial_ready(&la, &trial);
            }
        }
        return;
    }
    // only an image that really is the pre-rename install is moved (never whatever
    // happens to sit in a folder with that name)
    let user_new = crate::setup::user_dir();
    let machine_new = crate::setup::program_files().map(|p| format!(r"{p}\Hayate")).unwrap_or_default();
    let exe_is_legacy_install = legacy_install(&s.exe, &user_new, &machine_new, &crate::setup::legacy_user_dir(), &crate::setup::legacy_machine_dir())
        .is_some_and(|d| !d.is_empty() && !new_dir.is_empty() && norm_path(&d) == norm_path(&new_dir));
    let moved = if exe_is_legacy_install && !legacy_dir.is_empty() && !new_dir.is_empty() && norm_path(&s.exe_dir) == norm_path(&legacy_dir) {
        match move_install(&s.exe, &legacy_exe, &legacy_dir, &new_dir, &args, &la) {
            Ok(()) => {
                crate::bgio::flush(std::time::Duration::from_millis(1500));
                // the normal quit path of a start with no window yet
                std::process::exit(0);
            }
            Err(e) => {
                log(&format!("rename: install dir not moved, staying in {legacy_dir}: {e}"));
                false
            }
        }
    } else {
        true
    };
    // 2-5. registry, Startup link, shortcuts/pins, redirect
    let from_legacy = !legacy_dir.is_empty() && norm_path(&s.exe_dir) == norm_path(&legacy_dir);
    let legacy_launcher = crate::setup::program_files().and_then(|pf| crate::taskmgr::logic::legacy_expected_launcher(&pf)).unwrap_or_default();
    let (registry, key_migrated) = registry_step(osutil::is_elevated(), &legacy_launcher);
    let startup = startup_links_step(&s.exe, &legacy_exe, osutil::is_elevated());
    let links = links_step(&s.exe, &legacy_exe, osutil::is_elevated(), !from_legacy);
    // the redirect may only move once the exe it names lives in the new folder
    let redirect = !from_legacy && redirect_step(&s.exe, osutil::is_elevated());
    // never while this process still runs from the legacy folder (the rollback copy is needed there)
    let cleanup = if !moved || from_legacy { false } else if legacy_exe.is_empty() { true } else { cleanup_legacy(&legacy_exe, &legacy_dir) };
    // 9. done marker only when no step is still waiting for a later (elevated) start
    let all = moved && registry && startup && links && redirect && cleanup;
    if all {
        let mut keys: Vec<String> = Vec::new();
        if key_migrated {
            keys.push(LEGACY_KEY.to_string());
        }
        let d = Done { version: env!("CARGO_PKG_VERSION").into(), at: osutil::now_unix(), legacy: seen, keys };
        match osutil::write_atomic(&done, write_done(&d).as_bytes()) {
            Ok(()) => {
                log(&format!("rename: done ({}); legacy paths: {:?}", d.version, d.legacy));
                // a marker left by an earlier pass is finished: never let it read as pending
                let m = osutil::data_dir().join(PENDING);
                if m.is_file() {
                    let _ = std::fs::remove_file(&m);
                }
            }
            Err(e) => log(&format!("rename: done marker not written: {e}")),
        }
    } else {
        log(&format!("rename: not finished (move={moved} registry={registry} startup={startup} links={links} redirect={redirect} cleanup={cleanup}); retries on the next start"));
    }
}

fn setup_local_app_data() -> Option<String> {
    crate::setup::local_app_data()
}

/// Starts that are not a normal instance of the app: they do their own job and
/// exit (an uninstall must never move files first).
fn cli_mode(args: &[String]) -> bool {
    [
        "--uninstall",
        "--install-machine",
        "--startup-release",
        "--remove-jumplist",
        "--update-elevate",
        "--taskmgr-enable",
        "--taskmgr-disable",
        "--taskmgr-disable-legacy",
        "--taskmgr-refresh",
        "--test-launch-unelevated",
        "--test-svc",
        "--test-cpu-temp",
        "--test-gpu",
    ]
    .iter()
    .any(|a| args.iter().any(|x| x == a))
}

/// A relaunch that carries its own data dir (`--update-data-dir`): never copy.
fn args_have_data_dir(args: &[String]) -> bool {
    args.iter().any(|a| a == "--update-data-dir" || a == "--update-multi")
}

/// Anything at all from the pre-rename build. A fresh install answers false and
/// the migration stays silent (no done marker, no log lines).
fn has_legacy_state() -> bool {
    let ours_in = |d: &str| !d.is_empty() && [LEGACY_EXE, LEGACY_LAUNCHER].iter().any(|n| Path::new(d).join(n).is_file());
    let dirs = [crate::setup::legacy_user_dir(), crate::setup::legacy_machine_dir()];
    if dirs.iter().any(|d| ours_in(d)) {
        return true;
    }
    let reg = RealReg;
    if reg.key_exists(Hive::Machine, LEGACY_KEY) {
        return true;
    }
    if let Some(la) = crate::setup::local_app_data() {
        for multi in [false, true] {
            if data_dir_pair(&la, multi).0.is_dir() {
                return true;
            }
        }
    }
    for common in [false, true] {
        if crate::setup::startup_folder(common).is_some_and(|f| f.join(LEGACY_STARTUP_LINK).exists()) {
            return true;
        }
    }
    // the toast click still points at the pre-rename exe
    let key = format!(r"{}\LocalServer32", crate::setup::logic::clsid_key());
    reg.get_str(Hive::User, &key, "").is_some_and(|v| {
        let first = match v.trim().strip_prefix('"') {
            Some(r) => r.split('"').next().unwrap_or(""),
            None => v.trim().split(' ').next().unwrap_or(""),
        };
        first.rsplit(['\\', '/']).next().is_some_and(|n| n.eq_ignore_ascii_case(LEGACY_EXE))
    })
}

// ---- step 0: data dir -------------------------------------------------------------

/// Copy the legacy data dir (missing files only). `Ok(false)`: the copy failed,
/// so nothing else may run on a half-migrated state this start.
fn copy_data(_verbose: bool, multi: bool) -> bool {
    let Some(la) = setup_local_app_data() else { return true };
    let (old, new) = data_dir_pair(&la, multi);
    if !old.is_dir() {
        return true;
    }
    if !multi && new.join(DONE).is_file() {
        return true;
    }
    // an elevated start must not write through a link the user placed in their own
    // data folder (the same rule `osutil::data_dir` applies to the app's writes)
    if osutil::is_elevated() && !cfg!(test) && osutil::reparse_in_chain(&new) {
        log(&format!("rename: {} is behind a link; the data copy is skipped", new.display()));
        return false;
    }
    match copy_missing(&old, &new) {
        Ok(r) => {
            if r.files > 0 || r.dirs > 0 || !r.skipped.is_empty() {
                log(&format!("rename: data {} -> {}: {}{}", old.display(), new.display(), r.summary(), if r.skipped.is_empty() { String::new() } else { format!(" ({:?})", r.skipped) }));
            }
            true
        }
        Err(e) => {
            log(&format!("rename: data dir not copied (retries next start): {e}"));
            false
        }
    }
}

// ---- step 1: the install-dir move ---------------------------------------------------

/// An enabled Task Manager redirect written by the pre-rename build: its state
/// names `blink-tm.exe`, and only an elevated start may move it.
fn legacy_redirect_on() -> bool {
    let Some(pf) = crate::setup::program_files() else { return false };
    let Some(legacy) = crate::taskmgr::logic::legacy_expected_launcher(&pf) else { return false };
    let mut reg = crate::taskmgr::RealRegistry::new();
    let ifeo = crate::taskmgr::logic::ifeo_key(crate::taskmgr::logic::REAL_TARGET);
    reg.get(&ifeo, "Debugger").ok().flatten().is_some_and(|v| crate::taskmgr::logic::is_ours(&v, &legacy))
}

fn move_install(exe: &str, legacy_exe: &str, legacy_dir: &str, new_dir: &str, args: &[String], la: &str) -> Result<(), String> {
    let new_exe = Path::new(new_dir).join("hayate.exe");
    std::fs::create_dir_all(new_dir).map_err(|e| format!("cannot create {new_dir}: {e}"))?;
    let sha = crate::update::sha256_file(Path::new(exe)).map_err(|e| format!("cannot hash {exe}: {e}"))?;
    crate::setup::copy_exe_atomic(exe, Path::new(new_dir), &new_exe, new_dir)?;
    let copied = crate::update::sha256_file(&new_exe).map_err(|e| format!("cannot hash {}: {e}", new_exe.display()))?;
    if !copied.eq_ignore_ascii_case(&sha) {
        let _ = std::fs::remove_file(&new_exe);
        return Err(format!("the copy in {new_dir} does not match {exe}"));
    }
    let marker = osutil::data_dir().join(PENDING);
    let p = Pending {
        exe: new_exe.to_string_lossy().into_owned(),
        dir: new_dir.to_string(),
        sha: sha.clone(),
        old_pid: std::process::id(),
        legacy_dir: legacy_dir.to_string(),
        legacy_exe: legacy_exe.to_string(),
    };
    osutil::write_atomic(&marker, write_pending(&p).as_bytes()).map_err(|e| format!("cannot write {}: {e}", marker.display()))?;
    let line = crate::update::logic::command_line(&p.exe, &handoff_args(args, p.old_pid, &marker.to_string_lossy()));
    let dir = new_dir.to_string();
    match spawn(&line, &dir) {
        Ok(pid) => log(&format!("rename: {exe} copied to {} (sha {}), started pid {pid}, this process exits; the new one finishes the move", p.exe, &sha[..12])),
        Err(e) => {
            let _ = std::fs::remove_file(&marker);
            return Err(format!("cannot start {}: {e}", p.exe));
        }
    }
    // The bridged self-update: the 0.1.0 updater watches this process (its
    // child) for a ready signal in the OLD data dir. The commit rule of the
    // move is already met (hash-verified copy, new instance started), so say so
    // there before exiting; otherwise the watchdog would roll the new build back.
    if let Some(trial) = trial_sha(args).filter(|t| t.eq_ignore_ascii_case(&sha)) {
        report_trial_ready(la, &trial);
    }
    Ok(())
}

fn spawn(line: &str, dir: &str) -> Result<u32, String> {
    use windows::Win32::System::Threading::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};
    let mut cmd = wide(line);
    let app = wide(line.split('"').nth(1).unwrap_or(""));
    let cwd = wide(dir);
    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        CreateProcessW(pcw(&app), Some(windows::core::PWSTR(cmd.as_mut_ptr())), None, None, false, PROCESS_CREATION_FLAGS(0), None, pcw(&cwd), &si, &mut pi).map_err(|e| e.message())?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(pi.dwProcessId)
}

/// Tell the pre-rename updater (still watching this process) that its trial
/// passed: the new instance is hash-verified and running.
fn report_trial_ready(local_app_data: &str, sha: &str) {
    let (old, _) = data_dir_pair(local_app_data, false);
    // a rig/test copy carries its own data dir: the old updater read that one
    let f = if osutil::env_var_os("DATA_DIR").is_some() { osutil::data_dir().join("update-state.ini") } else { old.join("update-state.ini") };
    if osutil::is_elevated() && !cfg!(test) && osutil::reparse_in_chain(&f) {
        log(&format!("rename: {} is behind a link; the trial state is not written", f.display()));
        return;
    }
    let Ok(text) = std::fs::read_to_string(&f) else { return };
    let mut st = crate::update::logic::parse_state(&text);
    if !st.trial.eq_ignore_ascii_case(sha) {
        log(&format!("rename: the old updater's state names {}, not the build that moved; not marking it ready", st.trial));
        return;
    }
    st.trial_pid = std::process::id();
    st.quit_pid = 0;
    st.ready = sha.to_string();
    st.updated_to = crate::update::logic::label(None, sha);
    st.updated_at = osutil::now_unix();
    match osutil::write_durable(&f, crate::update::logic::write_state(&st).as_bytes()) {
        Ok(()) => log(&format!("rename: the bridged update trial is ready ({}); the old updater keeps this build", &sha[..12])),
        Err(e) => log(&format!("rename: could not mark the old updater's trial ready ({e}); it may roll back")),
    }
}

/// A `rename.pending` left by a cut-short move.
fn recover_pending(current_exe: &str) {
    let marker = osutil::data_dir().join(PENDING);
    let Ok(text) = std::fs::read_to_string(&marker) else { return };
    let Some(p) = parse_pending(&text) else {
        let _ = std::fs::remove_file(&marker);
        log("rename: removed an unreadable rename.pending");
        return;
    };
    let sha = crate::update::sha256_file(Path::new(&p.exe)).ok();
    match pending_check(&p, current_exe, sha.as_deref(), &known_dirs()) {
        PendingCheck::Finish => {
            log("rename: found a cut-short move; finishing it before anything else");
            finish(&marker);
        }
        PendingCheck::Restart => log("rename: found a cut-short move; the install-dir move is repeated"),
        PendingCheck::Clear(why) => {
            let _ = std::fs::remove_file(&marker);
            log(&format!("rename: cleared rename.pending ({why})"));
        }
    }
}

/// The two install layouts a marker may name, from the shell's known folders.
fn known_dirs() -> Known {
    let user = crate::setup::user_dir();
    let machine = crate::setup::program_files().map(|p| format!(r"{p}\Hayate")).unwrap_or_default();
    Known { user_dir: user, machine_dir: machine, legacy_user: crate::setup::legacy_user_dir(), legacy_machine: crate::setup::legacy_machine_dir() }
}

// ---- the finish half ----------------------------------------------------------------

/// `--rename-finish <marker>`: the new exe continues as a normal instance.
fn finish(marker: &Path) {
    let Ok(text) = std::fs::read_to_string(marker) else { return };
    let Some(p) = parse_pending(&text) else {
        let _ = std::fs::remove_file(marker);
        return;
    };
    // the old image is still the running exe until it exits
    wait_pid(p.old_pid, 15_000);
    let current = std::env::current_exe().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let sha = crate::update::sha256_file(Path::new(&p.exe)).ok();
    match pending_check(&p, &current, sha.as_deref(), &known_dirs()) {
        PendingCheck::Finish => {}
        PendingCheck::Restart => {
            log(&format!("rename: {current} is not {}; the move is not finished here", p.exe));
            return;
        }
        PendingCheck::Clear(why) => {
            let _ = std::fs::remove_file(marker);
            log(&format!("rename: cleared rename.pending ({why})"));
            return;
        }
    }
    log(&format!("rename: finishing the move into {} (old pid {})", p.dir, p.old_pid));
    // the new path's shortcuts, Startup link, pins and toast registration
    let links = links_step(&p.exe, &p.legacy_exe, osutil::is_elevated(), true);
    let startup = startup_links_step(&p.exe, &p.legacy_exe, osutil::is_elevated());
    // everything that names the old path, whether or not the old exe may go yet
    rewrite_clsid(&p.exe, &p.legacy_exe);
    rewrite_update_ini(&p.dir, &p.legacy_dir);
    if let Some(msg) = crate::setup::arp_after_move(&crate::setup::status().at, &p.dir) {
        log(&format!("rename: {msg}"));
    }
    // the exe itself may only go once the redirect no longer starts it; the normal
    // flow retries the same cleanup after its redirect step
    let cleaned = cleanup_legacy(&p.legacy_exe, &p.legacy_dir);
    if !links || !startup || !cleaned {
        log("rename: the move is not complete yet; rename.pending stays for the next start");
        return;
    }
    match std::fs::remove_file(marker) {
        Ok(()) => log("rename: move finished, rename.pending removed"),
        Err(e) => log(&format!("rename: could not remove {}: {e}", marker.display())),
    }
}

/// Delete the legacy image (and a pre-rename updater's leftovers beside it),
/// then the folder when nothing else is in it.
fn cleanup_legacy(legacy_exe: &str, legacy_dir: &str) -> bool {
    if legacy_exe.is_empty() || !Path::new(legacy_exe).exists() {
        remove_empty_dir(legacy_dir);
        return true;
    }
    // a redirect that still cold-starts this exe keeps it: deleting it would leave
    // Ctrl+Shift+Esc pointing at nothing (only possible when a step failed)
    if legacy_redirect_on() {
        log("rename: the Task Manager redirect still starts the pre-rename launcher; its exe is kept");
        return false;
    }
    let mut ok = true;
    for name in [LEGACY_EXE, "blink.old.exe", "blink.new.exe", "blink.bad.exe", "version.txt", "version.old.txt"] {
        let f = Path::new(legacy_dir).join(name);
        if f.exists() && std::fs::remove_file(&f).is_err() {
            ok = false;
        }
    }
    if ok {
        remove_empty_dir(legacy_dir);
        log(&format!("rename: removed {legacy_exe}"));
    } else {
        log(&format!("rename: {legacy_exe} is still in use; the next start removes it"));
    }
    ok
}

/// v0.2.1: after the migration finished, the pre-rename leftovers go: the data
/// folder the migration copied (only while it is provably just that copy), a
/// legacy install folder the migration recorded, and an emptied `SOFTWARE\Blink`
/// it renamed. One marker per profile; a step that cannot run yet keeps it from
/// being written, so the next start retries.
fn cleanup_after_migration(s: &crate::setup::Status) {
    let Some(la) = setup_local_app_data() else { return };
    // the marker sits with the profile, not in a DATA_DIR override: the folder it
    // describes belongs to the profile
    let profile = Path::new(&la).join("Hayate");
    let marker = profile.join(CLEANUP_DONE);
    let prev = std::fs::read_to_string(&marker).ok();
    if let Some(t) = &prev {
        // the only step that can still be owed is the emptied key (elevated only)
        if !t.lines().any(|l| l.trim() == "elevated=0") {
            return;
        }
    }
    let done = std::fs::read_to_string(osutil::data_dir().join(DONE)).ok().and_then(|t| parse_done(&t)).unwrap_or_default();
    let mut removed: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    if prev.is_none() {
        // the copied data folder: exactly the legacy leaf under this profile, never a
        // link, never a parent/child of the install, and only while every file in it
        // is byte-identical to the copy (a 0.1.0 write or a skipped file blocks it)
        let old = Path::new(&la).join(LEGACY_DIR);
        let old_s = old.to_string_lossy().into_owned();
        if old.is_dir() && !osutil::reparse_in_chain(&old) && crate::setup::logic::legacy_data_dir_removable(&old_s, &s.exe_dir) {
            match legacy_data_is_pure_copy(&old, &profile) {
                Ok(n) => match remove_tree_files(&old) {
                    Ok(()) => {
                        removed.push(format!("{old_s} ({n} files)"));
                        log(&format!("rename: cleanup removed {old_s} ({n} files, all identical to the copy)"));
                    }
                    Err(e) => failed.push(format!("{old_s}: {e}")),
                },
                Err(why) => {
                    log(&format!("rename: {old_s} is kept: {why}"));
                    kept.push(format!("{old_s}: {why}"));
                }
            }
        }
        // a legacy install folder the migration recorded, holding only our leftovers
        let leg = match s.at {
            Where::User => crate::setup::legacy_user_dir(),
            Where::Machine => crate::setup::legacy_machine_dir(),
            Where::Portable => String::new(),
        };
        if !leg.is_empty() && done.legacy.iter().any(|p| same_dir(p, &leg)) && Path::new(&leg).is_dir() {
            if cleanup_legacy(&format!(r"{leg}\{LEGACY_EXE}"), &leg) && !Path::new(&leg).exists() {
                removed.push(leg.clone());
            }
        }
    }
    // the key the migration renamed, when it is now empty. `RegDeleteKeyExW` refuses
    // anything with subkeys, so a foreign non-empty key is never emptied
    let key_owed = done.keys.iter().any(|k| k.eq_ignore_ascii_case(LEGACY_KEY)) && RealReg.key_exists(Hive::Machine, LEGACY_KEY);
    if key_owed {
        if osutil::is_elevated() {
            match RealReg.delete_empty_key(Hive::Machine, LEGACY_KEY) {
                Ok(()) => removed.push(format!("HKLM\\{LEGACY_KEY}")),
                Err(e) => failed.push(format!("HKLM\\{LEGACY_KEY}: {e}")),
            }
        } else {
            failed.push("HKLM\\SOFTWARE\\Blink needs an elevated start".into());
        }
    }
    if !failed.is_empty() {
        log(&format!("rename: cleanup not finished (removed: {removed:?}, waiting: {failed:?}); retries on the next start"));
        return;
    }
    let elevated = if key_owed && !osutil::is_elevated() { 0 } else { 1 };
    let text = format!(
        "; Hayate cleanup of the pre-rename leftovers (written by Hayate; safe to delete)\nversion={}\nat={}\nelevated={}\n{}{}",
        env!("CARGO_PKG_VERSION"),
        osutil::now_unix(),
        elevated,
        removed.iter().map(|r| format!("removed={r}\n")).collect::<String>(),
        kept.iter().map(|k| format!("kept={k}\n")).collect::<String>()
    );
    match osutil::write_atomic(&marker, text.as_bytes()) {
        Ok(()) => log(&format!("rename: cleanup done (removed: {removed:?}, kept: {kept:?})")),
        Err(e) => log(&format!("rename: cleanup marker not written: {e}")),
    }
}

/// Delete a tree file by file, then its folders bottom-up: a file another process
/// still holds fails the step (nothing is forced) and no reparse point is followed.
fn remove_tree_files(root: &Path) -> Result<(), String> {
    fn go(dir: &Path) -> Result<(), String> {
        for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let e = e.map_err(|e| format!("{}: {e}", dir.display()))?;
            let p = e.path();
            let m = std::fs::symlink_metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            if m.file_attributes() & 0x400 != 0 {
                return Err(format!("{} is a link", p.display()));
            }
            if m.is_dir() {
                go(&p)?;
                std::fs::remove_dir(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            } else {
                std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            }
        }
        Ok(())
    }
    go(root)?;
    std::fs::remove_dir(root).map_err(|e| format!("{}: {e}", root.display()))
}

/// `update.ini` still names the pre-rename folder (it is what the 0.1.0 updater
/// wrote): point it at the new one. Best effort, the updater accepts either.
fn rewrite_update_ini(new_dir: &str, legacy_dir: &str) {
    let f = osutil::data_dir().join("update.ini");
    let Ok(text) = std::fs::read_to_string(&f) else { return };
    let Some(moved) = update_ini_moved(&text, legacy_dir, new_dir) else { return };
    match osutil::write_atomic(&f, moved.as_bytes()) {
        Ok(()) => log(&format!("rename: update.ini now names {new_dir}")),
        Err(e) => log(&format!("rename: update.ini not rewritten: {e}")),
    }
}

fn remove_empty_dir(dir: &str) {    if dir.is_empty() {
        return;
    }
    let p = Path::new(dir);
    if p.is_dir() && std::fs::read_dir(p).map(|mut d| d.next().is_none()).unwrap_or(false) {
        match std::fs::remove_dir(p) {
            Ok(()) => log(&format!("rename: removed the empty legacy folder {dir}")),
            Err(e) => log(&format!("rename: could not remove the empty legacy folder {dir}: {e}")),
        }
    }
}

fn wait_pid(pid: u32, ms: u32) {
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(h, ms);
            let _ = CloseHandle(h);
        }
    }
}

// ---- step 2: registry ----------------------------------------------------------------

/// `true`: nothing is left for a later (elevated) start.
fn registry_step(elevated: bool, legacy_launcher: &str) -> (bool, bool) {
    if !elevated {
        let reg = RealReg;
        let pending = reg.key_exists(Hive::Machine, LEGACY_KEY);
        if pending {
            log("rename: HKLM\\SOFTWARE\\Blink needs an elevated start; retries");
        }
        return (!pending, false);
    }
    let mut reg = RealReg;
    let mut key_migrated = false;
    match migrate_key(&mut reg, Hive::Machine, LEGACY_KEY, r"SOFTWARE\Hayate", legacy_launcher) {
        Ok(o) => {
            key_migrated = matches!(o, KeyOutcome::Renamed | KeyOutcome::Merged);
            log(&format!("rename: HKLM\\SOFTWARE\\Blink -> Hayate: {o:?}"));
        }
        Err(e) => {
            log(&format!("rename: registry key not moved (retries next start): {e}"));
            return (false, false);
        }
    }
    let state_root = crate::taskmgr::logic::STATE_KEY;
    if !reg.key_exists(Hive::Machine, state_root) {
        return (true, key_migrated);
    }
    let mut ok = true;
    for target in reg.subkeys(Hive::Machine, state_root) {
        let key = format!(r"{state_root}\{target}");
        match rename_path_value(&mut reg, Hive::Machine, &key) {
            Ok(true) => log(&format!("rename: {}\\{target}: BlinkPath -> HayatePath", state_root)),
            Ok(false) => {}
            Err(e) => {
                ok = false;
                log(&format!("rename: {}\\{target}: BlinkPath not renamed: {e}", state_root));
            }
        }
    }
    (ok, key_migrated)
}

// ---- step 3: Startup link + StartupApproved ------------------------------------------

fn startup_links_step(new_exe: &str, legacy_exe: &str, elevated: bool) -> bool {
    if legacy_exe.is_empty() {
        return true;
    }
    let mut ok = true;
    for common in [false, true] {
        let Some(folder) = crate::setup::startup_folder(common) else {
            ok = false;
            continue;
        };
        let old = folder.join(LEGACY_STARTUP_LINK);
        let new = folder.join(crate::setup::logic::LINK_AUTO);
        // something to do here? the legacy link, or a Hayate.lnk an earlier start
        // left pointing at the old exe
        let pending = crate::setup::on_sta({
            let (old, new, legacy) = (old.clone(), new.clone(), legacy_exe.to_string());
            move || {
                old.exists()
                    || (new.exists() && crate::setup::link_target(&new).is_some_and(|t| norm_path(&t).eq_ignore_ascii_case(&norm_path(&legacy))))
            }
        })
        .unwrap_or(false);
        if !pending {
            continue;
        }
        if common && !elevated {
            ok = false;
            continue;
        }
        let r = crate::setup::on_sta({
            let (old, new, legacy) = (old.clone(), new.clone(), legacy_exe.to_string());
            let target = new_exe.to_string();
            move || {
                if old.exists() {
                    let Some(t) = crate::setup::link_target(&old) else { return Err("the legacy link cannot be read".to_string()) };
                    if !norm_path(&t).eq_ignore_ascii_case(&norm_path(&legacy)) {
                        return Err(format!("{old:?} does not start {legacy}; left alone"));
                    }
                    if !new.exists() {
                        crate::setup::clone_link_as(&old, &new, Path::new(&target)).map_err(|e| format!("cannot write {new:?}: {}", e.message()))?;
                    } else if crate::setup::link_target(&new).is_some_and(|t| norm_path(&t).eq_ignore_ascii_case(&norm_path(&legacy))) {
                        // an earlier start made Hayate.lnk point at blink.exe: retarget it
                        crate::setup::rewrite_link(&new, Path::new(&target), None, false).map_err(|e| format!("cannot rewrite {new:?}: {}", e.message()))?;
                    }
                    let mut reg = RealReg;
                    match move_approved(&mut reg, Hive::User, APPROVED_KEY, LEGACY_STARTUP_LINK, crate::setup::logic::LINK_AUTO) {
                        Ok(true) => log("rename: StartupApproved value moved to Hayate.lnk"),
                        Ok(false) => {}
                        Err(e) => log(&format!("rename: StartupApproved value not moved: {e}")),
                    }
                    std::fs::remove_file(&old).map_err(|e| format!("cannot remove {old:?}: {e}"))?;
                } else {
                    // no legacy link left, only the retarget above
                    crate::setup::rewrite_link(&new, Path::new(&target), None, false).map_err(|e| format!("cannot rewrite {new:?}: {}", e.message()))?;
                }
                Ok(())
            }
        });
        match r {
            Some(Ok(())) => log(&format!("rename: Startup link {} -> {}", old.display(), new.display())),
            Some(Err(e)) => {
                ok = false;
                log(&format!("rename: Startup link not moved: {e}"));
            }
            None => {
                ok = false;
                log("rename: the shortcut thread failed");
            }
        }
    }
    ok
}

// ---- step 4: Start-menu links, taskbar pins, toast key --------------------------------

fn links_step(new_exe: &str, legacy_exe: &str, elevated: bool, sweep_legacy: bool) -> bool {
    if legacy_exe.is_empty() {
        return true;
    }
    // shortcuts in the all-users menu need an elevated start; leaving them is not "done"
    let common_pending = !elevated
        && crate::setup::on_sta({
            let legacy = legacy_exe.to_string();
            move || {
                let Some(menu) = crate::setup::known_folder(&windows::Win32::UI::Shell::FOLDERID_CommonPrograms) else { return false };
                crate::setup::logic::MENU_LINKS.into_iter().any(|n| {
                    let f = menu.join(n);
                    f.exists() && crate::setup::link_target(&f).is_some_and(|t| norm_path(&t).eq_ignore_ascii_case(&norm_path(&legacy)))
                })
            }
        })
        .unwrap_or(false);
    let menus: Vec<(&'static windows::core::GUID, bool)> = if elevated {
        vec![(&windows::Win32::UI::Shell::FOLDERID_Programs, false), (&windows::Win32::UI::Shell::FOLDERID_CommonPrograms, false)]
    } else {
        vec![(&windows::Win32::UI::Shell::FOLDERID_Programs, false)]
    };
    let r = crate::setup::on_sta({
        let (target, legacy) = (new_exe.to_string(), legacy_exe.to_string());
        let menus = menus.clone();
        move || {
            let mut ok = true;
            for (id, _) in menus {
                let Some(menu) = crate::setup::known_folder(id) else { continue };
                for (name, app_id, toast) in [
                    (crate::setup::logic::LINK_MAIN, crate::notify::AUMID.to_string(), true),
                    (crate::setup::logic::LINK_ADMIN, format!("{}.Admin", crate::notify::AUMID), false),
                    (crate::setup::logic::LINK_PLAIN, format!("{}.User", crate::notify::AUMID), false),
                ] {
                    let f = menu.join(name);
                    if !f.exists() {
                        continue;
                    }
                    if crate::setup::link_target(&f).is_some_and(|t| norm_path(&t).eq_ignore_ascii_case(&norm_path(&legacy))) {
                        match crate::setup::rewrite_link(&f, Path::new(&target), Some(&app_id), toast) {
                            Ok(()) => log(&format!("rename: shortcut {} now starts {target}", f.display())),
                            Err(e) => {
                                ok = false;
                                log(&format!("rename: shortcut {} not rewritten: {}", f.display(), e.message()));
                            }
                        }
                    }
                }
                // the pre-rename names are swept once the exe they start is not this process
                // (while the move is still pending they are the user's only Start-menu entry)
                if sweep_legacy {
                    for old in crate::setup::logic::LEGACY_MENU_LINKS {
                        let f = menu.join(old);
                        if f.exists() && crate::setup::link_target(&f).is_some_and(|t| norm_path(&t).eq_ignore_ascii_case(&norm_path(&legacy))) {
                            let _ = std::fs::remove_file(&f);
                        }
                    }
                }
            }
            // taskbar pins (best effort: Windows may keep a cached copy)
            if let Some(roaming) = crate::setup::roaming_app_data() {
                let dir = Path::new(&roaming).join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar");
                for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let f = e.path();
                    if f.extension().is_none_or(|x| !x.eq_ignore_ascii_case("lnk")) {
                        continue;
                    }
                    if crate::setup::link_target(&f).is_some_and(|t| norm_path(&t).eq_ignore_ascii_case(&norm_path(&legacy))) {
                        match crate::setup::rewrite_link(&f, Path::new(&target), Some(crate::notify::AUMID), true) {
                            Ok(()) => log(&format!("rename: taskbar pin {} now starts {target}", f.display())),
                            Err(e) => log(&format!("rename: taskbar pin {} not rewritten (re-pin it): {}", f.display(), e.message())),
                        }
                    }
                }
            }
            ok
        }
    });
    let ok = r.unwrap_or(false) && !common_pending;
    if !ok {
        log("rename: some Start-menu links could not be rewritten; retries on the next start");
    }
    ok
}

fn rewrite_clsid(new_exe: &str, legacy_exe: &str) {
    let mut reg = RealReg;
    let key = format!(r"{}\LocalServer32", crate::setup::logic::clsid_key());
    let Some(v) = reg.get_str(Hive::User, &key, "") else { return };
    let first = match v.trim().strip_prefix('"') {
        Some(r) => r.split('"').next().unwrap_or(""),
        None => v.trim().split(' ').next().unwrap_or(""),
    };
    if !norm_path(first).eq_ignore_ascii_case(&norm_path(legacy_exe)) {
        return;
    }
    let args = crate::update::logic::relaunch_args(&[], &crate::setup::env_restore());
    let server = crate::setup::logic::local_server(new_exe, &args);
    let value = str_value("", &server);
    match reg.set(Hive::User, &key, &value) {
        Ok(()) => log(&format!("rename: toast LocalServer32 now starts {new_exe}")),
        Err(e) => log(&format!("rename: toast LocalServer32 not rewritten: {e}")),
    }
}

// ---- step 5: Task Manager redirect ----------------------------------------------------

/// Move an enabled redirect from `blink-tm.exe` to `hayate-tm.exe`. A failure
/// puts the legacy redirect back so Ctrl+Shift+Esc always opens something.
fn redirect_step(new_exe: &str, elevated: bool) -> bool {
    let Some(pf) = crate::setup::program_files() else { return true };
    let Some(legacy_launcher) = crate::taskmgr::logic::legacy_expected_launcher(&pf) else { return true };
    let mut reg = crate::taskmgr::RealRegistry::new();
    let ifeo = crate::taskmgr::logic::ifeo_key(crate::taskmgr::logic::REAL_TARGET);
    let cur = reg.get(&ifeo, "Debugger").ok().flatten();
    let Some(value) = cur else {
        // Windows or the user reset it: nothing of ours is left there
        return true;
    };
    if crate::taskmgr::logic::mentions_legacy(&value) && !crate::taskmgr::logic::is_ours(&value, &legacy_launcher) {
        log(&format!("rename: Task Manager Debugger {value} mentions the legacy launcher but is not ours; left alone"));
        return true;
    }
    if !crate::taskmgr::logic::is_ours(&value, &legacy_launcher) {
        return true;
    }
    if !elevated {
        log("rename: the Task Manager redirect still uses blink-tm.exe; needs an elevated start");
        return false;
    }
    let target = crate::taskmgr::logic::REAL_TARGET;
    let saved = crate::taskmgr::logic::read_state(&reg, target);
    // keep a copy of the legacy launcher: it is the restore path when the new
    // redirect fails to come up
    // the new launcher must be a real, hash-checked copy of this image before the
    // working redirect is touched: a half-copied or missing file here would be the
    // only launcher left
    let new_launcher = crate::taskmgr::logic::expected_launcher(&pf).unwrap_or_default();
    let prep = (|| -> Result<(), String> {
        if new_launcher.is_empty() {
            return Err("the new launcher path is unknown".into());
        }
        let dir = Path::new(&new_launcher).parent().ok_or("the new launcher path has no folder")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        std::fs::copy(&legacy_launcher, &new_launcher).map_err(|e| format!("cannot copy the launcher: {e}"))?;
        let a = crate::update::sha256_file(Path::new(&legacy_launcher))?;
        let b = crate::update::sha256_file(Path::new(&new_launcher))?;
        if !a.eq_ignore_ascii_case(&b) {
            return Err("the new launcher copy does not match the legacy one".into());
        }
        Ok(())
    })();
    if let Err(e) = prep {
        log(&format!("rename: the Task Manager redirect is left as it is ({e})"));
        return false;
    }
    let mut host = crate::taskmgr::RealHost;
    match crate::taskmgr::logic::disable_legacy(&mut reg, &mut host, target, &pf) {
        Ok(o) => log(&format!("rename: legacy Task Manager redirect turned off: {o:?}")),
        Err(e) => {
            log(&format!("rename: legacy Task Manager redirect not touched: {e}"));
            return false;
        }
    }
    match crate::taskmgr::logic::enable(&mut reg, &mut host, target, &pf, new_exe, None) {
        Ok(o) => {
            log(&format!("rename: Task Manager redirect now starts hayate-tm.exe: {o:?}"));
            true
        }
        Err(e) => {
            log(&format!("rename: the new Task Manager redirect failed ({e}); putting the legacy one back"));
            restore_legacy_redirect(&mut reg, saved.as_ref(), target, &legacy_launcher, &new_launcher, &value)
        }
    }
}

/// Best effort: the exact legacy value and state back, so the redirect works as
/// it did before the attempt.
fn restore_legacy_redirect(reg: &mut crate::taskmgr::RealRegistry, saved: Option<&crate::taskmgr::logic::State>, target: &str, legacy_launcher: &str, new_launcher: &str, value: &str) -> bool {
    if !Path::new(legacy_launcher).exists() && Path::new(new_launcher).exists() {
        if let Some(dir) = Path::new(legacy_launcher).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::copy(new_launcher, legacy_launcher);
    }
    let ifeo = crate::taskmgr::logic::ifeo_key(target);
    // never point Windows at a launcher that is not there: with no file at either
    // path the value goes, so Ctrl+Shift+Esc opens the native Task Manager again
    let restored = Path::new(legacy_launcher).is_file() || Path::new(new_launcher).is_file();
    let r = if restored { reg.set(&ifeo, "Debugger", value) } else { reg.delete_value(&ifeo, "Debugger") };
    let mut ok = restored;
    if let Err(e) = r {
        log(&format!("rename: could not put the Task Manager value back: {e}"));
        ok = false;
    } else if !restored {
        log("rename: no launcher file is left; the Debugger value was removed so the native Task Manager opens");
    }
    if let Some(s) = saved {
        for (n, v) in [
            ("SchemaVersion", s.schema.as_deref()),
            ("Enabled", Some(if s.enabled { "1" } else { "0" })),
            ("LauncherPath", Some(s.launcher.as_str())),
            ("HayatePath", Some(s.hayate_path.as_str())),
            ("PreviousDebuggerExisted", s.prev_existed.as_deref()),
            ("NativePath", s.native_path.as_deref()),
        ] {
            if let Some(v) = v {
                let _ = reg.set(&crate::taskmgr::logic::state_key(target), n, v);
            }
        }
        let build = s.build.map(|b| b.to_string());
        if let Some(b) = build {
            let _ = reg.set(&crate::taskmgr::logic::state_key(target), "BuildOrdinal", &b);
        }
    }
    log(&format!("rename: legacy Task Manager redirect restored (working: {ok})"));
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real `RegRenameKey` + value move, on disposable HKCU keys (created and
    /// removed by this test; no other user key is touched).
    #[test]
    fn real_hkcu_key_rename_and_value_move_work() {
        let tag = format!("HayateRenameTest{}", std::process::id());
        let (old, new) = (format!(r"Software\{tag}Old"), format!(r"Software\{tag}New"));
        let mut reg = RealReg;
        let _ = reg.delete_key(Hive::User, &old);
        let _ = reg.delete_key(Hive::User, &new);
        let state = format!(r"{old}\TaskManager\taskmgr.exe");
        for (n, v) in [("SchemaVersion", "1"), ("Enabled", "1"), ("LauncherPath", r"C:\Program Files\Blink\blink-tm.exe"), ("BlinkPath", r"C:\Program Files\Blink\blink.exe")] {
            reg.set(Hive::User, &state, &str_value(n, v)).unwrap();
        }
        let (o2, n2) = (format!(r"Software\{tag}O2"), format!(r"Software\{tag}N2"));
        reg.set(Hive::User, &o2, &str_value("x", "1")).unwrap();
        assert_eq!(RealReg.rename_key(Hive::User, &o2, &n2), Ok(true), "the real RegRenameKey works");
        assert!(RealReg.key_exists(Hive::User, &n2) && !RealReg.key_exists(Hive::User, &o2));
        let _ = reg.delete_key(Hive::User, &o2);
        let _ = reg.delete_key(Hive::User, &n2);
        let mut reg = RealReg;
        assert_eq!(migrate_key(&mut reg, Hive::User, &old, &new, r"C:\Program Files\Blink\blink-tm.exe").unwrap(), KeyOutcome::Renamed);        assert!(!reg.key_exists(Hive::User, &old));
        let moved = format!(r"{new}\TaskManager\taskmgr.exe");
        assert_eq!(reg.get_str(Hive::User, &moved, "Enabled").as_deref(), Some("1"), "TaskManager came along");
        assert!(rename_path_value(&mut reg, Hive::User, &moved).unwrap());
        assert_eq!(reg.get_str(Hive::User, &moved, "HayatePath").as_deref(), Some(r"C:\Program Files\Blink\blink.exe"));
        assert_eq!(reg.get_str(Hive::User, &moved, "BlinkPath"), None);
        assert_eq!(move_approved(&mut reg, Hive::User, APPROVED_KEY, &format!("{tag}.lnk"), &format!("{tag}2.lnk")).unwrap(), false, "no value, nothing to move");
        let _ = reg.delete_key(Hive::User, &new);
        let _ = reg.delete_key(Hive::User, &old);
    }

    /// The Startup link copy: a fresh link to the new exe keeps the arguments.
    #[test]
    fn startup_link_is_cloned_to_the_new_exe() {
        let dir = std::env::temp_dir().join(format!("hayate-rename-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("Blink.lnk"), dir.join("Hayate.lnk"));
        let (legacy, fresh) = (dir.join("blink.exe"), dir.join("hayate.exe"));
        std::fs::write(&legacy, b"x").unwrap();
        std::fs::write(&fresh, b"x").unwrap();
        let got = crate::setup::on_sta({
            let (old, new, legacy, fresh) = (old.clone(), new.clone(), legacy.clone(), fresh.clone());
            move || -> Option<String> {
                crate::setup::write_link(&old, &legacy, &crate::setup::logic::autostart_link()).ok()?;
                let before = crate::setup::link_target(&old)?;
                if norm_path(&before) != norm_path(&legacy.to_string_lossy()) {
                    return None;
                }
                crate::setup::clone_link_as(&old, &new, &fresh).ok()?;
                crate::setup::link_target(&new)
            }
        })
        .flatten();
        assert_eq!(got.as_deref().map(norm_path), Some(norm_path(&fresh.to_string_lossy())), "the cloned Startup link starts the new exe");
        let _ = std::fs::remove_dir_all(&dir);
    }
}



