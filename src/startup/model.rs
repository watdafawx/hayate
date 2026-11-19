//! The managed startup list: what the user asked Hayate to start at logon, and
//! the exact original state of every startup entry Hayate took over.
//!
//! Plain data + validation + a one-line-per-field text format (no new
//! dependency). Everything read from disk is untrusted: sizes, ranges, names,
//! references and shapes are checked here, once, before any other code uses it.

use std::collections::{HashMap, HashSet};

use super::approved;
use super::id::EntryId;

pub const MAX_APPS: usize = 200;
pub const MAX_NAME: usize = 80;
pub const MAX_COMMAND: usize = 2048;
pub const MAX_PROCESS: usize = 128;
pub const MAX_DELAY_MS: u32 = 3_600_000;
/// Larger than this is not a config this feature wrote.
pub const MAX_FILE_BYTES: usize = 512 * 1024;
const FORMAT: &str = "hayate-startup 1";
/// Written by 0.1.0: still read as the current format (data, not migration code).
const LEGACY_FORMAT: &str = "blink-startup 1";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OwnState {
    /// the owning record is saved but the original is not disabled yet (or the step is unproven)
    Pending,
    /// the original is disabled by Hayate and the record says exactly what to put back
    Owned,
}

/// The startup entry a managed app took over, with the state to restore.
#[derive(Clone, Debug, PartialEq)]
pub struct Own {
    pub source: EntryId,
    pub state: OwnState,
    /// StartupApproved bytes of the entry before Hayate touched it (None = no value)
    pub original: Option<Vec<u8>>,
    /// the bytes Hayate wrote to disable it; restore only while the live value is still these
    pub written: Vec<u8>,
    /// the Run value / file path as it was when adopted (to report a later external change)
    pub raw_at_adopt: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct App {
    /// stable reference used by `after` (survives renames)
    pub id: String,
    pub name: String,
    /// command line, .lnk path or shell: URI; %VARS% expanded at launch
    pub command: String,
    pub enabled: bool,
    pub delay_ms: u32,
    pub after: Option<String>,
    pub cpu_below: Option<u8>,
    /// start elevated through an explicit UAC prompt every time
    pub admin: bool,
    pub process: Option<String>,
    pub own: Option<Own>,
}

impl App {
    pub fn new(id: String, name: String, command: String) -> App {
        App { id, name, command, enabled: true, delay_ms: 0, after: None, cpu_below: None, admin: false, process: None, own: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// run the list at logon when Hayate starts (default OFF: nothing runs until the user opts in)
    pub manage: bool,
    pub apps: Vec<App>,
    pub next_id: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config { manage: false, apps: Vec::new(), next_id: 1 }
    }
}

impl Config {
    pub fn fresh_id(&mut self) -> String {
        loop {
            let id = format!("a{}", self.next_id);
            self.next_id += 1;
            if !self.apps.iter().any(|a| a.id == id) {
                return id;
            }
        }
    }

    pub fn app(&self, id: &str) -> Option<&App> {
        self.apps.iter().find(|a| a.id == id)
    }

    pub fn owner_of(&self, e: &EntryId) -> Option<&App> {
        self.apps.iter().find(|a| a.own.as_ref().is_some_and(|o| &o.source == e))
    }
}

// ---- validation ----------------------------------------------------------------

fn bad_text(s: &str) -> bool {
    s.chars().any(|c| c.is_control())
}

/// What is wrong with one entry's own fields (None = fine).
pub fn field_problem(a: &App) -> Option<String> {
    let n = a.name.trim();
    if n.is_empty() || n.chars().count() > MAX_NAME || bad_text(n) {
        return Some(format!("name must be 1-{MAX_NAME} characters, no control characters"));
    }
    let c = a.command.trim();
    if c.is_empty() || c.chars().count() > MAX_COMMAND || bad_text(c) {
        return Some(format!("command must be 1-{MAX_COMMAND} characters on one line"));
    }
    if a.delay_ms > MAX_DELAY_MS {
        return Some("delay is at most 3600 seconds".into());
    }
    if let Some(p) = a.cpu_below {
        if !(1..=99).contains(&p) {
            return Some("CPU limit must be 1-99 %".into());
        }
    }
    if let Some(p) = &a.process {
        if p.trim().is_empty() || p.chars().count() > MAX_PROCESS || bad_text(p) || p.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            return Some("process name must be a bare file name like app.exe".into());
        }
    }
    if a.id.is_empty() || a.id.len() > 16 || !a.id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Some("internal id is malformed".into());
    }
    if let Some(o) = &a.own {
        if !approved::canonical(&o.written) || o.original.as_deref().is_some_and(|b| !approved::canonical(b)) || o.raw_at_adopt.chars().count() > MAX_COMMAND || bad_text(&o.raw_at_adopt) {
            return Some("ownership record is not a value Windows writes".into());
        }
    }
    None
}

/// Every reason this list cannot be saved or run, in words. Empty = fine.
pub fn validate(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if cfg.apps.len() > MAX_APPS {
        out.push(format!("at most {MAX_APPS} entries"));
        return out;
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    let mut owned = HashSet::new();
    for a in &cfg.apps {
        let label = if a.name.trim().is_empty() { a.id.clone() } else { a.name.clone() };
        if let Some(p) = field_problem(a) {
            out.push(format!("{label}: {p}"));
        }
        if !ids.insert(a.id.clone()) {
            out.push(format!("{label}: duplicate internal id"));
        }
        if !names.insert(a.name.trim().to_lowercase()) {
            out.push(format!("{label}: another entry has the same name"));
        }
        if let Some(o) = &a.own {
            if !owned.insert(o.source.clone()) {
                out.push(format!("{label}: its original startup entry is already taken over by another entry"));
            }
        }
    }
    let by: HashMap<&str, &App> = cfg.apps.iter().map(|a| (a.id.as_str(), a)).collect();
    for a in &cfg.apps {
        if let Some(d) = a.after.as_deref() {
            if d == a.id {
                out.push(format!("{}: cannot start after itself", a.name));
            } else if !by.contains_key(d) {
                out.push(format!("{}: the entry it starts after no longer exists", a.name));
            }
        }
    }
    // cycles: follow each chain; a chain longer than the list repeats
    for a in &cfg.apps {
        if a.after.as_deref() == Some(a.id.as_str()) {
            continue; // already reported
        }
        let mut cur = a;
        let mut hops = 0;
        while let Some(next) = cur.after.as_deref().and_then(|d| by.get(d).copied()) {
            hops += 1;
            if std::ptr::eq(next, a) || hops > cfg.apps.len() {
                out.push(format!("{}: starts after itself through a chain", a.name));
                break;
            }
            cur = next;
        }
    }
    out
}

// ---- text format -----------------------------------------------------------------

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '%' || c.is_control() {
            let mut b = [0u8; 4];
            for byte in c.encode_utf8(&mut b).bytes() {
                o.push_str(&format!("%{byte:02X}"));
            }
        } else {
            o.push(c);
        }
    }
    o
}

fn unesc(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut v = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            v.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            v.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(v).ok()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || s.len() > 256 {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()).collect()
}

pub fn write(cfg: &Config) -> String {
    let mut o = String::new();
    o.push_str(FORMAT);
    o.push('\n');
    o.push_str(&format!("manage={}\nnext_id={}\n", cfg.manage as u8, cfg.next_id));
    for a in &cfg.apps {
        o.push_str("\n[app]\n");
        o.push_str(&format!("id={}\nname={}\ncommand={}\nenabled={}\ndelay_ms={}\nadmin={}\n", a.id, esc(&a.name), esc(&a.command), a.enabled as u8, a.delay_ms, a.admin as u8));
        if let Some(d) = &a.after {
            o.push_str(&format!("after={d}\n"));
        }
        if let Some(p) = a.cpu_below {
            o.push_str(&format!("cpu_below={p}\n"));
        }
        if let Some(p) = &a.process {
            o.push_str(&format!("process={}\n", esc(p)));
        }
        if let Some(w) = &a.own {
            o.push_str(&format!("own.source={}\nown.state={}\nown.original={}\nown.written={}\nown.raw={}\n", esc(&w.source.encode()), if w.state == OwnState::Owned { "owned" } else { "pending" }, w.original.as_deref().map(hex).unwrap_or_else(|| "none".into()), hex(&w.written), esc(&w.raw_at_adopt)));
        }
    }
    o
}

pub struct Loaded {
    pub cfg: Config,
    /// sections that could not be read (kept in the `.bak` copy, never silently lost)
    pub dropped: Vec<String>,
    /// how many of the dropped sections carried an ownership record of a
    /// per-user entry (their originals cannot be given back by this process).
    /// A dropped all-users record is not counted: such a record is only ever
    /// acted on with its administrator-protected copy (`proof`), and an
    /// unreadable one, forged or not, must not block an uninstall.
    pub dropped_owned: usize,
    /// set when the file is from a newer Hayate: do not overwrite it
    pub locked: Option<String>,
}

pub fn parse(text: &str) -> Loaded {
    let mut out = Loaded { cfg: Config::default(), dropped: Vec::new(), dropped_owned: 0, locked: None };
    if text.len() > MAX_FILE_BYTES {
        out.locked = Some("startup list file is too large to be one Hayate wrote".into());
        return out;
    }
    let mut lines = text.lines();
    match lines.next().map(str::trim) {
        Some(h) if h == FORMAT || h == LEGACY_FORMAT => {}
        Some(h) if h.starts_with("hayate-startup ") || h.starts_with("blink-startup ") => {
            out.locked = Some("startup list was written by a newer Hayate".into());
            return out;
        }
        None | Some("") => return out,
        Some(_) => {
            out.locked = Some("startup list file is not in a format Hayate knows".into());
            return out;
        }
    }
    let mut cur: Option<HashMap<String, String>> = None;
    let mut sections: Vec<HashMap<String, String>> = Vec::new();
    for l in lines {
        let l = l.trim_end();
        if l.is_empty() {
            continue;
        }
        if l == "[app]" {
            if let Some(c) = cur.take() {
                sections.push(c);
            }
            cur = Some(HashMap::new());
        } else if let Some((k, v)) = l.split_once('=') {
            match cur.as_mut() {
                Some(c) => {
                    c.insert(k.to_string(), v.to_string());
                }
                None => match k {
                    "manage" => out.cfg.manage = v == "1",
                    "next_id" => out.cfg.next_id = v.parse().unwrap_or(1).max(1),
                    _ => {}
                },
            }
        }
    }
    if let Some(c) = cur.take() {
        sections.push(c);
    }
    for (i, s) in sections.into_iter().enumerate() {
        let had_own = s.get("own.source").is_some_and(|v| !unesc(v).and_then(|t| EntryId::parse(&t)).is_some_and(|id| id.scope == super::id::Scope::AllUsers));
        if out.cfg.apps.len() >= MAX_APPS {
            out.dropped.push(format!("entry {} (over the {MAX_APPS} limit)", i + 1));
            out.dropped_owned += had_own as usize;
            continue;
        }
        match app_of(&s) {
            Some(a) => match field_problem(&a) {
                None => out.cfg.apps.push(a),
                Some(p) => {
                    out.dropped.push(format!("{}: {p}", if a.name.is_empty() { a.id } else { a.name }));
                    out.dropped_owned += had_own as usize;
                }
            },
            None => {
                out.dropped.push(format!("entry {} (unreadable)", i + 1));
                out.dropped_owned += had_own as usize;
            }
        }
    }
    out
}

fn app_of(s: &HashMap<String, String>) -> Option<App> {
    let get = |k: &str| s.get(k).map(String::as_str);
    let mut a = App::new(get("id")?.to_string(), unesc(get("name")?)?, unesc(get("command")?)?);
    a.enabled = get("enabled") != Some("0");
    a.admin = get("admin") == Some("1");
    a.delay_ms = get("delay_ms").and_then(|v| v.parse().ok()).unwrap_or(0);
    a.after = get("after").filter(|v| !v.is_empty()).map(str::to_string);
    a.cpu_below = match get("cpu_below") {
        Some(v) => Some(v.parse().ok()?),
        None => None,
    };
    a.process = match get("process") {
        Some(v) => Some(unesc(v)?),
        None => None,
    };
    if let Some(src) = get("own.source") {
        let original = match get("own.original")? {
            "none" => None,
            h => Some(unhex(h)?),
        };
        a.own = Some(Own {
            source: EntryId::parse(&unesc(src)?)?,
            state: match get("own.state")? {
                "owned" => OwnState::Owned,
                "pending" => OwnState::Pending,
                _ => return None,
            },
            original,
            written: unhex(get("own.written")?)?,
            raw_at_adopt: unesc(get("own.raw").unwrap_or(""))?,
        });
    }
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::id::{Kind, Scope, View};

    fn app(id: &str, name: &str) -> App {
        App::new(id.into(), name.into(), format!(r"C:\nope\{name}.exe"))
    }

    #[test]
    fn default_is_off_and_empty() {
        let c = Config::default();
        assert!(!c.manage && c.apps.is_empty());
        assert!(!parse("").cfg.manage);
        assert!(!parse(&write(&Config::default())).cfg.manage);
    }

    #[test]
    fn round_trips_every_field_including_awkward_text() {
        let mut c = Config { manage: true, apps: vec![app("a1", "One"), app("a2", "Two % 100 é=|[app]")], next_id: 9 };
        c.apps[0].delay_ms = 2500;
        c.apps[0].cpu_below = Some(30);
        c.apps[0].process = Some("one.exe".into());
        c.apps[1].after = Some("a1".into());
        c.apps[1].admin = true;
        c.apps[1].enabled = false;
        c.apps[1].command = "\"C:\\x y\\a.exe\" --k=v%20".into();
        c.apps[1].own = Some(Own { source: EntryId::new(Kind::Run, Scope::User, View::Native, "Weird|Name%"), state: OwnState::Owned, original: Some(approved::enabled_bytes()), written: approved::disabled_bytes(77), raw_at_adopt: r"C:\old.exe".into() });
        c.apps[0].own = Some(Own { source: EntryId::new(Kind::Folder, Scope::AllUsers, View::Native, "x.lnk"), state: OwnState::Pending, original: None, written: approved::disabled_bytes(5), raw_at_adopt: String::new() });
        let l = parse(&write(&c));
        assert!(l.dropped.is_empty() && l.locked.is_none(), "{:?}", l.dropped);
        assert_eq!(l.cfg, c);
        assert!(validate(&l.cfg).is_empty());
    }

    #[test]
    fn hostile_values_are_dropped_not_trusted() {
        let long = "x".repeat(MAX_COMMAND + 1);
        let text = format!(
            "{FORMAT}\n\n[app]\nid=a1\nname=ok\ncommand=C:\\a.exe\n\n[app]\nid=a2\nname=big\ncommand={long}\n\n[app]\nid=a3\nname=cpu\ncommand=x.exe\ncpu_below=200\n\n[app]\nid=a4\nname=proc\ncommand=x.exe\nprocess=..%5Cevil.exe\n\n[app]\nid=a5\nname=delay\ncommand=x.exe\ndelay_ms=99999999\n\n[app]\nid=a6\nname=own\ncommand=x.exe\nown.source=run|user|64|k\nown.state=owned\nown.original=none\nown.written=ff\n\n[app]\nname=noid\ncommand=x\n"
        );
        let l = parse(&text);
        assert_eq!(l.cfg.apps.len(), 1, "{:?}", l.dropped);
        assert_eq!(l.cfg.apps[0].name, "ok");
        assert_eq!(l.dropped.len(), 6);
    }

    #[test]
    fn newer_or_foreign_files_are_locked_not_overwritten() {
        assert!(parse("hayate-startup 2\nmanage=1\n").locked.is_some());
        assert!(parse("[[app]]\nname = \"x\"\n").locked.is_some());
        assert!(parse(&"x".repeat(MAX_FILE_BYTES + 1)).locked.is_some());
    }

    #[test]
    fn validation_covers_duplicates_missing_deps_and_cycles() {
        let mut c = Config { apps: vec![app("a1", "One"), app("a2", "Two"), app("a3", "Three")], ..Config::default() };
        assert!(validate(&c).is_empty());
        c.apps[1].after = Some("a1".into());
        c.apps[2].after = Some("a2".into());
        assert!(validate(&c).is_empty(), "a chain is fine");
        c.apps[0].after = Some("a3".into());
        assert!(validate(&c).iter().any(|p| p.contains("through a chain")), "{:?}", validate(&c));
        c.apps[0].after = Some("a1".into());
        assert!(validate(&c).iter().any(|p| p.contains("after itself")));
        c.apps[0].after = Some("zz".into());
        assert!(validate(&c).iter().any(|p| p.contains("no longer exists")));
        c.apps[0].after = None;
        c.apps[2].name = "ONE".into();
        assert!(validate(&c).iter().any(|p| p.contains("same name")));
        c.apps[2].name = "Three".into();
        c.apps[2].id = "a1".into();
        assert!(validate(&c).iter().any(|p| p.contains("duplicate internal id")));
    }

    #[test]
    fn one_original_entry_has_one_owner() {
        let src = EntryId::new(Kind::Run, Scope::User, View::Native, "Dup");
        let own = || Own { source: src.clone(), state: OwnState::Owned, original: None, written: approved::disabled_bytes(1), raw_at_adopt: String::new() };
        let mut c = Config { apps: vec![app("a1", "One"), app("a2", "Two")], ..Config::default() };
        c.apps[0].own = Some(own());
        c.apps[1].own = Some(own());
        assert!(validate(&c).iter().any(|p| p.contains("already taken over")));
        assert_eq!(c.owner_of(&EntryId::new(Kind::Run, Scope::User, View::Native, "dup")).unwrap().id, "a1");
    }

    #[test]
    fn fresh_ids_never_collide() {
        let mut c = Config { apps: vec![app("a1", "One"), app("a2", "Two")], next_id: 1, ..Config::default() };
        assert_eq!(c.fresh_id(), "a3");
        assert_eq!(c.fresh_id(), "a4");
    }
}
