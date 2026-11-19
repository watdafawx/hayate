//! Import a Launchpad `config.toml`: read the file the user names, show exactly
//! what would be added, and add only what they select.
//!
//! * Read-only: the file is read once; nothing of Launchpad's is written,
//!   launched, disabled or "fixed". Hayate never looks for the file by itself.
//! * Preview first: every entry with its problems, name clashes and what its
//!   original startup entry looks like right now. Nothing is selected by default.
//! * Importing changes no Windows startup entry. Entries are added switched
//!   OFF, so nothing starts twice by accident: Launchpad itself most likely
//!   still starts the same programs, and only the user can decide which of the
//!   two managers keeps doing it.
//! * Limits: the file is untrusted text (size, entry count, field bounds and the
//!   same validation as a hand-edited Hayate list).

use std::collections::HashMap;
use std::path::Path;

use super::approved::{self, Locations};
use super::id::{EntryId, Kind, Scope, View};
use super::model::{field_problem, App, MAX_APPS, MAX_FILE_BYTES};
use super::store::Store;

// ---- the TOML subset Launchpad writes --------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Str(String),
    Bool(bool),
    Num(f64),
    /// an array, inline table, multi-line string, date...: not used by Launchpad
    Other,
}

type Table = HashMap<String, Val>;

fn parse_string(v: &str) -> Option<(String, &str)> {
    let mut chars = v.char_indices();
    let (_, q) = chars.next()?;
    if q == '\'' {
        let end = v[1..].find('\'')? + 1;
        return Some((v[1..end].to_string(), &v[end + 1..]));
    }
    if q != '"' {
        return None;
    }
    let mut out = String::new();
    let mut esc = false;
    let mut it = v[1..].char_indices();
    while let Some((i, c)) = it.next() {
        if esc {
            esc = false;
            match c {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'u' | 'U' => {
                    let n = if c == 'u' { 4 } else { 8 };
                    let hex: String = (0..n).filter_map(|_| it.next().map(|(_, h)| h)).collect();
                    out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)?);
                }
                _ => return None,
            }
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            return Some((out, &v[1 + i + 1..]));
        } else {
            out.push(c);
        }
    }
    None
}

fn parse_value(v: &str) -> Val {
    let v = v.trim();
    if v.starts_with("\"\"\"") || v.starts_with("'''") {
        return Val::Other;
    }
    if v.starts_with('"') || v.starts_with('\'') {
        return match parse_string(v) {
            Some((s, rest)) if rest.trim().is_empty() || rest.trim_start().starts_with('#') => Val::Str(s),
            _ => Val::Other,
        };
    }
    let bare = v.split('#').next().unwrap_or("").trim();
    match bare {
        "true" => Val::Bool(true),
        "false" => Val::Bool(false),
        n => n.replace('_', "").parse::<f64>().map(Val::Num).unwrap_or(Val::Other),
    }
}

struct Parsed {
    top: Table,
    /// each `[[app]]` with its `[app.startup]`
    apps: Vec<(Table, Table)>,
    notes: Vec<String>,
}

fn parse_toml(text: &str) -> Parsed {
    let mut p = Parsed { top: Table::new(), apps: Vec::new(), notes: Vec::new() };
    #[derive(PartialEq)]
    enum At {
        Top,
        App,
        Startup,
        Skip,
    }
    let mut at = At::Top;
    for (n, raw) in text.lines().enumerate() {
        let l = raw.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.starts_with('[') {
            let head = l.split('#').next().unwrap_or("").trim();
            at = match head {
                "[[app]]" => {
                    p.apps.push((Table::new(), Table::new()));
                    At::App
                }
                "[app.startup]" if !p.apps.is_empty() => At::Startup,
                _ => {
                    p.notes.push(format!("line {}: section {head} is not one Hayate reads; skipped", n + 1));
                    At::Skip
                }
            };
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let (k, val) = (k.trim().trim_matches('"').to_string(), parse_value(v));
        match at {
            At::Top => {
                p.top.insert(k, val);
            }
            At::App => {
                if let Some(a) = p.apps.last_mut() {
                    a.0.insert(k, val);
                }
            }
            At::Startup => {
                if let Some(a) = p.apps.last_mut() {
                    a.1.insert(k, val);
                }
            }
            At::Skip => {}
        }
    }
    p
}

fn s(t: &Table, k: &str) -> Option<String> {
    match t.get(k) {
        Some(Val::Str(v)) => Some(v.clone()),
        _ => None,
    }
}

fn b(t: &Table, k: &str, d: bool) -> bool {
    match t.get(k) {
        Some(Val::Bool(v)) => *v,
        _ => d,
    }
}

fn source_id(src: &str, name: &str) -> Option<EntryId> {
    let (kind, scope, view) = match src {
        "hkcu_run" => (Kind::Run, Scope::User, View::Native),
        "hklm_run" => (Kind::Run, Scope::AllUsers, View::Native),
        "hklm_run32" => (Kind::Run, Scope::AllUsers, View::Wow32),
        "user_folder" => (Kind::Folder, Scope::User, View::Native),
        "common_folder" => (Kind::Folder, Scope::AllUsers, View::Native),
        _ => return None,
    };
    (!name.is_empty() && name.chars().count() <= super::id::MAX_KEY && !name.chars().any(|c| c.is_control())).then(|| EntryId::new(kind, scope, view, name))
}

// ---- the preview ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ImportItem {
    pub app: App,
    /// name of the entry it waits for, as written in the file
    pub after_name: Option<String>,
    /// the Windows startup entry Launchpad replaced, and how it looks now
    pub original: Option<(EntryId, String)>,
    /// reasons it cannot be imported
    pub problems: Vec<String>,
    /// reasons to look twice (still importable)
    pub warnings: Vec<String>,
}

impl ImportItem {
    pub fn importable(&self) -> bool {
        self.problems.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Preview {
    pub path: String,
    pub items: Vec<ImportItem>,
    pub notes: Vec<String>,
}

pub const DUAL_LAUNCH_NOTE: &str = "Launchpad most likely still starts these programs at logon. Importing does not change Launchpad or any Windows startup entry. Entries are added switched off; before you turn one on here, decide which manager keeps starting it, or it will start twice.";

/// Read `path` and describe what importing it would do. Changes nothing.
pub fn preview_file(path: &Path, loc: &Locations, store: &Store) -> Result<Preview, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file.", path.display()));
    }
    if meta.len() as usize > MAX_FILE_BYTES {
        return Err("That file is too large to be a Launchpad config.".into());
    }
    // the size check above can be stale (the file may grow): the read itself is bounded
    let bytes = crate::osutil::read_bounded(path, MAX_FILE_BYTES).map_err(|e| if e.kind() == std::io::ErrorKind::InvalidData { "That file is too large to be a Launchpad config.".to_string() } else { format!("Cannot read {}: {e}", path.display()) })?;
    let text = String::from_utf8(bytes).map_err(|_| "That file is not UTF-8 text.".to_string())?;
    let mut pv = preview_text(&text, loc, store);
    pv.path = path.display().to_string();
    Ok(pv)
}

pub fn preview_text(text: &str, loc: &Locations, store: &Store) -> Preview {
    // a UTF-8 byte-order mark (Notepad, PowerShell 5) is not part of the first key
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let parsed = parse_toml(text);
    let mut pv = Preview { notes: parsed.notes.clone(), ..Preview::default() };
    if parsed.apps.is_empty() {
        pv.notes.push("No [[app]] entries found. Is this a Launchpad config.toml?".into());
        return pv;
    }
    pv.notes.push(DUAL_LAUNCH_NOTE.into());
    let existing = store.snapshot().cfg;
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, (t, st)) in parsed.apps.iter().enumerate() {
        if i >= MAX_APPS {
            pv.notes.push(format!("Only the first {MAX_APPS} entries are read; the rest are ignored."));
            break;
        }
        let name = s(t, "name").unwrap_or_default();
        let command = s(t, "command").unwrap_or_default();
        // placeholder id so the shared field check runs; `apply` assigns the real one
        let mut app = App::new("preview".into(), name.clone(), command);
        let mut problems = Vec::new();
        let mut warnings = Vec::new();
        for k in ["name", "command"] {
            if !matches!(t.get(k), Some(Val::Str(_))) {
                problems.push(format!("{k} is missing or not text"));
            }
        }
        app.enabled = false;
        match t.get("delay") {
            None => {}
            Some(Val::Num(d)) if d.is_finite() && *d >= 0.0 && *d <= 3600.0 => app.delay_ms = (*d * 1000.0).round() as u32,
            Some(_) => problems.push("delay must be 0-3600 seconds".into()),
        }
        match t.get("cpu_below") {
            None => {}
            Some(Val::Num(c)) if c.fract() == 0.0 && (1.0..=99.0).contains(c) => app.cpu_below = Some(*c as u8),
            Some(_) => problems.push("CPU limit must be a whole number 1-99".into()),
        }
        app.admin = b(t, "admin", false);
        app.process = s(t, "process").filter(|p| !p.trim().is_empty());
        let after_name = s(t, "after").filter(|a| !a.is_empty());
        if problems.is_empty() {
            if let Some(p) = field_problem(&app) {
                problems.push(p);
            }
        }
        // clashes: with the current list and inside the file
        let key = name.trim().to_lowercase();
        if existing.apps.iter().any(|a| a.name.trim().to_lowercase() == key) {
            problems.push("an entry with this name is already in the Hayate list".into());
        }
        if let Some(first) = seen.insert(key, i) {
            problems.push(format!("the file lists this name twice (entries {} and {})", first + 1, i + 1));
        }
        let original = (!st.is_empty()).then(|| source_id(&s(st, "source").unwrap_or_default(), &s(st, "name").unwrap_or_default())).flatten().map(|id| {
            let state = match approved::read(loc, &id) {
                Ok(l) if !l.present => "no longer exists".to_string(),
                Ok(l) => match l.effective() {
                    approved::State::Enabled => "currently enabled".into(),
                    approved::State::Disabled => "currently disabled".into(),
                    approved::State::Unknown => "state not recognised".into(),
                },
                Err(e) => format!("unreadable ({})", e.text()),
            };
            (id, state)
        });
        if let Some((id, _)) = &original {
            if let Some(owner) = existing.owner_of(id) {
                warnings.push(format!("its original entry is already managed by \"{}\"", owner.name));
            }
        }
        if b(t, "enabled", true) {
            warnings.push("Launchpad starts it at logon".into());
        }
        pv.items.push(ImportItem { app, after_name, original, problems, warnings });
    }
    // dependencies must point at something that exists once imported
    let names: Vec<String> = pv.items.iter().map(|i| i.app.name.trim().to_lowercase()).collect();
    for it in pv.items.iter_mut() {
        if let Some(a) = &it.after_name {
            let k = a.trim().to_lowercase();
            let in_file = names.contains(&k);
            let in_list = existing.apps.iter().any(|x| x.name.trim().to_lowercase() == k);
            if !in_file && !in_list {
                it.warnings.push(format!("it waited for \"{a}\", which is not in the file or the list; that wait is dropped"));
            }
        }
    }
    pv
}

#[derive(Debug, Default, PartialEq)]
pub struct Imported {
    pub added: Vec<String>,
    pub notes: Vec<String>,
}

/// Add the selected, importable entries (all in one save). `selected[i]`
/// refers to `pv.items[i]`. Entries are added switched off.
pub fn apply(store: &Store, pv: &Preview, selected: &[bool]) -> Result<Imported, String> {
    let picks: Vec<&ImportItem> = pv.items.iter().enumerate().filter(|(i, it)| selected.get(*i).copied().unwrap_or(false) && it.importable()).map(|(_, it)| it).collect();
    if picks.is_empty() {
        return Err("Nothing selected.".into());
    }
    let mut out = Imported::default();
    store.update_sync(|c| {
        let mut ids: HashMap<String, String> = c.apps.iter().map(|a| (a.name.trim().to_lowercase(), a.id.clone())).collect();
        let first_new = c.apps.len();
        for it in &picks {
            let mut a = it.app.clone();
            a.id = c.fresh_id();
            a.enabled = false;
            a.after = None;
            ids.insert(a.name.trim().to_lowercase(), a.id.clone());
            out.added.push(a.name.clone());
            c.apps.push(a);
        }
        for (k, it) in picks.iter().enumerate() {
            if let Some(dep) = &it.after_name {
                match ids.get(&dep.trim().to_lowercase()) {
                    Some(id) => c.apps[first_new + k].after = Some(id.clone()),
                    None => out.notes.push(format!("{}: the wait for \"{dep}\" was dropped (not imported)", it.app.name)),
                }
            }
        }
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::reg::scratch::Scratch;
    use std::sync::Arc;
    use windows::Win32::System::Registry::*;

    const SAMPLE: &str = r#"
# written by launchpad
stay_resident = true
guard = true
auto_update = true

[[app]]
name = "Steam"
command = '"C:\Program Files (x86)\Steam\steam.exe" -silent'
enabled = true
delay = 0.0
admin = false

[app.startup]
source = "hkcu_run"
name = "Steam"

[[app]]
name = "Discord"
command = "\"C:\\Users\\u\\AppData\\Local\\Discord\\Update.exe\" --processStart Discord.exe"
enabled = false
delay = 2.5
after = "Steam"
cpu_below = 30
admin = true
process = "discord.exe"

[app.startup]
source = "hklm_run32"
name = "Discord"

[[app]]
name = "Broken"
command = "x.exe"
delay = 99999
cpu_below = 250
"#;

    struct Rig {
        _s: Scratch,
        dir: std::path::PathBuf,
        loc: Locations,
        store: Arc<Store>,
    }
    fn rig(tag: &str) -> Rig {
        let s = Scratch::new(tag);
        let dir = std::env::temp_dir().join(format!("HayateStartupFixture-import-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let loc = Locations { hkcu: s.user.0, hklm: s.machine.0, user_dir: dir.join("user"), common_dir: dir.join("common") };
        let store = Arc::new(Store::new(dir.join("startup.ini")));
        store.load();
        Rig { _s: s, dir, loc, store }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn parses_basic_and_literal_strings_numbers_and_the_startup_subtable() {
        let r = rig("parse");
        let pv = preview_text(SAMPLE, &r.loc, &r.store);
        assert_eq!(pv.items.len(), 3);
        let steam = &pv.items[0];
        assert_eq!(steam.app.command, r#""C:\Program Files (x86)\Steam\steam.exe" -silent"#, "literal string, backslashes kept");
        assert!(steam.importable());
        let d = &pv.items[1];
        assert_eq!(d.app.command, r#""C:\Users\u\AppData\Local\Discord\Update.exe" --processStart Discord.exe"#, "basic string escapes decoded");
        assert_eq!((d.app.delay_ms, d.app.cpu_below, d.app.admin), (2500, Some(30), true));
        assert_eq!(d.after_name.as_deref(), Some("Steam"));
        assert_eq!(d.app.process.as_deref(), Some("discord.exe"));
        assert_eq!(d.original.as_ref().unwrap().0, EntryId::new(Kind::Run, Scope::AllUsers, View::Wow32, "Discord"));
        assert!(!pv.items[2].importable(), "{:?}", pv.items[2].problems);
        assert!(pv.items[2].problems.iter().any(|p| p.contains("delay")));
        assert!(pv.notes.iter().any(|n| n.contains("starts these programs at logon")), "dual launch is stated up front");
    }

    #[test]
    fn preview_and_cancel_change_nothing() {
        let r = rig("cancel");
        let file = r.dir.join("config.toml");
        std::fs::write(&file, SAMPLE).unwrap();
        let before = std::fs::read(&file).unwrap();
        let pv = preview_file(&file, &r.loc, &r.store).unwrap();
        assert_eq!(pv.items.len(), 3);
        drop(pv); // cancel
        assert!(r.store.snapshot().cfg.apps.is_empty());
        assert!(!r.dir.join("startup.ini").exists());
        assert_eq!(std::fs::read(&file).unwrap(), before, "the source file is untouched");
    }

    #[test]
    fn nothing_is_imported_unless_selected_and_entries_arrive_switched_off() {
        let r = rig("apply");
        let pv = preview_text(SAMPLE, &r.loc, &r.store);
        assert_eq!(apply(&r.store, &pv, &[false, false, false]).unwrap_err(), "Nothing selected.");
        assert!(apply(&r.store, &pv, &[false, false, true]).is_err(), "a broken entry cannot be force-selected");
        let out = apply(&r.store, &pv, &[true, true, true]).unwrap();
        assert_eq!(out.added, vec!["Steam", "Discord"]);
        let c = r.store.snapshot().cfg;
        assert_eq!(c.apps.len(), 2);
        assert!(c.apps.iter().all(|a| !a.enabled && a.own.is_none()), "added off, owning nothing");
        assert_eq!(c.apps[1].after.as_deref(), Some(c.apps[0].id.as_str()), "the dependency is rewired to the new id");
        assert!(!c.manage, "importing never turns the manager on");
        assert!(crate::startup::model::validate(&c).is_empty());
    }

    #[test]
    fn a_dependency_that_is_not_imported_is_dropped_and_said() {
        let r = rig("dep");
        let pv = preview_text(SAMPLE, &r.loc, &r.store);
        let out = apply(&r.store, &pv, &[false, true, false]).unwrap();
        assert!(out.notes.iter().any(|n| n.contains("dropped")), "{out:?}");
        assert_eq!(r.store.snapshot().cfg.apps[0].after, None);
    }

    #[test]
    fn name_clashes_with_the_list_and_inside_the_file_block_import() {
        let r = rig("clash");
        r.store
            .update_sync(|c| {
                let id = c.fresh_id();
                c.apps.push(App::new(id, "steam".into(), "x.exe".into()));
            })
            .unwrap();
        let text = format!("{SAMPLE}\n[[app]]\nname = \"Discord\"\ncommand = \"d.exe\"\n");
        let pv = preview_text(&text, &r.loc, &r.store);
        assert!(pv.items[0].problems.iter().any(|p| p.contains("already in the Hayate list")));
        assert!(pv.items[1].problems.is_empty() || pv.items[1].problems.iter().any(|p| p.contains("twice")));
        assert!(pv.items[3].problems.iter().any(|p| p.contains("twice")));
        assert!(apply(&r.store, &pv, &[true, false, false, false]).is_err());
    }

    #[test]
    fn the_originals_current_state_is_shown_read_only() {
        let r = rig("orig");
        let k = crate::startup::reg::create(r.loc.hkcu, approved::RUN, KEY_READ | KEY_WRITE | KEY_WOW64_64KEY).unwrap();
        let v: Vec<u8> = crate::osutil::wide("C:\\x.exe").iter().flat_map(|c| c.to_le_bytes()).collect();
        let n = crate::osutil::wide("Steam");
        unsafe {
            assert_eq!(RegSetValueExW(k.0, crate::osutil::pcw(&n), None, REG_SZ, Some(&v)), windows::Win32::Foundation::ERROR_SUCCESS);
        }
        let id = EntryId::new(Kind::Run, Scope::User, View::Native, "Steam");
        approved::write(&r.loc, &id, Some(&approved::disabled_bytes(3))).unwrap();
        let pv = preview_text(SAMPLE, &r.loc, &r.store);
        assert_eq!(pv.items[0].original.as_ref().unwrap().1, "currently disabled");
        assert_eq!(pv.items[1].original.as_ref().unwrap().1, "no longer exists");
        apply(&r.store, &pv, &[true, false, false]).unwrap();
        assert_eq!(approved::read(&r.loc, &id).unwrap().approved, Some(approved::disabled_bytes(3)), "import left the Windows entry exactly as it was");
    }

    #[test]
    fn hostile_files_are_refused_or_contained() {
        let r = rig("hostile");
        let big = r.dir.join("big.toml");
        std::fs::write(&big, vec![b'#'; MAX_FILE_BYTES + 10]).unwrap();
        assert!(preview_file(&big, &r.loc, &r.store).unwrap_err().contains("too large"));
        let bin = r.dir.join("bin.toml");
        std::fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
        assert!(preview_file(&bin, &r.loc, &r.store).unwrap_err().contains("UTF-8"));
        assert!(preview_file(&r.dir, &r.loc, &r.store).is_err(), "a folder");
        assert!(preview_file(&r.dir.join("missing.toml"), &r.loc, &r.store).is_err());
        let pv = preview_text("just = \"text\"\n", &r.loc, &r.store);
        assert!(pv.items.is_empty() && pv.notes.iter().any(|n| n.contains("Is this a Launchpad")));
        // control characters, newlines via escapes, and a command with a NUL are problems, not imports
        let evil = "[[app]]\nname = \"a\\nb\"\ncommand = \"x\\u0000y\"\n[[app]]\nname = \"ok\"\ncommand = \"\"\"multi\nline\"\"\"\n";
        let pv = preview_text(evil, &r.loc, &r.store);
        assert!(pv.items.iter().all(|i| !i.importable()), "{:?}", pv.items.iter().map(|i| &i.problems).collect::<Vec<_>>());
        // more entries than the list can hold are cut off
        let many: String = (0..MAX_APPS + 5).map(|i| format!("[[app]]\nname = \"n{i}\"\ncommand = \"c{i}.exe\"\n")).collect();
        let pv = preview_text(&many, &r.loc, &r.store);
        assert_eq!(pv.items.len(), MAX_APPS);
    }
}
