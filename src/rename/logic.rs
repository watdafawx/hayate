//! Pure parts of the `Blink` -> `Hayate` migration (docs/rename-full.md):
//! the data-dir copy, the pending/done markers, the install-path decision, the
//! registry key/value move and the `StartupApproved` byte move. No Win32 here:
//! the file work uses `std::fs`, the registry is the [`Reg`] seam, so the whole
//! decision surface is unit-tested. `rename/mod.rs` is the thin OS layer.

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::setup::logic::Hive;
use crate::update::logic::{norm_path, same_dir};

/// Data folder of the pre-rename build (below `%LOCALAPPDATA%`).
pub const LEGACY_DIR: &str = "Blink";
/// Data folder of the pre-rename test instance.
pub const LEGACY_MULTI: &str = "Blink-multi";
/// The pre-rename executable.
pub const LEGACY_EXE: &str = "blink.exe";
/// The pre-rename Task Manager launcher.
pub const LEGACY_LAUNCHER: &str = "blink-tm.exe";
/// The pre-rename Startup-folder shortcut.
pub const LEGACY_STARTUP_LINK: &str = "Blink.lnk";
/// HKLM key of the pre-rename build.
pub const LEGACY_KEY: &str = r"SOFTWARE\Blink";
/// The pre-rename value naming the installed exe in the Task Manager state.
pub const LEGACY_PATH_VALUE: &str = "BlinkPath";
/// `StartupApproved\StartupFolder` value names are the link file names.
pub const APPROVED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";
/// The 12-byte value Windows keeps per Startup link (enabled byte + timestamp).
pub const APPROVED_LEN: usize = 12;

/// One-time marker in the new data dir: every step ran (or was not needed).
pub const DONE: &str = "rename.done";
/// Commit marker of the install-dir move.
pub const PENDING: &str = "rename.pending";

fn file(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

fn parent(p: &str) -> &str {
    match p.rfind(['\\', '/']) {
        Some(i) => &p[..i],
        None => "",
    }
}

/// `%LOCALAPPDATA%\Blink` -> `%LOCALAPPDATA%\Hayate` (and `-multi`).
pub fn data_dir_pair(local_app_data: &str, multi: bool) -> (PathBuf, PathBuf) {
    let la = local_app_data.trim_end_matches(['\\', '/']);
    let (old, new) = if multi {
        (LEGACY_MULTI, "Hayate-multi")
    } else {
        (LEGACY_DIR, "Hayate")
    };
    (Path::new(la).join(old), Path::new(la).join(new))
}

/// `%LOCALAPPDATA%\Programs\Blink` / `%LOCALAPPDATA%\Programs\Hayate`.
pub fn legacy_user_install(local_app_data: &str) -> String {
    format!(r"{}\Programs\{}", local_app_data.trim_end_matches(['\\', '/']), LEGACY_DIR)
}

/// `%ProgramFiles%\Blink` / `%ProgramFiles%\Hayate`.
pub fn legacy_machine_install(program_files: &str) -> String {
    format!(r"{}\{}", program_files.trim_end_matches(['\\', '/']), LEGACY_DIR)
}

/// What `copy_missing` did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CopyReport {
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    /// entries left alone (a link, an unreadable file)
    pub skipped: Vec<String>,
}

impl CopyReport {
    pub fn summary(&self) -> String {
        format!("{} files, {} folders, {} bytes{}", self.files, self.dirs, self.bytes, if self.skipped.is_empty() { String::new() } else { format!(", {} skipped", self.skipped.len()) })
    }
}

/// Copy every file/subfolder of `src` into `dst` that is not there yet. Never
/// overwrites, never deletes, never follows a link. Missing `src` = nothing to
/// do. The same function covers `Blink` -> `Hayate` and `Blink-multi` ->
/// `Hayate-multi`.
pub fn copy_missing(src: &Path, dst: &Path) -> Result<CopyReport, String> {
    let mut r = CopyReport::default();
    if !src.is_dir() {
        return Ok(r);
    }
    copy_tree(src, dst, &mut r)?;
    Ok(r)
}

fn copy_tree(src: &Path, dst: &Path, r: &mut CopyReport) -> Result<(), String> {
    // The destination is the user's own data folder, which that user can turn into
    // a junction or symlink. Never follow a link there (the app's own data dir does
    // the same check for an elevated start): refuse instead of copying through it.
    match std::fs::symlink_metadata(dst) {
        Ok(m) if m.file_attributes() & 0x400 != 0 => return Err(format!("rename: {} is a link; not copied into", dst.display())),
        Ok(m) if !m.is_dir() => return Err(format!("rename: {} is not a folder", dst.display())),
        Ok(_) => {}
        Err(_) => std::fs::create_dir_all(dst).map_err(|e| format!("rename: cannot create {}: {e}", dst.display()))?,
    }
    let rd = std::fs::read_dir(src).map_err(|e| format!("rename: cannot read {}: {e}", src.display()))?;
    for e in rd {
        let e = match e {
            Ok(e) => e,
            Err(e) => {
                r.skipped.push(format!("{}: {e}", src.display()));
                continue;
            }
        };
        let from = e.path();
        let to = dst.join(e.file_name());
        // an existing destination entry is never replaced, and never followed (a
        // dangling link reports exists() == false and would write through it)
        if let Ok(m) = std::fs::symlink_metadata(&to) {
            if m.file_attributes() & 0x400 != 0 || !m.is_dir() {
                r.skipped.push(to.display().to_string());
                continue;
            }
        }
        let meta = match std::fs::symlink_metadata(&from) {
            Ok(m) => m,
            Err(e) => {
                r.skipped.push(format!("{}: {e}", from.display()));
                continue;
            }
        };
        // a link is never followed out of the user's data dir
        if meta.file_attributes() & 0x400 != 0 {
            r.skipped.push(from.display().to_string());
            continue;
        }
        if meta.is_dir() {
            match copy_tree(&from, &to, r) {
                Ok(()) => r.dirs += 1,
                Err(e) => r.skipped.push(e),
            }
            continue;
        }
        if meta.is_file() {
            if to.exists() {
                continue;
            }
            match std::fs::copy(&from, &to) {
                Ok(n) => {
                    r.files += 1;
                    r.bytes += n;
                }
                Err(e) => r.skipped.push(format!("{}: {e}", from.display())),
            }
        }
    }
    Ok(())
}

/// Every file in the pre-rename data folder has an identical file in the new one:
/// the folder is exactly what the migration copied, with nothing written after.
/// Returns the file count; `Err` says why the folder must be kept.
pub fn legacy_data_is_pure_copy(old: &Path, new: &Path) -> Result<usize, String> {
    if !old.is_dir() {
        return Err("no pre-rename data folder".into());
    }
    fn walk(old: &Path, new: &Path, n: &mut usize) -> Result<(), String> {
        let rd = std::fs::read_dir(old).map_err(|e| format!("{}: {e}", old.display()))?;
        for e in rd {
            let e = e.map_err(|e| format!("{}: {e}", old.display()))?;
            let from = e.path();
            let m = std::fs::symlink_metadata(&from).map_err(|e| format!("{}: {e}", from.display()))?;
            if m.file_attributes() & 0x400 != 0 {
                return Err(format!("{} is a link", from.display()));
            }
            let to = new.join(e.file_name());
            if m.is_dir() {
                let tm = std::fs::symlink_metadata(&to).map_err(|_| format!("{} is not in the new folder", from.display()))?;
                if tm.file_attributes() & 0x400 != 0 || !tm.is_dir() {
                    return Err(format!("{} is not a folder in the new one", from.display()));
                }
                walk(&from, &to, n)?;
                continue;
            }
            if !m.is_file() {
                continue;
            }
            let tm = std::fs::symlink_metadata(&to).map_err(|_| format!("{} was never copied", from.display()))?;
            if tm.file_attributes() & 0x400 != 0 || !tm.is_file() || tm.len() != m.len() {
                return Err(format!("{} differs from the copy", from.display()));
            }
            let a = std::fs::read(&from).map_err(|e| format!("{}: {e}", from.display()))?;
            let b = std::fs::read(&to).map_err(|e| format!("{}: {e}", to.display()))?;
            if a != b {
                return Err(format!("{} was written after the copy", from.display()));
            }
            *n += 1;
        }
        Ok(())
    }
    let mut n = 0;
    walk(old, new, &mut n)?;
    Ok(n)
}

// ---- markers -------------------------------------------------------------------

/// `rename.pending`: the commit marker of the install-dir move.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pending {
    /// `hayate.exe` at the new install path
    pub exe: String,
    /// the new install folder
    pub dir: String,
    /// sha256 of the exe the move copied
    pub sha: String,
    /// the process that must exit before the finish step may delete `blink.exe`
    pub old_pid: u32,
    /// the legacy install folder and its exe (what finish removes)
    pub legacy_dir: String,
    pub legacy_exe: String,
}

pub fn write_pending(p: &Pending) -> String {
    format!(
        "; Hayate rename in progress (written by Hayate; safe to delete)\nexe={}\ndir={}\nsha={}\nold_pid={}\nlegacy_dir={}\nlegacy_exe={}\n",
        p.exe, p.dir, p.sha, p.old_pid, p.legacy_dir, p.legacy_exe
    )
}

pub fn parse_pending(text: &str) -> Option<Pending> {
    let mut p = Pending::default();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "exe" => p.exe = v,
            "dir" => p.dir = v,
            "sha" => p.sha = v,
            "old_pid" => p.old_pid = v.parse().unwrap_or(0),
            "legacy_dir" => p.legacy_dir = v,
            "legacy_exe" => p.legacy_exe = v,
            _ => {}
        }
    }
    if p.exe.is_empty() || p.dir.is_empty() || p.sha.len() != 64 {
        return None;
    }
    Some(p)
}

/// What a `rename.pending` found at start means.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PendingCheck {
    /// this process is the new exe: finish the move (shortcuts, cleanup, marker)
    Finish,
    /// this process is still the legacy exe: repeat the move (copy + relaunch)
    Restart,
    /// the marker does not describe a usable move: delete it and start over
    Clear(&'static str),
}

/// The install folders a `rename.pending` may name. Anything else in the marker
/// is not this install's business (the data dir is writable by the user).
pub struct Known {
    pub user_dir: String,
    pub machine_dir: String,
    pub legacy_user: String,
    pub legacy_machine: String,
}

/// `current`: this process's exe path; `new_sha`: sha256 of `p.exe` (None =
/// missing); `k`: the only folders the marker may name.
pub fn pending_check(p: &Pending, current: &str, new_sha: Option<&str>, k: &Known) -> PendingCheck {
    // a marker anyone with access to the data dir can edit decides nothing outside
    // the two install layouts and the two exe names
    let names_ours = same_dir(&p.exe, &format!(r"{}\hayate.exe", p.dir))
        && same_dir(&p.legacy_exe, &format!(r"{}\{LEGACY_EXE}", p.legacy_dir))
        && [&k.user_dir, &k.machine_dir].iter().any(|d| !d.is_empty() && same_dir(d, &p.dir))
        && [&k.legacy_user, &k.legacy_machine].iter().any(|d| !d.is_empty() && same_dir(d, &p.legacy_dir));
    if !names_ours {
        return PendingCheck::Clear("the marker does not name this install");
    }
    if !same_dir(current, &p.exe) && norm_path(current) != norm_path(&p.exe) {
        return match new_sha {
            None => PendingCheck::Clear("the new exe is missing"),
            Some(s) if s.eq_ignore_ascii_case(&p.sha) => PendingCheck::Restart,
            Some(_) => PendingCheck::Clear("the new exe is not the build the move copied"),
        };
    }
    match new_sha {
        None => PendingCheck::Clear("the new exe is missing"),
        Some(s) if s.eq_ignore_ascii_case(&p.sha) => PendingCheck::Finish,
        Some(_) => PendingCheck::Clear("the new exe is not the build the move copied"),
    }
}

/// v0.2.1: the one-time removal of the pre-rename leftovers (marker file).
pub const CLEANUP_DONE: &str = "cleanup.done";

/// `rename.done`: version, time, the legacy paths this migration saw, and the
/// registry key it renamed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Done {
    pub version: String,
    pub at: u64,
    pub legacy: Vec<String>,
    pub keys: Vec<String>,
}

pub fn write_done(d: &Done) -> String {
    let mut s = format!("; Hayate rename done (written by Hayate; safe to delete)\nversion={}\nat={}\n", d.version, d.at);
    for l in &d.legacy {
        s.push_str(&format!("legacy={l}\n"));
    }
    for k in &d.keys {
        s.push_str(&format!("key={k}\n"));
    }
    s
}

pub fn parse_done(text: &str) -> Option<Done> {
    let mut d = Done::default();
    let mut any = false;
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "version" => {
                d.version = v.trim().into();
                any = true;
            }
            "at" => d.at = v.trim().parse().unwrap_or(0),
            "legacy" if !v.trim().is_empty() => d.legacy.push(v.trim().into()),
            "key" if !v.trim().is_empty() => d.keys.push(v.trim().into()),
            _ => {}
        }
    }
    any.then_some(d)
}

// ---- install path ----------------------------------------------------------------
/// The new install folder when this exe runs from a pre-rename install folder
/// (`%LOCALAPPDATA%\Programs\Blink` or `%ProgramFiles%\Blink`). `None` for every
/// other path, including a portable copy next to one.
pub fn legacy_install(exe: &str, user_dir: &str, machine_dir: &str, legacy_user: &str, legacy_machine: &str) -> Option<String> {
    let dir = parent(exe);
    let name = file(exe);
    if !(name.eq_ignore_ascii_case(LEGACY_EXE) || name.eq_ignore_ascii_case("hayate.exe")) {
        return None;
    }
    if !legacy_user.is_empty() && same_dir(dir, legacy_user) {
        return Some(user_dir.to_string());
    }
    if !legacy_machine.is_empty() && same_dir(dir, legacy_machine) {
        return Some(machine_dir.to_string());
    }
    None
}

/// The private relaunch arguments of the install-dir move: the original ones,
/// minus a stale wait/finish pair, plus ours.
pub fn handoff_args(args: &[String], old_pid: u32, marker: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--wait-pid" | "--rename-finish" => i += 2,
            _ => {
                out.push(args[i].clone());
                i += 1;
            }
        }
    }
    out.extend(["--wait-pid".to_string(), old_pid.to_string(), "--rename-finish".to_string(), marker.to_string()]);
    out
}

/// `--rename-finish <marker>`: the marker path.
pub fn finish_arg(args: &[String]) -> Option<String> {
    let i = args.iter().position(|a| a == "--rename-finish")?;
    args.get(i + 1).filter(|v| !v.is_empty()).cloned()
}

/// `--update-trial <sha>` of a bridged self-update, when the sha is a sha256.
pub fn trial_sha(args: &[String]) -> Option<String> {
    let i = args.iter().position(|a| a == "--update-trial")?;
    args.get(i + 1).filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit())).cloned()
}

// ---- update.ini -------------------------------------------------------------------

/// Point `install_dir` at the new folder after the move (a generated file, so only
/// that one line is handled). `None` = nothing to change.
pub fn update_ini_moved(text: &str, legacy_dir: &str, new_dir: &str) -> Option<String> {
    let mut changed = false;
    let mut out = String::new();
    for line in text.lines() {
        let t = line.trim_start();
        let hit = t.find('=').map(|eq| (t[..eq].trim(), &t[eq + 1..])).filter(|(k, v)| {
            k.eq_ignore_ascii_case("install_dir") && norm_path(v.split(';').next().unwrap_or("").trim().trim_matches('"')) == norm_path(legacy_dir)
        });
        match hit {
            Some((k, v)) => {
                out.push_str(&format!("{}{k} = {new_dir}", &line[..line.len() - t.len()]));
                if let Some((_, c)) = v.split_once(';') {
                    out.push_str(&format!(" ;{c}"));
                }
                changed = true;
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    changed.then_some(out)
}

// ---- registry ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RegKind {
    Str,
    Dword,
    Binary,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RegValue {
    pub name: String,
    pub kind: RegKind,
    pub data: Vec<u8>,
}

/// The registry seam: `HKCU`/`HKLM` (64-bit view in the real one), values with
/// their type, subkeys, and `RegRenameKey`. The tests use an in-memory map.
pub trait Reg {
    fn key_exists(&self, hive: Hive, key: &str) -> bool;
    fn subkeys(&self, hive: Hive, key: &str) -> Vec<String>;
    fn values(&self, hive: Hive, key: &str) -> Vec<RegValue>;
    fn get_str(&self, hive: Hive, key: &str, name: &str) -> Option<String>;
    fn get_binary(&self, hive: Hive, key: &str, name: &str) -> Option<Vec<u8>>;
    fn set(&mut self, hive: Hive, key: &str, value: &RegValue) -> Result<(), String>;
    fn delete_value(&mut self, hive: Hive, key: &str, name: &str) -> Result<(), String>;
    fn delete_key(&mut self, hive: Hive, key: &str) -> Result<(), String>;
    /// `RegDeleteKeyExW` only (no `RegDeleteTreeW`): refuses a key that has
    /// subkeys, so a failed or partial enumeration can never empty a whole tree.
    fn delete_empty_key(&mut self, hive: Hive, key: &str) -> Result<(), String>;
    /// `RegRenameKey`: false when the platform cannot do it (the caller merges instead).
    fn rename_key(&mut self, hive: Hive, from: &str, to: &str) -> Result<bool, String>;
}

fn set_str(reg: &mut dyn Reg, hive: Hive, key: &str, name: &str, s: &str) -> Result<(), String> {
    let mut data = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect::<Vec<u8>>();
    data.extend([0, 0]);
    reg.set(hive, key, &RegValue { name: name.into(), kind: RegKind::Str, data })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyOutcome {
    /// no legacy key
    Nothing,
    /// `RegRenameKey` moved the whole key
    Renamed,
    /// both existed (or the platform cannot rename): missing values were copied, the legacy key deleted
    Merged,
    /// both exist and the legacy one is not provably ours: touched nothing
    BothKept,
}

/// `SOFTWARE\Blink` -> `SOFTWARE\Hayate`, preserving `TaskManager` and
/// `StartupOwnership`. One atomic rename when the new key is not there; when
/// both exist, only the values the new key lacks are copied, and the legacy key
/// is deleted only when it is provably ours. Failures are returned (retry on
/// the next start); nothing is ever overwritten.
pub fn migrate_key(reg: &mut dyn Reg, hive: Hive, legacy: &str, new: &str, legacy_launcher: &str) -> Result<KeyOutcome, String> {
    if !reg.key_exists(hive, legacy) {
        return Ok(KeyOutcome::Nothing);
    }
    // a key another product owns is never renamed or merged, whatever the target
    // key looks like (the repo documents an unrelated program called Blink)
    if !legacy_key_is_ours(reg, hive, legacy, legacy_launcher) {
        return Ok(KeyOutcome::BothKept);
    }
    if !reg.key_exists(hive, new) {
        if reg.rename_key(hive, legacy, new)? {
            return Ok(KeyOutcome::Renamed);
        }
        copy_missing_tree(reg, hive, legacy, new, 0)?;
        reg.delete_key(hive, legacy)?;
        return Ok(KeyOutcome::Merged);
    }
    copy_missing_tree(reg, hive, legacy, new, 0)?;
    reg.delete_key(hive, legacy)?;
    Ok(KeyOutcome::Merged)
}

/// The legacy HKLM key is ours when our Task Manager state names our legacy
/// launcher, or it holds the `StartupOwnership` key only Hayate ever writes.
pub fn legacy_key_is_ours(reg: &dyn Reg, hive: Hive, legacy: &str, legacy_launcher: &str) -> bool {
    if !legacy_launcher.trim().is_empty() {
        let state = format!(r"{legacy}\TaskManager\taskmgr.exe");
        if reg.get_str(hive, &state, "LauncherPath").is_some_and(|p| norm_path(&p) == norm_path(legacy_launcher)) {
            return true;
        }
    }
    reg.key_exists(hive, &format!(r"{legacy}\StartupOwnership"))
}

fn copy_missing_tree(reg: &mut dyn Reg, hive: Hive, from: &str, to: &str, depth: u8) -> Result<(), String> {
    if depth > 8 {
        return Err(format!("rename: registry tree {from} is too deep"));
    }
    let have: Vec<String> = reg.values(hive, to).into_iter().map(|v| v.name).collect();
    for v in reg.values(hive, from) {
        if !have.iter().any(|n| n.eq_ignore_ascii_case(&v.name)) {
            reg.set(hive, to, &v)?;
        }
    }
    for sub in reg.subkeys(hive, from) {
        let (a, b) = (format!(r"{from}\{sub}"), format!(r"{to}\{sub}"));
        copy_missing_tree(reg, hive, &a, &b, depth + 1)?;
    }
    Ok(())
}

/// `BlinkPath` -> `HayatePath` inside the (already moved) Task Manager state.
/// Only when the new name is missing: with both present nothing is guessed.
pub fn rename_path_value(reg: &mut dyn Reg, hive: Hive, state_key: &str) -> Result<bool, String> {
    if reg.get_str(hive, state_key, "HayatePath").is_some() {
        return Ok(false);
    }
    let Some(old) = reg.get_str(hive, state_key, LEGACY_PATH_VALUE) else { return Ok(false) };
    set_str(reg, hive, state_key, "HayatePath", &old)?;
    reg.delete_value(hive, state_key, LEGACY_PATH_VALUE)?;
    Ok(true)
}

// ---- StartupApproved ------------------------------------------------------------

/// Move the 12-byte `StartupApproved` value `from` -> `to` (the link file
/// names). Write first, then delete: a crash leaves both, which only means
/// Windows sees the old link's approval too.
pub fn move_approved(reg: &mut dyn Reg, hive: Hive, key: &str, from: &str, to: &str) -> Result<bool, String> {
    let Some(bytes) = reg.get_binary(hive, key, from) else { return Ok(false) };
    if bytes.len() != APPROVED_LEN {
        return Err(format!("rename: the StartupApproved value for {from} is {} bytes, expected {APPROVED_LEN}", bytes.len()));
    }
    if reg.get_binary(hive, key, to).is_none() {
        reg.set(hive, key, &RegValue { name: to.into(), kind: RegKind::Binary, data: bytes })?;
    }
    reg.delete_value(hive, key, from)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- data dir copy ----

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("hayate-rename-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn legacy_data_is_only_a_pure_copy() {
        let root = tmp("pure");
        let (old, new) = (root.join("Blink"), root.join("Hayate"));
        std::fs::create_dir_all(old.join("sub")).unwrap();
        std::fs::create_dir_all(new.join("sub")).unwrap();
        std::fs::write(old.join("a.ini"), b"1").unwrap();
        std::fs::write(new.join("a.ini"), b"1").unwrap();
        std::fs::write(old.join("sub/b"), b"2").unwrap();
        std::fs::write(new.join("sub/b"), b"2").unwrap();
        assert_eq!(legacy_data_is_pure_copy(&old, &new), Ok(2));
        std::fs::write(new.join("extra"), b"x").unwrap();
        assert_eq!(legacy_data_is_pure_copy(&old, &new), Ok(2), "new writes after the copy are fine");
        std::fs::write(old.join("a.ini"), b"changed").unwrap();
        assert!(legacy_data_is_pure_copy(&old, &new).is_err(), "a write after the copy blocks the deletion");
        std::fs::write(old.join("a.ini"), b"1").unwrap();
        std::fs::write(old.join("c"), b"3").unwrap();
        assert!(legacy_data_is_pure_copy(&old, &new).is_err(), "a file that was never copied blocks it");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_an_empty_key_can_be_deleted_without_a_tree() {
        let mut r = Fake::default();
        assert_eq!(r.delete_empty_key(Hive::Machine, r"SOFTWARE\Probe"), Ok(()), "a missing key is already gone");
        r.set(Hive::Machine, r"SOFTWARE\Probe", &RegValue { name: "x".into(), kind: RegKind::Str, data: vec![0, 0] }).unwrap();
        assert_eq!(r.delete_empty_key(Hive::Machine, r"SOFTWARE\Probe"), Ok(()));
        r.set(Hive::Machine, r"SOFTWARE\Probe\sub", &RegValue { name: "x".into(), kind: RegKind::Str, data: vec![0, 0] }).unwrap();
        assert!(r.delete_empty_key(Hive::Machine, r"SOFTWARE\Probe").is_err(), "a key with subkeys is refused");
        assert!(r.key_exists(Hive::Machine, r"SOFTWARE\Probe\sub"), "and nothing was removed");
    }

    #[test]
    fn data_copy_copies_everything_when_missing() {
        let root = tmp("copy-all");
        let (src, dst) = (root.join("Blink"), root.join("Hayate"));
        std::fs::create_dir_all(src.join("nilesoft")).unwrap();
        std::fs::write(src.join("rules.ini"), b"a=1").unwrap();
        std::fs::write(src.join("nilesoft/x.nss"), b"nss").unwrap();
        let r = copy_missing(&src, &dst).unwrap();
        assert_eq!(r.files, 2, "{r:?}");
        assert_eq!(std::fs::read(dst.join("rules.ini")).unwrap(), b"a=1");
        assert_eq!(std::fs::read(dst.join("nilesoft/x.nss")).unwrap(), b"nss");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn data_copy_never_overwrites_and_is_idempotent() {
        let root = tmp("copy-missing");
        let (src, dst) = (root.join("Blink"), root.join("Hayate"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("a.ini"), b"old").unwrap();
        std::fs::write(dst.join("a.ini"), b"new").unwrap();
        std::fs::write(src.join("b.ini"), b"old").unwrap();
        let r = copy_missing(&src, &dst).unwrap();
        assert_eq!(r.files, 1, "{r:?}");
        assert_eq!(std::fs::read(dst.join("a.ini")).unwrap(), b"new", "Hayate's own file wins");
        assert_eq!(std::fs::read(dst.join("b.ini")).unwrap(), b"old");
        let r = copy_missing(&src, &dst).unwrap();
        assert_eq!(r.files, 0, "second run copies nothing: {r:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn data_copy_multi_uses_the_multi_dirs() {
        let la = r"C:\Users\u\AppData\Local";
        let (a, b) = data_dir_pair(la, true);
        assert!(a.ends_with(r"Local\Blink-multi"));
        assert!(b.ends_with(r"Local\Hayate-multi"));
        let (a, b) = data_dir_pair(la, false);
        assert!(a.ends_with(r"Local\Blink"));
        assert!(b.ends_with(r"Local\Hayate"));
    }

    #[test]
    fn data_copy_missing_source_is_a_no_op() {        let root = tmp("copy-none");
        let r = copy_missing(&root.join("nope"), &root.join("new")).unwrap();
        assert_eq!(r, CopyReport::default());
        assert!(!root.join("new").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- install path ----

    #[test]
    fn legacy_install_is_recognised_only_for_the_two_known_folders() {
        let ud = r"C:\Users\u\AppData\Local\Programs\Hayate";
        let md = r"C:\Program Files\Hayate";
        let lu = r"C:\Users\u\AppData\Local\Programs\Blink";
        let lm = r"C:\Program Files\Blink";
        assert_eq!(legacy_install(r"C:\Users\u\AppData\Local\Programs\Blink\blink.exe", ud, md, lu, lm).as_deref(), Some(ud));
        assert_eq!(legacy_install(r"C:\Program Files\Blink\blink.exe", ud, md, lu, lm).as_deref(), Some(md));
        assert_eq!(legacy_install(r"C:\Users\u\AppData\Local\Programs\Blink\hayate.exe", ud, md, lu, lm).as_deref(), Some(ud));
        assert_eq!(legacy_install(r"C:\Users\u\Downloads\blink.exe", ud, md, lu, lm), None);
        assert_eq!(legacy_install(r"C:\Users\u\AppData\Local\Programs\Blink\blink-tm.exe", ud, md, lu, lm), None);
    }

    #[test]
    fn handoff_args_keep_the_update_arguments_once() {
        let a = vec!["--update-trial".to_string(), "a".repeat(64), "--wait-pid".into(), "7".into(), "--resident".into()];
        let out = handoff_args(&a, 42, r"C:\D\rename.pending");
        assert_eq!(out[0], "--update-trial");
        assert!(out.contains(&"--resident".to_string()));
        assert_eq!(out.iter().filter(|x| *x == "--wait-pid").count(), 1);
        assert_eq!(out.last().unwrap(), r"C:\D\rename.pending");
        assert!(out.windows(2).any(|w| w[0] == "--wait-pid" && w[1] == "42"));
    }

    #[test]
    fn update_ini_is_pointed_at_the_new_folder() {
        let t = "; c\n[update]\nenabled = true\nsource = X:\\d\ninstall_dir = \"C:\\U\\Programs\\Blink\" ; note\n";
        let out = update_ini_moved(t, r"C:\U\Programs\Blink", r"C:\U\Programs\Hayate").unwrap();
        assert!(out.contains(r"install_dir = C:\U\Programs\Hayate ; note"), "{out}");
        assert!(out.contains("enabled = true") && out.contains("source = X:\\d"), "{out}");
        assert_eq!(update_ini_moved(&out, r"C:\U\Programs\Blink", r"X:\y"), None, "already moved: nothing to do");
        assert_eq!(update_ini_moved("[update]\ninstall_dir = C:\\Other\n", r"C:\U\Programs\Blink", r"X:\y"), None, "someone else's folder is never rewritten");
    }

    // ---- markers ----

    fn pending() -> Pending {
        Pending {
            exe: r"C:\U\Programs\Hayate\hayate.exe".into(),
            dir: r"C:\U\Programs\Hayate".into(),
            sha: "a".repeat(64),
            old_pid: 7,
            legacy_dir: r"C:\U\Programs\Blink".into(),
            legacy_exe: r"C:\U\Programs\Blink\blink.exe".into(),
        }
    }

    #[test]
    fn pending_marker_round_trips_and_needs_all_fields() {        let p = pending();
        assert_eq!(parse_pending(&write_pending(&p)), Some(p));
        assert_eq!(parse_pending("exe=C:\\x\\hayate.exe\n"), None);
        assert_eq!(parse_pending(""), None);
    }

    #[test]
    fn pending_check_finishes_restarts_or_clears() {
        let p = pending();
        let k = Known { user_dir: r"C:\U\Programs\Hayate".into(), machine_dir: r"M:\Hayate".into(), legacy_user: r"C:\U\Programs\Blink".into(), legacy_machine: r"M:\Blink".into() };
        assert_eq!(pending_check(&p, &p.exe, Some(&p.sha), &k), PendingCheck::Finish);
        assert_eq!(pending_check(&p, &p.legacy_exe, Some(&p.sha), &k), PendingCheck::Restart);
        assert_eq!(pending_check(&p, &p.legacy_exe, None, &k), PendingCheck::Clear("the new exe is missing"));
        assert_eq!(pending_check(&p, &p.exe, Some(&"b".repeat(64)), &k), PendingCheck::Clear("the new exe is not the build the move copied"));
        // a marker anyone with access to the data dir can edit names only these folders
        let mut evil = p.clone();
        evil.legacy_dir = r"C:\Windows\Temp".into();
        evil.legacy_exe = r"C:\Windows\Temp\blink.exe".into();
        assert_eq!(pending_check(&evil, &evil.exe, Some(&evil.sha), &k), PendingCheck::Clear("the marker does not name this install"));
        let mut evil = p.clone();
        evil.dir = r"C:\Windows".into();
        evil.exe = r"C:\Windows\hayate.exe".into();
        assert_eq!(pending_check(&evil, &evil.exe, Some(&evil.sha), &k), PendingCheck::Clear("the marker does not name this install"));
    }

    #[test]
    fn done_marker_round_trips() {
        let d = Done { version: "0.2.0".into(), at: 12, legacy: vec![r"C:\U\Programs\Blink".into(), r"C:\U\AppData\Local\Blink".into()], keys: vec![r"SOFTWARE\Blink".into()] };
        assert_eq!(parse_done(&write_done(&d)), Some(d));
        assert_eq!(parse_done("; nothing\n"), None);
    }

    // ---- registry ----

    #[derive(Default)]
    struct Fake {
        keys: std::collections::BTreeMap<String, Vec<RegValue>>,
        subkeys: std::collections::BTreeMap<String, Vec<String>>,
        fail: Option<&'static str>,
    }

    fn hk(h: Hive) -> &'static str {
        if h == Hive::User {
            "HKCU"
        } else {
            "HKLM"
        }
    }

    impl Fake {
        fn key(&self, h: Hive, k: &str) -> String {
            format!(r"{}\{}", hk(h), k).to_lowercase()
        }
        fn put(&mut self, h: Hive, k: &str, v: RegValue) {
            let k = self.key(h, k);
            self.keys.entry(k.clone()).or_default().push(v);
            self.ensure_parents(h, &k);
        }
        fn ensure_parents(&mut self, h: Hive, k: &str) {
            let mut cur = k.to_string();
            while let Some((p, leaf)) = cur.rsplit_once('\\') {
                let subs = self.subkeys.entry(p.to_string()).or_default();
                if !subs.contains(&leaf.to_string()) {
                    subs.push(leaf.to_string());
                }
                cur = p.to_string();
                if cur == hk(h) {
                    break;
                }
            }
        }
        fn put_str(&mut self, h: Hive, k: &str, n: &str, s: &str) {
            let mut data = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect::<Vec<u8>>();
            data.extend([0, 0]);
            self.put(h, k, RegValue { name: n.into(), kind: RegKind::Str, data });
        }
    }

    impl Reg for Fake {
        fn key_exists(&self, h: Hive, key: &str) -> bool {
            self.keys.contains_key(&self.key(h, key))
        }
        fn subkeys(&self, h: Hive, key: &str) -> Vec<String> {
            self.subkeys.get(&self.key(h, key)).cloned().unwrap_or_default()
        }
        fn values(&self, h: Hive, key: &str) -> Vec<RegValue> {
            self.keys.get(&self.key(h, key)).cloned().unwrap_or_default()
        }
        fn get_str(&self, h: Hive, key: &str, name: &str) -> Option<String> {
            let v = self.values(h, key).into_iter().find(|v| v.name == name)?;
            let units: Vec<u16> = v.data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            Some(crate::osutil::from_wide(&units))
        }
        fn get_binary(&self, h: Hive, key: &str, name: &str) -> Option<Vec<u8>> {
            self.values(h, key).into_iter().find(|v| v.name == name).map(|v| v.data)
        }
        fn set(&mut self, h: Hive, key: &str, value: &RegValue) -> Result<(), String> {
            if self.fail == Some("set") {
                return Err("access denied".into());
            }
            let k = self.key(h, key);
            let vals = self.keys.entry(k.clone()).or_default();
            vals.retain(|v| !v.name.eq_ignore_ascii_case(&value.name));
            vals.push(value.clone());
            self.ensure_parents(h, &k);
            Ok(())
        }
        fn delete_value(&mut self, h: Hive, key: &str, name: &str) -> Result<(), String> {
            if self.fail == Some("delete") {
                return Err("access denied".into());
            }
            if let Some(v) = self.keys.get_mut(&self.key(h, key)) {
                v.retain(|v| !v.name.eq_ignore_ascii_case(name));
            }
            Ok(())
        }
        fn delete_empty_key(&mut self, h: Hive, key: &str) -> Result<(), String> {
            let k = self.key(h, key);
            let has_sub = self.subkeys.get(&k).is_some_and(|v| !v.is_empty()) || self.keys.keys().any(|x| x.starts_with(&format!("{k}\\")));
            if has_sub {
                return Err(format!("{key} is not empty; kept"));
            }
            self.keys.remove(&k);
            self.subkeys.remove(&k);
            Ok(())
        }
        fn delete_key(&mut self, h: Hive, key: &str) -> Result<(), String> {
            let k = self.key(h, key);
            self.keys.retain(|x, _| x != &k && !x.starts_with(&format!("{k}\\")));
            self.subkeys.retain(|x, _| x != &k && !x.starts_with(&format!("{k}\\")));
            Ok(())
        }
        fn rename_key(&mut self, h: Hive, from: &str, to: &str) -> Result<bool, String> {
            let (a, b) = (self.key(h, from), self.key(h, to));
            if !self.keys.contains_key(&a) {
                return Ok(false);
            }
            let moved: Vec<_> = self.keys.iter().filter(|(k, _)| *k == &a || k.starts_with(&format!("{a}\\"))).map(|(k, v)| (k.clone(), v.clone())).collect();
            for (k, v) in moved {
                let nk = format!("{b}{}", &k[a.len()..]);
                self.keys.insert(nk, v);
                self.keys.remove(&k);
            }
            Ok(true)
        }
    }

    fn legacy_key(v: &mut Fake, ours: bool) {
        v.put_str(Hive::Machine, LEGACY_KEY, "x", "1");
        v.put_str(Hive::Machine, &format!(r"{LEGACY_KEY}\TaskManager\taskmgr.exe"), "Enabled", "1");
        if ours {
            v.put_str(Hive::Machine, &format!(r"{LEGACY_KEY}\TaskManager\taskmgr.exe"), "LauncherPath", r"C:\Program Files\Blink\blink-tm.exe");
        }
        v.put_str(Hive::Machine, &format!(r"{LEGACY_KEY}\TaskManager\taskmgr.exe"), "HayatePath", r"C:\Program Files\Blink\blink.exe");
    }

    #[test]
    fn registry_rename_moves_the_whole_key() {
        let mut f = Fake::default();
        legacy_key(&mut f, true);
        let o = migrate_key(&mut f, Hive::Machine, LEGACY_KEY, r"SOFTWARE\Hayate", r"C:\Program Files\Blink\blink-tm.exe").unwrap();
        assert_eq!(o, KeyOutcome::Renamed);
        assert!(!f.key_exists(Hive::Machine, LEGACY_KEY));
        assert_eq!(f.get_str(Hive::Machine, &format!(r"SOFTWARE\Hayate\TaskManager\taskmgr.exe"), "Enabled").as_deref(), Some("1"), "TaskManager came along");
    }

    #[test]
    fn registry_merge_copies_only_missing_values_and_keeps_a_foreign_key() {
        let mut f = Fake::default();
        legacy_key(&mut f, false); // not provably ours
        f.put_str(Hive::Machine, r"SOFTWARE\Hayate", "Version", "new");
        assert_eq!(migrate_key(&mut f, Hive::Machine, LEGACY_KEY, r"SOFTWARE\Hayate", r"C:\Program Files\Blink\blink-tm.exe").unwrap(), KeyOutcome::BothKept);
        assert!(f.key_exists(Hive::Machine, LEGACY_KEY), "foreign key never deleted");
        assert_eq!(f.get_str(Hive::Machine, r"SOFTWARE\Hayate", "Version").as_deref(), Some("new"));

        let mut f = Fake::default();
        legacy_key(&mut f, true);
        f.put_str(Hive::Machine, r"SOFTWARE\Hayate", "Version", "new");
        assert_eq!(migrate_key(&mut f, Hive::Machine, LEGACY_KEY, r"SOFTWARE\Hayate", r"C:\Program Files\Blink\blink-tm.exe").unwrap(), KeyOutcome::Merged);
        assert!(!f.key_exists(Hive::Machine, LEGACY_KEY));
        assert_eq!(f.get_str(Hive::Machine, r"SOFTWARE\Hayate", "Version").as_deref(), Some("new"), "never overwritten");
        assert_eq!(f.get_str(Hive::Machine, r"SOFTWARE\Hayate\TaskManager\taskmgr.exe", "Enabled").as_deref(), Some("1"), "missing subkey copied");
    }

    #[test]
    fn blob_path_value_is_renamed_not_copied_twice() {
        let mut f = Fake::default();
        let st = r"SOFTWARE\Hayate\TaskManager\taskmgr.exe";
        f.put_str(Hive::Machine, st, "BlinkPath", r"C:\Program Files\Blink\blink.exe");
        assert!(rename_path_value(&mut f, Hive::Machine, st).unwrap());
        assert_eq!(f.get_str(Hive::Machine, st, "HayatePath").as_deref(), Some(r"C:\Program Files\Blink\blink.exe"));
        assert_eq!(f.get_str(Hive::Machine, st, "BlinkPath"), None);
        assert!(!rename_path_value(&mut f, Hive::Machine, st).unwrap(), "second run does nothing");
    }

    #[test]
    fn startup_approved_bytes_move_after_the_write() {
        let mut f = Fake::default();
        f.put(Hive::User, APPROVED_KEY, RegValue { name: "Blink.lnk".into(), kind: RegKind::Binary, data: vec![2, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8] });
        assert!(move_approved(&mut f, Hive::User, APPROVED_KEY, "Blink.lnk", "Hayate.lnk").unwrap());
        assert_eq!(f.get_binary(Hive::User, APPROVED_KEY, "Hayate.lnk").unwrap().len(), 12);
        assert_eq!(f.get_binary(Hive::User, APPROVED_KEY, "Blink.lnk"), None);
        assert!(!move_approved(&mut f, Hive::User, APPROVED_KEY, "Blink.lnk", "Hayate.lnk").unwrap(), "idempotent");
    }
}
