//! Guard for the Blink -> Hayate rename (v0.2.0): every remaining
//! `Blink`/`blink` in a source *string literal* must be listed here with a
//! reason. Comments and identifiers are not scanned: what matters is what can
//! reach a file, registry key, window class, env var or the user.
//!
//! The scan reads `src/**/*.rs` at test time, so a newly typed literal fails
//! this test until it is renamed or allowlisted on purpose.

use std::path::Path;

/// (file, exact literal content, why it must stay).
///
/// Matching is exact (not a prefix): a new `"Blink ..."` string in one of these
/// files does not pass by accident.
const ALLOWED: &[(&str, &str, &str)] = &[
    // The legacy env names are read when HAYATE_<suffix> is unset (rig tooling,
    // older docs and scripts still set BLINK_MULTI / BLINK_DATA_DIR).
    ("src/osutil.rs", "BLINK_", "legacy env var fallback"),
    ("src/osutil.rs", "BLINK_RENAME_GUARD_PROBE", "legacy env var fallback test probe"),
    // Uninstall sweeps the pre-rename Installed-apps key. It is ours only when
    // DisplayName is Blink/Hayate and the UninstallString is "<dir>\blink.exe".
    ("src/setup/logic.rs", "Blink", "legacy ARP key DisplayName"),
    (
        "src/setup/logic.rs",
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Blink",
        "legacy ARP key sweep (raw string)",
    ),
    ("src/setup/logic.rs", r#"\"{dir}\\blink.exe\""#, "legacy ARP UninstallString"),
    ("src/setup/logic.rs", r#"{dir}\\blink.exe"#, "legacy ARP UninstallString (test)"),
    (
        "src/setup/logic.rs",
        r#""C:\Program Files\Hayate\blink.exe" --uninstall"#,
        "legacy ARP UninstallString (test)",
    ),
    ("src/setup/logic.rs", r"C:\Users\a\AppData\Local\Blink-Alpha", "foreign ARP key fixture"),
    ("src/setup/logic.rs", r"C:\Users\a\AppData\Local\Blink", "legacy data folder fixture"),
    ("src/setup/logic.rs", r"C:\Users\a\AppData\Local\Blink\sub", "legacy data folder fixture (not removable)"),
    ("src/setup/logic.rs", r"C:\Users\a\AppData\Local\Blink\Programs", "legacy data folder fixture (install inside)"),
    ("src/setup/logic.rs", "Blink Alpha", "foreign ARP key fixture"),
    // Start-menu names before the rename: swept on install/uninstall and
    // migrated to the Hayate names by `menu_migration`.
    ("src/setup/logic.rs", "Blink.lnk", "legacy Start-menu link"),
    ("src/setup/logic.rs", "Blink (admin).lnk", "legacy Start-menu link"),
    ("src/setup/logic.rs", "Blink (no admin).lnk", "legacy Start-menu link"),
    // The guard's own assertion text.
    ("src/setup/logic.rs", "blink", "the user-visible-name assertion"),
    // Startup list / claim file headers written by 0.1.0: still parsed as data.
    ("src/startup/model.rs", "blink-startup 1", "legacy startup list header"),
    ("src/startup/model.rs", "blink-startup ", "legacy startup list header (newer-version check)"),
    ("src/startup/run.rs", "blink-startup-claim 2", "legacy logon claim header"),

    // ---- Stage B: the one-time migration (src/rename) ----

    ("src/rename/logic.rs", "Blink", "legacy data folder name"),
    ("src/rename/logic.rs", "Blink-multi", "legacy multi data folder name"),
    ("src/rename/logic.rs", "blink.exe", "legacy executable name"),
    ("src/rename/logic.rs", "blink-tm.exe", "legacy Task Manager launcher name"),
    ("src/rename/logic.rs", "Blink.lnk", "legacy Startup link name"),
    ("src/rename/logic.rs", "SOFTWARE\\Blink", "legacy HKLM key that is renamed"),
    ("src/rename/logic.rs", "BlinkPath", "legacy Task Manager state value that is renamed"),
    ("src/rename/logic.rs", "Local\\Blink-multi", "legacy multi data path (test)"),
    ("src/rename/logic.rs", "Local\\Blink", "legacy data path (test)"),
    ("src/rename/logic.rs", "C:\\Users\\u\\AppData\\Local\\Programs\\Blink", "legacy per-user install folder (test)"),
    ("src/rename/logic.rs", "C:\\Program Files\\Blink", "legacy all-users install folder (test)"),
    ("src/rename/logic.rs", "C:\\Users\\u\\AppData\\Local\\Programs\\Blink\\blink.exe", "legacy install path (test)"),
    ("src/rename/logic.rs", "C:\\Program Files\\Blink\\blink.exe", "legacy install path (test)"),
    ("src/rename/logic.rs", "C:\\Users\\u\\AppData\\Local\\Programs\\Blink\\hayate.exe", "new name in the legacy folder (test)"),
    ("src/rename/logic.rs", "C:\\Users\\u\\Downloads\\blink.exe", "portable legacy copy (test)"),
    ("src/rename/logic.rs", "C:\\Users\\u\\AppData\\Local\\Programs\\Blink\\blink-tm.exe", "legacy launcher, not an install exe (test)"),
    ("src/rename/logic.rs", "C:\\U\\Programs\\Blink", "legacy install folder fixture"),
    ("src/rename/logic.rs", r#"; c\n[update]\nenabled = true\nsource = X:\\d\ninstall_dir = \"C:\\U\\Programs\\Blink\" ; note\n"#, "update.ini rewrite fixture"),
    ("src/rename/logic.rs", r"M:\Blink", "pending-marker layout fixture"),
    ("src/rename/logic.rs", r"C:\Windows\Temp\blink.exe", "pending-marker validation fixture"),
    ("src/rename/logic.rs", "C:\\U\\Programs\\Blink\\blink.exe", "legacy install exe fixture"),
    ("src/rename/logic.rs", "C:\\U\\AppData\\Local\\Blink", "legacy data folder fixture"),
    ("src/rename/logic.rs", "C:\\Program Files\\Blink\\blink-tm.exe", "legacy launcher fixture / ownership check"),
    ("src/rename/logic.rs", "C:\\Program Files\\Blink\\blink.exe", "legacy exe fixture in the ownership test"),
    ("src/rename/mod.rs", "blink.new.exe", "legacy updater leftover removed with the old folder"),
    ("src/rename/mod.rs", "blink.old.exe", "legacy updater leftover removed with the old folder"),
    ("src/rename/mod.rs", "blink.bad.exe", "legacy updater leftover removed with the old folder"),
    ("src/rename/mod.rs", "rename: HKLM\\\\SOFTWARE\\\\Blink needs an elevated start; retries", "migration log line"),
    ("src/rename/mod.rs", "rename: HKLM\\\\SOFTWARE\\\\Blink -> Hayate: {o:?}", "migration log line"),
    ("src/rename/mod.rs", "rename: {}\\\\{target}: BlinkPath -> HayatePath", "migration log line"),
    ("src/rename/mod.rs", "rename: {}\\\\{target}: BlinkPath not renamed: {e}", "migration log line"),
    ("src/rename/mod.rs", "rename: the Task Manager redirect still uses blink-tm.exe; needs an elevated start", "migration log line"),
    ("src/rename/mod.rs", r"HKLM\\SOFTWARE\\Blink needs an elevated start", "cleanup retry note"),    ("src/rename/mod.rs", "C:\\Program Files\\Blink\\blink-tm.exe", "real HKCU registry test fixture"),
    ("src/rename/mod.rs", "C:\\Program Files\\Blink\\blink.exe", "real HKCU registry test fixture"),
    ("src/rename/mod.rs", "BlinkPath", "the value renamed by the real HKCU test"),
    ("src/rename/mod.rs", "blink.exe", "the Startup link test's legacy target"),
    ("src/rename/mod.rs", "Blink.lnk", "the Startup link test's legacy link name"),

    // ---- Stage B: the other generation is recognised by setup/update/taskmgr ----

    ("src/netetw.rs", "Blink-Kernel-Network-", "legacy ETW session prefix for the dead-owner sweep"),
    ("src/setup/logic.rs", "blink.exe", "legacy exe accepted by --uninstall"),
    ("src/setup/logic.rs", "C:\\Users\\a\\AppData\\Local\\Programs\\Blink", "legacy per-user install folder (test)"),
    ("src/setup/logic.rs", "C:\\Program Files\\Blink", "legacy all-users install folder (test)"),
    ("src/setup/logic.rs", r"{lu}\blink.exe", "legacy per-user uninstall (test)"),
    ("src/setup/logic.rs", r"{lm}\blink.exe", "legacy all-users uninstall (test)"),
    ("src/setup/logic.rs", "C:\\Users\\a\\Downloads\\blink.exe", "portable legacy copy refused (test)"),
    ("src/setup/logic.rs", "C:\\Program Files\\Blink\\blink-tm.exe", "legacy launcher is not an install exe (test)"),
    ("src/setup/mod.rs", "blink.exe", "legacy install exe exists (status/installed)"),
    ("src/setup/mod.rs", "blink-tm.old.exe", "pre-rename launcher leftover removed on uninstall"),
    ("src/setup/mod.rs", "blink-tm.new.exe", "pre-rename launcher leftover removed on uninstall"),
    ("src/setup/mod.rs", "Only the installed hayate.exe (or the pre-rename blink.exe) can uninstall Hayate", "uninstall refusal text"),
    ("src/taskmgr/logic.rs", "blink-tm.exe", "legacy launcher name"),
    ("src/taskmgr/logic.rs", "{pf}\\Blink\\{LEGACY_LAUNCHER_NAME}", "legacy launcher path"),
    ("src/taskmgr/logic.rs", "\\blink\\", "a value naming the legacy folder is never guessed at"),
    ("src/taskmgr/logic.rs", "C:\\Program Files\\Blink\\blink-tm.exe", "legacy redirect fixture"),
    ("src/taskmgr/logic.rs", "C:\\Program Files\\Blink\\blink.exe", "legacy redirect fixture"),
    ("src/taskmgr/logic.rs", "\"C:\\Program Files\\Blink\\blink-tm.exe\" \"C:\\other.exe\"", "inexact legacy value must be refused"),
    ("src/update/logic.rs", "blink.exe", "legacy layout exe / source name"),
    ("src/update/logic.rs", "Blink", "legacy install folder name (update.ini must match its leaf)"),
    ("src/update/logic.rs", "blink.new.exe", "legacy layout staged name"),
    ("src/update/logic.rs", "blink.old.exe", "legacy layout previous name"),
    ("src/update/logic.rs", "blink.bad.exe", "legacy layout rolled-back name"),
    ("src/update/logic.rs", "blink.new.version.txt", "legacy layout staged version file"),
    ("src/update/logic.rs", "blink.exe.sha256", "legacy layout source hash"),
    ("src/update/logic.rs", "{la}\\\\Programs\\\\Blink", "legacy per-user install folder"),
    ("src/update/logic.rs", "{pf}\\\\Blink", "legacy all-users install folder"),
    ("src/update/logic.rs", "C:\\Users\\u\\AppData\\Local\\Programs\\Blink", "legacy install folder (test)"),
    ("src/update/logic.rs", "C:\\Users\\u\\AppData\\Local\\Programs\\Blink\\blink.exe", "legacy layout (test)"),
    ("src/update/logic.rs", "C:\\Program Files\\Blink\\blink.exe", "legacy layout (test)"),
    ("src/update/logic.rs", "C:\\Program Files\\Blink", "legacy layout (test)"),
    ("src/update/logic.rs", "C:\\Users\\u\\Downloads\\blink.exe", "portable legacy copy refused (test)"),
    ("src/update/logic.rs", "C:\\Users\\u\\AppData\\Local\\Programs\\Hayate\\blink.exe", "old name in the new folder refused (test)"),
    ("src/update/logic.rs", "C:\\Users\\u\\AppData\\Local\\Programs\\Blink\\hayate.exe", "new name in the old folder refused (test)"),
    ("src/update/logic.rs", "a new install never takes blink.exe", "assertion text"),
    ("src/update/logic.rs", "BLINK.EXE", "the name gate is case-insensitive (test)"),
    ("src/update/logic.rs", "blink-tm.exe", "the launcher is never an update exe (test)"),
    ("src/update/mod.rs", "blink.exe", "the layout has no usable source (log)"),

    // Grouping: while an old install is still around, its image is still us
    // (and a lookalike must not be mistaken for us).
    ("src/ui/procs.rs", "blink.exe", "grouping test: pre-rename image is still ours"),
    ("src/ui/procs.rs", "blink", "grouping: the pre-rename image family is still us"),
    ("src/ui/procs.rs", "blink-tm.exe", "grouping test: pre-rename launcher is still ours"),
    ("src/ui/procs.rs", "blink.old.exe", "grouping test: pre-rename staging copy is still ours"),
    ("src/ui/procs.rs", "blink-77.exe", "grouping test: pre-rename parked copy is still ours"),
    ("src/ui/procs.rs", "blinkfeed.exe", "grouping test: a lookalike name is not ours"),
];

/// String literals of one source file whose text contains `blink` (any case),
/// with the 1-based line each starts on. Comments are skipped.
fn blink_literals(src: &str) -> Vec<(usize, String)> {
    let b: Vec<char> = src.chars().collect();
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut line = 1usize;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i < b.len() && !(b[i] == '*' && b.get(i + 1) == Some(&'/')) {
                if b[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            i += 2;
            continue;
        }
        // raw string: r"..." / r#"..."#, but not an identifier ending in r
        if c == 'r' && !(i > 0 && (b[i - 1].is_alphanumeric() || b[i - 1] == '_')) {
            let mut j = i + 1;
            let mut hashes = 0;
            while b.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if b.get(j) == Some(&'"') {
                let start_line = line;
                let start = j + 1;
                let mut k = start;
                let mut end = None;
                while k < b.len() {
                    if b[k] == '\n' {
                        line += 1;
                    }
                    if b[k] == '"' && (0..hashes).all(|h| b.get(k + 1 + h) == Some(&'#')) {
                        end = Some(k);
                        break;
                    }
                    k += 1;
                }
                if let Some(end) = end {
                    out.push((start_line, b[start..end].iter().collect()));
                    i = end + 1 + hashes;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        if c == '"' {
            let start_line = line;
            let start = i + 1;
            let mut j = start;
            while j < b.len() {
                if b[j] == '\\' {
                    j += 2;
                    continue;
                }
                if b[j] == '"' || b[j] == '\n' {
                    break;
                }
                j += 1;
            }
            out.push((start_line, b[start..j].iter().collect()));
            i = j + 1;
            continue;
        }
        if c == '\'' {
            // char literal: 'x' or '\n'; a lifetime ('a) is not a string
            if b.get(i + 1) == Some(&'\\') {
                j_skip_escape(&b, &mut i);
            } else if b.get(i + 2) == Some(&'\'') {
                i += 3;
            } else {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out.retain(|(_, s)| s.to_ascii_lowercase().contains("blink"));
    out
}

fn j_skip_escape(b: &[char], i: &mut usize) {
    *i += 2;
    while *i < b.len() && b[*i] != '\'' {
        *i += 1;
    }
    *i += 1;
}

fn allowed(file: &str, lit: &str) -> Option<&'static str> {
    ALLOWED.iter().find(|(f, l, _)| *f == file && *l == lit).map(|(_, _, why)| *why)
}

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display())).flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn identity_strings_are_renamed() {
    assert_eq!(crate::osutil::APP_NAME, "Hayate");
    assert_eq!(crate::notify::AUMID, "Hayate.TaskManager");
    assert_eq!(crate::etw::SESSION, "Hayate-Kernel-Process");
    assert_eq!(crate::setup::logic::LINK_AUTO, "Hayate.lnk");
    assert_eq!(crate::setup::logic::ARP_KEY.rsplit('\\').next(), Some("Hayate"));
    assert_eq!(crate::setup::logic::PARK_DIR, "Hayate-uninstalled");
    assert_eq!(crate::taskmgr::logic::LAUNCHER_NAME, "hayate-tm.exe");
    assert_eq!(crate::taskmgr::logic::STATE_KEY, r"SOFTWARE\Hayate\TaskManager");
    assert_eq!(crate::taskmgr::logic::SELF_TEST_ARG, "--hayate-self-test");
}

#[test]
fn no_source_literal_still_names_the_old_product() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    assert!(files.len() > 40, "the scan found no sources: {:?}", files);
    let mut bad = Vec::new();
    let mut seen: std::collections::BTreeSet<(String, String)> = Default::default();
    for f in &files {
        let rel = f.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        if rel == "src/rename_guard.rs" {
            continue; // this file is the allowlist itself
        }
        let src = std::fs::read_to_string(f).unwrap_or_else(|e| panic!("cannot read {}: {e}", f.display()));
        for (line, lit) in blink_literals(&src) {
            match allowed(&rel, &lit) {
                Some(_) => {
                    seen.insert((rel.clone(), lit));
                }
                None => bad.push(format!("{rel}:{line}: \"{lit}\"")),
            }
        }
    }
    assert!(bad.is_empty(), "rename these or allowlist them with a reason:\n{}", bad.join("\n"));
    // a stale entry would silently widen the allowlist: every one must still be used
    let unused: Vec<_> = ALLOWED
        .iter()
        .filter(|(f, l, _)| !seen.contains(&(f.to_string(), l.to_string())))
        .map(|(f, l, _)| format!("{f}: \"{l}\""))
        .collect();
    assert!(unused.is_empty(), "allowlist entries no longer in the source:\n{}", unused.join("\n"));
}
