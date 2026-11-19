//! Startup tab.
//!
//! Two views of one idea:
//! * **Windows entries**: everything Windows starts at sign-in (Run keys, Startup
//!   folders), with stable identity, scope, state and who manages it. Enable or
//!   disable an ordinary entry the way Task Manager does, or take it over.
//! * **Hayate list**: the apps Hayate itself starts (opt-in), with an editor, a
//!   deterministic scheduler (delay, After, CPU idle gate), an explicit Run now,
//!   and an import of a Launchpad config.
//!
//! Nothing here runs by itself: opening the tab only reads. Every change is an
//! explicit click, done on the I/O worker, and reported only after it is on disk
//! or in the registry and read back. Overlays (editor, import) take keyboard
//! input; `key`/`char` are routed here from `App` while one is open.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::cpu::{visible_rect, wrap};
use super::setup::accent;
use super::*;
use crate::startup::approved::{Locations, State};
use crate::startup::id::{EntryId, Kind, Scope};
use crate::startup::model::{self, App as MApp, Config};
use crate::startup::scan::{self, Entry, Scan};
use crate::startup::{adopt, import, run};

// ---- hit ids ---------------------------------------------------------------------------------

const SEG_WIN: usize = 1;
const SEG_LIST: usize = 2;
const FILTER0: usize = 3; // 3..=7
const REFRESH: usize = 10;
const ADD: usize = 11;
const IMPORT: usize = 12;
const RUN_ALL: usize = 13;
const CANCEL: usize = 14;
const MANAGE: usize = 15;
const AUTOSTART: usize = 16;
const SIGNIN_FIX: usize = 17;
const D_TOGGLE: usize = 20;
const D_MANAGE: usize = 21;
const D_COPY: usize = 22;
const D_ADMIN: usize = 23;
const D_OPENLIST: usize = 24;
const D_EDIT: usize = 25;
const D_RUN: usize = 26;
const D_APP_ON: usize = 27;
const D_UNMANAGE: usize = 28;
const D_REMOVE: usize = 29;
const INLINE_FIELD: usize = 60;
const HDR0: usize = 40; // 40..=49
const ED_PANEL: usize = 100;
const ED_BACKDROP: usize = 101;
const ED_CLOSE: usize = 102;
const ED_SAVE: usize = 103;
const ED_NAME: usize = 104;
const ED_CMD: usize = 105;
const ED_DELAY: usize = 106;
const ED_PROC: usize = 107;
const ED_AFTER_P: usize = 108;
const ED_AFTER_N: usize = 109;
const ED_CPU_M: usize = 110;
const ED_CPU_P: usize = 111;
const ED_ADMIN: usize = 112;
const ED_ENABLED: usize = 113;
const IM_PANEL: usize = 200;
const IM_BACKDROP: usize = 201;
const IM_CLOSE: usize = 202;
const IM_PATH: usize = 203;
const IM_LOAD: usize = 204;
const IM_FILL: usize = 205;
const IM_ALL: usize = 206;
const IM_DO: usize = 207;
const IM_ITEM: usize = 1000;
const ROW: usize = 10000;

// ---- small types ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Windows,
    List,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Filter {
    All,
    Enabled,
    Disabled,
    Managed,
    AllUsers,
}

const FILTERS: [(Filter, &str); 5] = [(Filter::All, "All"), (Filter::Enabled, "Enabled"), (Filter::Disabled, "Disabled"), (Filter::Managed, "Managed"), (Filter::AllUsers, "All users")];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SortKey {
    Name,
    Status,
    Scope,
    Managed,
}

#[derive(Clone, PartialEq, Debug)]
enum Sel {
    Entry(EntryId),
    App(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Name,
    Command,
    Delay,
    Process,
}

/// A cell that can be changed with a double-click (everything else keeps the plain row behaviour).
/// Not editable in place, on purpose: Name, Command and Last run (free text / read-only: use the
/// editor panel), and CPU gate, Admin, Skip-if-running (no column; editor panel only).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cell {
    /// Windows entries: Status (Enabled/Disabled)
    Status,
    /// Hayate list: State (On/Off)
    AppState,
    /// Hayate list: Delay (seconds)
    Delay,
    /// Hayate list: After (which app it waits for)
    After,
}

/// Which editable cell column holds `x` in the last painted row layout.
fn cell_at(cols: &[(Cell, f32, f32)], x: f32) -> Option<Cell> {
    cols.iter().find(|(_, a, b)| x >= *a && x < *b).map(|(c, _, _)| *c)
}

/// A Delay edit in progress: the text of the small field over the cell.
struct Inline {
    app_id: String,
    text: String,
    err: Option<String>,
    saving: bool,
    token: u64,
}

/// What the Enable/Disable button, the context menu and a double-click on Status all ask:
/// may this entry be switched, and to what? (The write itself re-checks everything in `adopt::set_enabled`.)
#[derive(Clone, Debug, PartialEq)]
enum Toggle {
    Go(bool),
    No(String),
}

fn toggle_for(state: State, machine: bool, user_override: bool, owned: bool, elevated: bool) -> Toggle {
    if owned {
        return Toggle::No("Hayate manages this entry; unmanage it first.".into());
    }
    if state == State::Unknown {
        return Toggle::No("Windows holds a setting for this entry in a form Hayate does not recognise.".into());
    }
    if machine && user_override {
        return Toggle::No("This all-users entry also has this user's own setting. Change it in Task Manager.".into());
    }
    if machine && !elevated {
        return Toggle::No("Changing an all-users entry needs administrator rights.".into());
    }
    Toggle::Go(state != State::Enabled)
}

fn is_elevated() -> bool {
    // anything but a proven normal token counts as elevated (unknown fails closed)
    crate::osutil::elevation() != Some(false)
}

fn toggle_of(e: &Entry, owned: bool) -> Toggle {
    toggle_for(e.state(), e.id.machine(), e.live.user_override.is_some(), owned, is_elevated())
}

/// Tooltip line for the Status cell.
fn status_hint(t: &Toggle) -> String {
    match t {
        Toggle::Go(true) => "Double-click to enable".into(),
        Toggle::Go(false) => "Double-click to disable".into(),
        Toggle::No(why) => format!("Cannot change: {why}"),
    }
}

/// Tooltip line for an editable cell of the Hayate list.
fn app_hint(c: Cell, a: &MApp) -> Option<String> {
    match c {
        Cell::AppState => Some(format!("Double-click to turn {}", if a.enabled { "off" } else { "on" })),
        Cell::Delay => Some("Double-click to set the delay".into()),
        Cell::After => Some("Double-click to choose what it waits for".into()),
        Cell::Status => None,
    }
}

/// The folder to show for "Open file location" of a Windows entry.
fn locate_of(e: &Entry) -> Option<String> {
    let p = if e.id.kind == Kind::Folder { e.command.clone() } else { crate::osutil::clean_image_path(&e.command) };
    (!p.trim().is_empty()).then_some(p)
}

struct Editor {
    /// editing this managed app (None: a new one, or a take-over)
    app_id: Option<String>,
    /// taking over this Windows entry (preview shown, original disabled only after Save)
    adopt: Option<(Entry, adopt::Plan)>,
    name: String,
    command: String,
    delay: String,
    process: String,
    after: Option<String>,
    cpu: Option<u8>,
    admin: bool,
    enabled: bool,
    focus: Option<Field>,
    err: Option<String>,
    saving: bool,
    /// the background job this editor waits for: a completion only closes the overlay that owns it
    token: u64,
    scroll: f32,
}

struct ImportPanel {
    path: String,
    focus: bool,
    preview: Option<import::Preview>,
    picked: Vec<bool>,
    err: Option<String>,
    loading: bool,
    importing: bool,
    /// the background job this panel waits for (see Editor::token)
    token: u64,
    scroll: f32,
    loaded: Arc<Mutex<Option<Result<import::Preview, String>>>>,
}

pub struct StartupView {
    pub scroll: f32,
    pub loaded: bool,
    data: crate::bgio::Cell<Arc<Scan>>,
    scan: Arc<Scan>,
    /// the first scan has arrived (before that the Start with Windows shortcut is not known, never "missing")
    scanned: bool,
    /// the user pressed Run at sign-in ON and it did not stick (no usable Start with Windows shortcut): the offer shows until it is sorted out
    want_signin: bool,
    mode: Mode,
    filter: Filter,
    sort: (SortKey, bool),
    sel: Option<Sel>,
    /// what the rows of the last paint stand for (hit `ROW + i`)
    order: Vec<Sel>,
    /// keyboard: where the selection was last seen in `order`, and the list height of the last paint
    nav_last: Option<usize>,
    list_h: f32,
    msg: Option<(bool, String)>,
    busy: Arc<AtomicBool>,
    /// finished jobs: (job token, ok, text)
    inbox: Arc<Mutex<Vec<(u64, bool, String)>>>,
    next_token: u64,
    editor: Option<Editor>,
    import: Option<ImportPanel>,
    /// two-step buttons (remove): the button + selection that was pressed once
    confirm: Option<(usize, Option<Sel>)>,
    wd: Widths,
    /// the editable cells of the last paint: (kind, x0, x1) for the rows' columns
    cols: Vec<(Cell, f32, f32)>,
    /// a Delay cell being edited in place
    inline: Option<Inline>,
}

impl Default for StartupView {
    fn default() -> Self {
        StartupView {
            scroll: 0.0,
            loaded: false,
            data: Default::default(),
            scan: Arc::new(Scan { entries: Vec::new(), problems: Vec::new() }),
            scanned: false,
            want_signin: false,
            mode: Mode::Windows,
            filter: Filter::All,
            sort: (SortKey::Name, true),
            sel: None,
            order: Vec::new(),
            nav_last: None,
            list_h: 0.0,
            msg: None,
            busy: Arc::new(AtomicBool::new(false)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            next_token: 0,
            editor: None,
            import: None,
            confirm: None,
            wd: Widths::default(),
            cols: Vec::new(),
            inline: None,
        }
    }
}

/// Debug builds only: `HAYATE_STARTUP_FIXTURE=<file>` replaces the scan with synthetic rows
/// (`name|on/off|user/all/all32/folder|command` per line) so README screenshots never show
/// real entries.
#[cfg(debug_assertions)]
fn fixture() -> Option<scan::Scan> {
    use crate::startup::id::{EntryId, Kind, Scope, View};
    let text = std::fs::read_to_string(crate::osutil::env_var("STARTUP_FIXTURE").ok()?).ok()?;
    let mut entries = Vec::new();
    for l in text.lines() {
        let p: Vec<&str> = l.splitn(4, '|').collect();
        if p.len() != 4 {
            continue;
        }
        let (kind, scope, view) = match p[2] {
            "user" => (Kind::Run, Scope::User, View::Native),
            "all" => (Kind::Run, Scope::AllUsers, View::Native),
            "all32" => (Kind::Run, Scope::AllUsers, View::Wow32),
            _ => (Kind::Folder, Scope::User, View::Native),
        };
        let mut st = vec![if p[1] == "on" { 2u8 } else { 3u8 }];
        st.extend([0u8; 11]);
        entries.push(scan::Entry {
            id: EntryId::new(kind, scope, view, p[0]),
            raw: p[3].into(),
            command: p[3].into(),
            target: None,
            icon: String::new(),
            live: crate::startup::approved::Live { present: true, wrong_type: false, approved: Some(st), user_override: None },
            is_hayate: false,
            sig: None,
        });
    }
    Some(scan::Scan { entries, problems: Vec::new() })
}

fn mgr() -> Option<&'static run::Manager> {
    run::manager()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl StartupView {
    /// Re-read every source on the I/O worker. Called when the tab is shown and
    /// by Refresh; there is no timer.
    pub fn load(&mut self) {
        self.loaded = true;
        self.inline = None;
        self.data.invalidate();
        self.data.refresh("startup scan", std::time::Duration::ZERO, || {
            #[cfg(debug_assertions)]
            let s = fixture().unwrap_or_else(|| scan::scan(&Locations::system()));
            #[cfg(not(debug_assertions))]
            let s = scan::scan(&Locations::system());
            super::icons::poke();
            Some(Arc::new(s))
        });
    }

    /// Pick up finished background work.
    fn take(&mut self) {
        if let Some(s) = self.data.get() {
            self.scan = s;
            self.scanned = true;
        }
        let any = self.apply_done();
        if any {
            self.load();
        }
        if let Some(im) = self.import.as_mut() {
            if let Some(r) = lock(&im.loaded).take() {
                im.loading = false;
                match r {
                    Ok(pv) => {
                        im.picked = vec![false; pv.items.len()];
                        im.err = None;
                        im.preview = Some(pv);
                    }
                    Err(e) => {
                        im.preview = None;
                        im.err = Some(e);
                    }
                }
            }
        }
    }

    /// Apply finished background jobs to the overlays that own them. True when any finished.
    fn apply_done(&mut self) -> bool {
        let done: Vec<(u64, bool, String)> = std::mem::take(&mut *lock(&self.inbox));
        let any = !done.is_empty();
        for (token, ok, text) in done {
            // a completion only touches the overlay that started that very job: a job that
            // belongs to something else (a toggle, Unmanage, ...) never closes or fails an editor
            let (mut close_editor, mut close_import) = (false, false);
            if let Some(ed) = self.editor.as_mut().filter(|e| e.saving && e.token == token) {
                if ok {
                    close_editor = true;
                } else {
                    ed.saving = false;
                    ed.err = Some(text.clone());
                }
            }
            if let Some(im) = self.import.as_mut().filter(|i| i.importing && i.token == token) {
                if ok {
                    close_import = true;
                } else {
                    im.importing = false;
                    im.err = Some(text.clone());
                }
            }
            if let Some(i) = self.inline.as_mut().filter(|i| i.saving && i.token == token) {
                if ok {
                    self.inline = None;
                } else {
                    i.saving = false;
                    i.err = Some(text.clone());
                }
            }
            if close_editor {
                self.editor = None;
            }
            if close_import {
                self.import = None;
            }
            self.msg = Some((ok, text));
        }
        any
    }

    /// Run `f` on the I/O worker; its text becomes the message once it returned.
    /// `Some(token)` = accepted (the caller marks its overlay as waiting for THAT
    /// token); `None` = refused because another change is still being saved, and
    /// nothing was started: the caller must leave its overlay and its edit alone.
    fn job(&mut self, label: &'static str, f: impl FnOnce() -> Result<String, String> + Send + 'static) -> Option<u64> {
        if self.busy.swap(true, Ordering::SeqCst) {
            self.msg = Some((false, "The last change is still being saved. Try again in a moment.".into()));
            return None;
        }
        self.next_token += 1;
        let token = self.next_token;
        self.msg = Some((true, "Working…".into()));
        let (busy, inbox) = (self.busy.clone(), self.inbox.clone());
        crate::bgio::submit(label, move || {
            struct Guard {
                busy: Arc<AtomicBool>,
                inbox: Arc<Mutex<Vec<(u64, bool, String)>>>,
                token: u64,
                result: Option<(bool, String)>,
            }
            impl Drop for Guard {
                fn drop(&mut self) {
                    let r = self.result.take().unwrap_or((false, "The change did not finish (internal error).".into()));
                    lock(&self.inbox).push((self.token, r.0, r.1));
                    self.busy.store(false, Ordering::SeqCst);
                    super::icons::poke();
                }
            }
            let mut g = Guard { busy, inbox, token, result: None };
            g.result = Some(match f() {
                Ok(s) => (true, s),
                Err(e) => (false, e),
            });
        });
        Some(token)
    }

    fn selected_entry(&self) -> Option<Entry> {
        match &self.sel {
            Some(Sel::Entry(id)) => self.scan.entries.iter().find(|e| &e.id == id).cloned(),
            _ => None,
        }
    }
}

// ---- pure helpers (unit-tested) -----------------------------------------------------

fn keep(f: Filter, e: &Entry, managed: bool) -> bool {
    match f {
        Filter::All => true,
        Filter::Enabled => e.state() == State::Enabled,
        Filter::Disabled => e.state() == State::Disabled,
        Filter::Managed => managed,
        Filter::AllUsers => e.id.scope == Scope::AllUsers,
    }
}

fn state_word(s: State) -> &'static str {
    match s {
        State::Enabled => "Enabled",
        State::Disabled => "Disabled",
        State::Unknown => "Unknown",
    }
}

fn sort_entries(rows: &mut [(&Entry, Option<&MApp>)], key: SortKey, asc: bool) {
    rows.sort_by(|(a, am), (b, bm)| {
        let name = |e: &Entry| e.display().to_lowercase();
        let ord = match key {
            SortKey::Name => name(a).cmp(&name(b)),
            SortKey::Status => state_word(a.state()).cmp(state_word(b.state())),
            SortKey::Scope => (a.id.scope == Scope::AllUsers).cmp(&(b.id.scope == Scope::AllUsers)),
            SortKey::Managed => am.is_some().cmp(&bm.is_some()).reverse(),
        };
        // stable and total: ties fall back to name, then to the identity text
        let ord = ord.then_with(|| name(a).cmp(&name(b))).then_with(|| a.id.encode().cmp(&b.id.encode()));
        if asc {
            ord
        } else {
            ord.reverse()
        }
    });
}

fn parse_delay(s: &str) -> Result<u32, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(0);
    }
    if !t.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err("Delay must be a number of seconds.".into());
    }
    let v: f64 = t.parse().map_err(|_| "Delay must be a number of seconds.".to_string())?;
    if !v.is_finite() || !(0.0..=3600.0).contains(&v) {
        return Err("Delay must be 0 to 3600 seconds.".into());
    }
    Ok((v * 1000.0).round() as u32)
}

fn delay_text(ms: u32) -> String {
    if ms == 0 {
        String::new()
    } else if ms % 1000 == 0 {
        (ms / 1000).to_string()
    } else {
        format!("{:.1}", ms as f32 / 1000.0)
    }
}

fn cpu_step(cur: Option<u8>, up: bool) -> Option<u8> {
    match (cur, up) {
        (None, true) => Some(30),
        (None, false) => None,
        (Some(p), true) => Some((p + 5).min(95)),
        (Some(p), false) if p <= 5 => None,
        (Some(p), false) => Some(p - 5),
    }
}

/// Next/previous choice for After among `others` (None = nothing).
fn after_step(cur: Option<&str>, others: &[String], forward: bool) -> Option<String> {
    let mut cycle: Vec<Option<&str>> = vec![None];
    cycle.extend(others.iter().map(|s| Some(s.as_str())));
    let i = cycle.iter().position(|c| *c == cur).unwrap_or(0);
    let n = cycle.len();
    cycle[(i + if forward { 1 } else { n - 1 }) % n].map(str::to_string)
}

fn clamp_text(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

fn quote_path(p: &str) -> String {
    if p.contains(' ') && !p.starts_with('"') {
        format!("\"{p}\"")
    } else {
        p.to_string()
    }
}

impl Editor {
    fn blank() -> Editor {
        Editor { app_id: None, adopt: None, name: String::new(), command: String::new(), delay: String::new(), process: String::new(), after: None, cpu: None, admin: false, enabled: true, focus: Some(Field::Name), err: None, saving: false, token: 0, scroll: 0.0 }
    }

    fn from_app(a: &MApp) -> Editor {
        Editor { app_id: Some(a.id.clone()), name: a.name.clone(), command: a.command.clone(), delay: delay_text(a.delay_ms), process: a.process.clone().unwrap_or_default(), after: a.after.clone(), cpu: a.cpu_below, admin: a.admin, enabled: a.enabled, focus: None, ..Editor::blank() }
    }

    fn from_entry(e: &Entry, plan: adopt::Plan) -> Editor {
        let command = if e.id.kind == Kind::Folder { quote_path(&e.raw) } else { e.raw.clone() };
        Editor { name: clamp_text(&e.display(), model::MAX_NAME), command: clamp_text(&command, model::MAX_COMMAND), adopt: Some((e.clone(), plan)), focus: None, ..Editor::blank() }
    }

    fn to_app(&self) -> Result<MApp, String> {
        let mut a = MApp::new(self.app_id.clone().unwrap_or_else(|| "preview".into()), self.name.trim().to_string(), self.command.trim().to_string());
        a.delay_ms = parse_delay(&self.delay)?;
        a.after = self.after.clone();
        a.cpu_below = self.cpu;
        a.admin = self.admin;
        a.enabled = self.enabled;
        let p = self.process.trim();
        a.process = (!p.is_empty()).then(|| p.to_string());
        match model::field_problem(&a) {
            Some(p) => Err(first_upper(&p)),
            None => Ok(a),
        }
    }

    fn field_mut(&mut self, f: Field) -> (&mut String, usize) {
        match f {
            Field::Name => (&mut self.name, model::MAX_NAME),
            Field::Command => (&mut self.command, model::MAX_COMMAND),
            Field::Delay => (&mut self.delay, 8),
            Field::Process => (&mut self.process, model::MAX_PROCESS),
        }
    }
}

fn first_upper(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str() + if s.ends_with('.') { "" } else { "." },
        None => String::new(),
    }
}

/// What the typed text may contain for this field.
fn accepts(f: Field, c: char) -> bool {
    match f {
        Field::Delay => c.is_ascii_digit() || c == '.',
        _ => !c.is_control(),
    }
}

// ---- painting --------------------------------------------------------------------------------

const DET_H: f32 = 132.0;
const ROW_H: f32 = 28.0;

pub fn paint(app: &mut App, g: &mut Gfx, body: Rect) {
    if !app.startup.loaded {
        app.startup.load();
    }
    app.startup.take();
    let snap = mgr().map(|m| m.store.snapshot());
    let cfg: Config = snap.as_ref().map(|s| s.cfg.clone()).unwrap_or_default();
    let runv = mgr().map(|m| m.run.view()).unwrap_or_default();
    if runv.active {
        // the runner changes state on its own thread: look again soon
        app.timer_ms = Some(500);
    }
    let flight = mgr().map(|m| m.run.in_flight()).unwrap_or_default();
    let scan = app.startup.scan.clone();
    let area = body.inset(12.0, 10.0);

    // ---- title line
    let n = scan.entries.len();
    let off = scan.entries.iter().filter(|e| e.state() != State::Enabled).count();
    let managed = cfg.apps.iter().filter(|a| a.own.is_some()).count();
    g.text("STARTUP", Rect::new(area.x, area.y, 70.0, 20.0), f(11.0).b(), TX2);
    let head = format!("{n} {} · {off} not enabled · {managed} managed by Hayate", if n == 1 { "entry" } else { "entries" });
    g.text(&head, Rect::new(area.x + 70.0, area.y, (area.w - 70.0).max(40.0), 20.0), f(11.5), DIM);

    // ---- toolbar
    let mut y = area.y + 26.0;
    let mut x = area.x;
    for (m, label, count) in [(Mode::Windows, "Windows entries", n), (Mode::List, "Hayate list", cfg.apps.len())] {
        let id = if m == Mode::Windows { SEG_WIN } else { SEG_LIST };
        let on = app.startup.mode == m;
        let hov = app.hover == Some(Hit::Startup(id));
        let r = chip(&mut app.startup.wd, g, x, y, label, Some(count), on, hov);
        app.hits.push((r, Hit::Startup(id)));
        x = r.r() + 6.0;
    }
    x += 10.0;
    if app.startup.mode == Mode::Windows {
        for (i, (fl, label)) in FILTERS.iter().enumerate() {
            let on = app.startup.filter == *fl;
            let hov = app.hover == Some(Hit::Startup(FILTER0 + i));
            let w = chip_w(&mut app.startup.wd, g, label, None);
            if x + w > area.r() - 80.0 {
                break;
            }
            let r = chip(&mut app.startup.wd, g, x, y, label, None, on, hov);
            app.hits.push((r, Hit::Startup(FILTER0 + i)));
            x = r.r() + 6.0;
        }
    }
    // right side buttons (laid out right to left)
    let mut rx = area.r();
    let btn = |app: &mut App, g: &mut Gfx, rx: &mut f32, id: usize, label: &str, enabled: bool, tip: &str| {
        let w = g.measure(label, f(12.0)) + 24.0;
        let r = Rect::new(*rx - w, y, w, 24.0);
        let hov = app.hover == Some(Hit::Startup(id));
        button(g, r, label, hov, enabled);
        if enabled {
            app.hits.push((r, Hit::Startup(id)));
        }
        if !tip.is_empty() && app.hovering(Hit::Startup(id)) {
            app.set_tip(r, vec![label.to_string(), tip.to_string()]);
        }
        *rx = r.x - 6.0;
    };
    btn(app, g, &mut rx, REFRESH, "Refresh", !app.startup.busy.load(Ordering::SeqCst), "Read every startup source again.");
    if app.startup.mode == Mode::List {
        if runv.active {
            btn(app, g, &mut rx, CANCEL, "Cancel run", true, "Stops what has not started yet. A program already handed to Windows cannot be recalled and may still start.");
        } else {
            btn(app, g, &mut rx, RUN_ALL, "Run all now", flight.is_empty(), "Starts every enabled entry now, in order, with their delays, Admin entries included. Separate from the sign-in run, which skips Admin entries.");
        }
        btn(app, g, &mut rx, IMPORT, "Import Launchpad…", true, "Read a Launchpad config.toml you choose and preview it. Nothing changes until you select entries and import.");
        btn(app, g, &mut rx, ADD, "Add app", true, "Add a program, shortcut or shell:AppsFolder item to the list.");
    }
    y += 32.0;

    // ---- message line (success only after it happened)
    if let Some((ok, m)) = app.startup.msg.clone() {
        let r = Rect::new(area.x, y, area.w, 18.0);
        g.text(&m, r, f(11.5), if ok { OK_TX } else { HOT_TX });
        if app.hover.is_none() {
            // full text on hover is handled by the tip below
        }
        y += 20.0;
    }
    if let Some(s) = snap.as_ref().filter(|s| !s.dropped.is_empty()) {
        // data that could not be read is NOT silently saved over: say so before anything is saved again
        let r = Rect::new(area.x, y, area.w, 18.0);
        let t = format!("{} entr{} in startup.ini could not be read and will be dropped the next time the list is saved (the original text is kept in startup.ini.bak): {}", s.dropped.len(), if s.dropped.len() == 1 { "y" } else { "ies" }, s.dropped.join("; "));
        g.text(&t, r, f(11.5), HOT_TX);
        y += 20.0;
    }
    if !flight.is_empty() {
        let r = Rect::new(area.x, y, area.w, 18.0);
        let t = format!("Handed to Windows, no answer yet: {}. It may still start. Run now and Run all now wait until Windows answers.", flight.join(", "));
        g.text(&t, r, f(11.5), WARN_TX);
        y += 20.0;
    }
    if !scan.problems.is_empty() {
        let r = Rect::new(area.x, y, area.w, 18.0);
        let t = format!("{} source{} could not be read: {}", scan.problems.len(), if scan.problems.len() == 1 { "" } else { "s" }, scan.problems.join("; "));
        g.text(&t, r, f(11.5), WARN_TX);
        y += 20.0;
    }

    // ---- manager panel (list view)
    if app.startup.mode == Mode::List {
        y = manager_panel(app, g, area, y, &cfg, snap.as_ref());
    }

    // ---- details at the bottom, table in between
    let det_h = DET_H.min((area.b() - y) * 0.45).max(0.0);
    let det = Rect::new(area.x, area.b() - det_h, area.w, det_h);
    let table = Rect::new(area.x, y, area.w, (det.y - 8.0 - y).max(0.0));
    match app.startup.mode {
        Mode::Windows => windows_table(app, g, table, &scan, &cfg),
        Mode::List => list_table(app, g, table, &cfg, &runv),
    }
    if det_h > 60.0 {
        details(app, g, det, &scan, &cfg, &runv);
    }
}

fn manager_panel(app: &mut App, g: &mut Gfx, area: Rect, y: f32, cfg: &Config, snap: Option<&crate::startup::store::Snapshot>) -> f32 {
    use crate::startup::own::{self, Signin};
    // what the shortcut looks like (the last scan; no I/O here) and what that means for the switch
    let sig = {
        let user_dir = mgr().map(|m| m.loc.user_dir.clone()).unwrap_or_default();
        let shortcut = own::from_scan(&app.startup.scan.entries, &app.startup.scan.problems, &user_dir);
        let installed = crate::setup::per_user_installed();
        own::signin(cfg.manage, app.startup.want_signin, app.startup.scanned, shortcut.as_ref(), crate::osutil::elevation(), installed)
    };
    let iw0 = area.w - 24.0;
    // the inline offer (below the Start with Windows row): wrapped first, the panel grows to fit it
    let offer = match &sig {
        Signin::Blocked { offer: Some(o), .. } => {
            let bw = o.button.map(|l| g.measure(l, f(12.0)) + 24.0).unwrap_or(0.0);
            let lines = wrap(g, o.text, (iw0 - bw - 12.0).max(80.0), f(11.5), 3);
            Some((o.clone(), bw, lines))
        }
        _ => None,
    };
    let extra = offer.as_ref().map(|(_, _, l)| (l.len() as f32 * 16.0).max(24.0) + 8.0).unwrap_or(0.0);
    let r = Rect::new(area.x, y, area.w, 86.0 + extra);
    panel(g, r);
    let x = r.x + 12.0;
    let iw = r.w - 24.0;
    let on = cfg.manage;
    // switch
    let sw = Rect::new(x, r.y + 10.0, 150.0, 24.0);
    let hov = app.hover == Some(Hit::Startup(MANAGE));
    g.round(sw, 12.0, if on { ACC_BG } else if hov { PAN3 } else { PAN2 });
    g.round_stroke(sw, 12.0, if on { ACC_LN } else { LN }, 1.0);
    g.circle(sw.x + 12.0, sw.y + 12.0, 5.0, if on { ACC } else { TX4 });
    g.text(if on { "Run at sign-in: ON" } else { "Run at sign-in: OFF" }, Rect::new(sw.x + 24.0, sw.y, sw.w - 28.0, sw.h), f(12.0).b(), if on { ACC_TX } else { TX2 });
    let can = snap.is_some_and(|s| s.loaded && s.locked.is_none());
    if can {
        app.hits.push((sw, Hit::Startup(MANAGE)));
    }
    let locked = snap.and_then(|s| s.locked.clone());
    let note = mgr().map(|m| m.note()).unwrap_or_default();
    let tx = sw.r() + 14.0;
    // status line: the list cannot run -> say so and why (plain words); ready -> when it runs
    let (text, tint) = match (locked, &sig) {
        (Some(l), _) => (format!("The list cannot be changed: {l}"), TX2),
        (None, Signin::Blocked { why, .. }) if on => (format!("The list will not run at sign-in: {why}"), WARN_TX),
        // ready: say when it runs; what happened at THIS start (the runtime note) follows it
        (None, Signin::Ready) => (if note.is_empty() { own::READY_TEXT.to_string() } else { format!("{} {note}", own::READY_TEXT) }, TX2),
        (None, _) if !note.is_empty() => (note, TX2),
        (None, _) => ("Off by default. When on, Hayate starts the enabled entries once per sign-in, only if Hayate itself starts hidden at sign-in. Opening this tab, restarting as administrator or updating never runs it.".to_string(), TX2),
    };
    for (i, l) in wrap(g, &text, (r.r() - 12.0 - tx).max(60.0), f(11.5), 2).iter().enumerate() {
        g.text(l, Rect::new(tx, r.y + 7.0 + i as f32 * 16.0, r.r() - 12.0 - tx, 16.0), f(11.5), tint);
    }
    // Start with Windows (Hayate's own entry)
    let own_entry = app.startup.scan.entries.iter().find(|e| e.is_hayate && e.id.scope == Scope::User).cloned();
    let (line, can_toggle, label) = match &own_entry {
        Some(e) => match e.state() {
            State::Enabled => ("Start with Windows: on. Hayate's own Startup entry is enabled.".to_string(), true, "Turn off"),
            State::Disabled => ("Start with Windows: off. Without it the list cannot run at sign-in.".to_string(), true, "Turn on"),
            State::Unknown => ("Start with Windows: its setting is in a form Hayate does not change.".to_string(), false, ""),
        },
        None => ("Start with Windows: off. Hayate has no Startup entry yet.".to_string(), false, ""),
    };
    let ly = r.y + 46.0;
    let bw = if can_toggle { g.measure(label, f(12.0)) + 24.0 } else { 0.0 };
    g.text(&line, Rect::new(x, ly, iw - bw - 8.0, 22.0), f(11.5), TX3);
    if can_toggle {
        let br = Rect::new(r.r() - 12.0 - bw, ly - 1.0, bw, 24.0);
        button(g, br, label, app.hover == Some(Hit::Startup(AUTOSTART)), true);
        app.hits.push((br, Hit::Startup(AUTOSTART)));
    }
    // the offer: one button, nothing happens until it is pressed
    if let Some((o, bw, lines)) = offer {
        let oy = r.y + 76.0;
        for (i, l) in lines.iter().enumerate() {
            g.text(l, Rect::new(x, oy + i as f32 * 16.0, (iw - bw - 12.0).max(80.0), 16.0), f(11.5), WARN_TX);
        }
        if let Some(label) = o.button {
            let br = Rect::new(r.r() - 12.0 - bw, oy - 2.0, bw, 24.0);
            let hov = app.hover == Some(Hit::Startup(SIGNIN_FIX));
            let busy = app.startup.busy.load(Ordering::SeqCst);
            button(g, br, label, hov, !busy);
            if !busy {
                app.hits.push((br, Hit::Startup(SIGNIN_FIX)));
            }
            if app.hovering(Hit::Startup(SIGNIN_FIX)) {
                let tip = if cfg.manage { o.tip.to_string() } else { format!("{} Run at sign-in stays off until you switch it on.", o.tip) };
                app.set_tip(br, vec![label.to_string(), tip]);
            }
        }
    }
    r.b() + 8.0
}

fn header_cell(app: &mut App, g: &mut Gfx, id: Option<usize>, label: &str, x: f32, y: f32, w: f32, active: Option<bool>) {
    let r = Rect::new(x, y, w, 24.0);
    let t = match active {
        Some(true) => format!("{label} ↑"),
        Some(false) => format!("{label} ↓"),
        None => label.to_string(),
    };
    let hov = id.is_some_and(|i| app.hover == Some(Hit::Startup(i)));
    g.text(&t, Rect::new(x + 6.0, y, w - 8.0, 24.0), f(11.5), if active.is_some() || hov { TX2 } else { DIM });
    if let Some(i) = id {
        app.hits.push((r, Hit::Startup(i)));
    }
}

fn windows_table(app: &mut App, g: &mut Gfx, table: Rect, scan: &Scan, cfg: &Config) {
    if table.h < 50.0 {
        return;
    }
    let (key, asc) = app.startup.sort;
    let mut rows: Vec<(&Entry, Option<&MApp>)> = scan.entries.iter().map(|e| (e, cfg.owner_of(&e.id))).filter(|(e, o)| keep(app.startup.filter, e, o.is_some())).collect();
    sort_entries(&mut rows, key, asc);

    // the scroll bar takes its gutter from the rows only while the list overflows
    let list_content = rows.len() as f32 * ROW_H;
    let tw = table.w - super::nav::gutter(list_content, table.h - 24.0);
    let w_status = 84.0;
    let w_scope = 84.0;
    let w_man = if tw > 760.0 { 130.0 } else { 96.0 };
    let flex = (tw - w_status - w_scope - w_man).max(120.0);
    let w_name = (flex * 0.36).max(110.0).min(flex - 60.0);
    let w_cmd = (flex - w_name).max(40.0);
    let mut x = table.x;
    for (i, (label, w, sk)) in [("Name", w_name, Some(SortKey::Name)), ("Status", w_status, Some(SortKey::Status)), ("Scope", w_scope, Some(SortKey::Scope)), ("Managed", w_man, Some(SortKey::Managed)), ("Command", w_cmd, None)].into_iter().enumerate() {
        let act = sk.filter(|s| *s == key).map(|_| asc);
        header_cell(app, g, sk.map(|_| HDR0 + i), label, x, table.y, w, act);
        x += w;
    }
    g.fill(Rect::new(table.x, table.y + 23.0, table.w, 1.0), LN);
    app.startup.cols = vec![(Cell::Status, table.x + w_name, table.x + w_name + w_status)];
    let list = Rect::new(table.x, table.y + 24.0, table.w, table.h - 24.0);
    app.startup.order = rows.iter().map(|(e, _)| Sel::Entry(e.id.clone())).collect();
    seen_at(app, list.h);
    if rows.is_empty() {
        let t = if scan.entries.is_empty() { "No startup entries found for this session." } else { "No entry matches this filter." };
        g.text(t, Rect::new(list.x + 6.0, list.y + 12.0, list.w, 20.0), f(12.0), DIM);
        return;
    }
    let max_scroll = (rows.len() as f32 * ROW_H - list.h).max(0.0);
    app.startup.scroll = app.startup.scroll.clamp(0.0, max_scroll);
    g.clip(list);
    for (i, (e, owner)) in rows.iter().enumerate() {
        let ry = list.y + i as f32 * ROW_H - app.startup.scroll;
        if ry + ROW_H < list.y || ry > list.b() {
            continue;
        }
        let rr = Rect::new(table.x, ry, tw, ROW_H);
        let sel = app.startup.sel == Some(Sel::Entry(e.id.clone()));
        let hit = Hit::Startup(ROW + i);
        if sel {
            g.fill(rr, ACC_BG);
            rail(g, rr, ACC);
        } else if app.hover == Some(hit) {
            g.fill(rr, PAN2);
        }
        g.fill(Rect::new(rr.x, rr.b() - 1.0, rr.w, 1.0), SEP);
        let ir = Rect::new(rr.x + 8.0, rr.y + 6.0, 16.0, 16.0);
        if !super::icons::draw(&mut app.icons, g, &e.icon, 0, ir) {
            g.round(Rect::new(ir.x + 2.0, ir.y + 2.0, 12.0, 12.0), 3.0, PAN2);
        }
        let mut x = rr.x;
        g.text(&e.display(), Rect::new(x + 30.0, rr.y, w_name - 34.0, rr.h), f(12.5), TX1);
        x += w_name;
        let st = e.state();
        g.text(state_word(st), Rect::new(x + 6.0, rr.y, w_status - 8.0, rr.h), f(11.5), if st == State::Enabled { TX1 } else { WARN_TX });
        x += w_status;
        g.text(e.id.scope_label(), Rect::new(x + 6.0, rr.y, w_scope - 8.0, rr.h), f(11.5), TX3);
        x += w_scope;
        match owner {
            Some(a) => g.text(&a.name, Rect::new(x + 6.0, rr.y, w_man - 8.0, rr.h), f(11.5), ACC_TX),
            None => g.text("—", Rect::new(x + 6.0, rr.y, w_man - 8.0, rr.h), f(11.5), TX4),
        }
        x += w_man;
        g.text(&e.shown_command(), Rect::new(x + 6.0, rr.y, w_cmd - 10.0, rr.h), f(11.5).m(), TX3);
        app.hits.push((Rect::new(rr.x, rr.y.max(list.y), rr.w, (rr.b().min(list.b()) - rr.y.max(list.y)).max(0.0)), hit));
        if app.hovering(hit) {
            let mut p = vec![e.display(), e.shown_command(), format!("{} · {}", e.id.source_label(), e.id.key)];
            if let Some(a) = owner {
                p.push(format!("Managed by Hayate as \"{}\".", a.name));
            }
            if cell_at(&app.startup.cols, app.mx) == Some(Cell::Status) {
                p.push(status_hint(&toggle_of(e, owner.is_some())));
            }
            app.set_tip(rr, p);
        }
    }
    g.unclip();
    let sc = app.startup.scroll;
    super::nav::paint_bar(app, g, super::nav::BAR_STARTUP, list, list_content, sc);
}

fn run_row<'a>(runv: &'a run::RunView, id: &str) -> Option<&'a run::RunRow> {
    runv.rows.iter().find(|r| r.id == id)
}

fn list_table(app: &mut App, g: &mut Gfx, table: Rect, cfg: &Config, runv: &run::RunView) {
    if table.h < 50.0 {
        return;
    }
    // the scroll bar takes its gutter from the rows only while the list overflows
    let list_content = cfg.apps.len() as f32 * ROW_H;
    let tw = table.w - super::nav::gutter(list_content, table.h - 24.0);
    let w_state = 74.0;
    let w_delay = 60.0;
    let w_after = 110.0;
    let w_last = if tw > 760.0 { 170.0 } else { 120.0 };
    let flex = (tw - w_state - w_delay - w_after - w_last).max(120.0);
    let w_name = (flex * 0.36).max(110.0).min(flex - 60.0);
    let w_cmd = (flex - w_name).max(40.0);
    let mut x = table.x;
    for (label, w) in [("Name", w_name), ("State", w_state), ("Delay", w_delay), ("After", w_after), ("Last run", w_last), ("Command", w_cmd)] {
        header_cell(app, g, None, label, x, table.y, w, None);
        x += w;
    }
    g.fill(Rect::new(table.x, table.y + 23.0, table.w, 1.0), LN);
    app.startup.cols = vec![(Cell::AppState, table.x + w_name, table.x + w_name + w_state), (Cell::Delay, table.x + w_name + w_state, table.x + w_name + w_state + w_delay), (Cell::After, table.x + w_name + w_state + w_delay, table.x + w_name + w_state + w_delay + w_after)];
    let list = Rect::new(table.x, table.y + 24.0, table.w, table.h - 24.0);
    app.startup.order = cfg.apps.iter().map(|a| Sel::App(a.id.clone())).collect();
    seen_at(app, list.h);
    if cfg.apps.is_empty() {
        g.text("Nothing in the Hayate list yet. Add an app, import a Launchpad config, or take over a Windows entry (Manage…).", Rect::new(list.x + 6.0, list.y + 12.0, list.w - 12.0, 20.0), f(12.0), DIM);
        return;
    }
    let max_scroll = (cfg.apps.len() as f32 * ROW_H - list.h).max(0.0);
    app.startup.scroll = app.startup.scroll.clamp(0.0, max_scroll);
    g.clip(list);
    for (i, a) in cfg.apps.iter().enumerate() {
        let ry = list.y + i as f32 * ROW_H - app.startup.scroll;
        if ry + ROW_H < list.y || ry > list.b() {
            continue;
        }
        let rr = Rect::new(table.x, ry, tw, ROW_H);
        let sel = app.startup.sel == Some(Sel::App(a.id.clone()));
        let hit = Hit::Startup(ROW + i);
        if sel {
            g.fill(rr, ACC_BG);
            rail(g, rr, ACC);
        } else if app.hover == Some(hit) {
            g.fill(rr, PAN2);
        }
        g.fill(Rect::new(rr.x, rr.b() - 1.0, rr.w, 1.0), SEP);
        let ir = Rect::new(rr.x + 8.0, rr.y + 6.0, 16.0, 16.0);
        let icon = crate::startup::launch::icon_source(&a.command).unwrap_or_default();
        if !super::icons::draw(&mut app.icons, g, &icon, 0, ir) {
            g.round(Rect::new(ir.x + 2.0, ir.y + 2.0, 12.0, 12.0), 3.0, PAN2);
        }
        let mut x = rr.x;
        g.text(&a.name, Rect::new(x + 30.0, rr.y, w_name - 34.0, rr.h), f(12.5), if a.enabled { TX1 } else { TX3 });
        x += w_name;
        g.text(if a.enabled { "On" } else { "Off" }, Rect::new(x + 6.0, rr.y, w_state - 8.0, rr.h), f(11.5), if a.enabled { TX1 } else { WARN_TX });
        x += w_state;
        g.text(&if a.delay_ms == 0 { "—".to_string() } else { format!("{} s", delay_text(a.delay_ms)) }, Rect::new(x + 6.0, rr.y, w_delay - 8.0, rr.h), f(11.5), TX3);
        let delay_x = x;
        x += w_delay;
        let after = a.after.as_deref().and_then(|id| cfg.app(id)).map(|d| d.name.clone()).unwrap_or_else(|| "—".into());
        g.text(&after, Rect::new(x + 6.0, rr.y, w_after - 8.0, rr.h), f(11.5), TX3);
        x += w_after;
        let (last, col) = match run_row(runv, &a.id) {
            Some(r) => (r.status.clone(), match r.ok {
                Some(true) => OK_TX,
                Some(false) => HOT_TX,
                None => TX2,
            }),
            None => ("—".to_string(), TX4),
        };
        g.text(&last, Rect::new(x + 6.0, rr.y, w_last - 8.0, rr.h), f(11.5), col);
        x += w_last;
        g.text(&a.command, Rect::new(x + 6.0, rr.y, w_cmd - 10.0, rr.h), f(11.5).m(), TX3);
        app.hits.push((Rect::new(rr.x, rr.y.max(list.y), rr.w, (rr.b().min(list.b()) - rr.y.max(list.y)).max(0.0)), hit));
        let editing = app.startup.inline.as_ref().is_some_and(|i| i.app_id == a.id);
        if editing {
            let (text, err) = app.startup.inline.as_ref().map(|i| (i.text.clone(), i.err.clone())).unwrap_or_default();
            let fr = Rect::new(delay_x + 2.0, rr.y + 2.0, w_delay - 4.0, rr.h - 4.0);
            text_field(app, g, fr, INLINE_FIELD, &text, true, "0", true);
            let mut p = vec!["Seconds, 0 to 3600. Enter saves, Esc cancels.".to_string()];
            if let Some(e) = err {
                p.insert(0, e);
            }
            app.set_tip(fr, p);
        } else if app.hovering(hit) {
            let mut p = vec![a.name.clone(), a.command.clone()];
            if let Some(r) = run_row(runv, &a.id) {
                p.push(format!("Last run: {}", r.status));
            }
            if let Some(h) = cell_at(&app.startup.cols, app.mx).and_then(|c| app_hint(c, a)) {
                p.push(h);
            }
            app.set_tip(rr, p);
        }
    }
    g.unclip();
    let sc = app.startup.scroll;
    super::nav::paint_bar(app, g, super::nav::BAR_STARTUP, list, list_content, sc);
}

/// A row of buttons laid out left to right at (x, y); returns nothing, pushes hits.
fn buttons(app: &mut App, g: &mut Gfx, x: f32, y: f32, max_x: f32, list: &[(usize, String, bool, bool)]) {
    let mut bx = x;
    for (id, label, enabled, danger) in list {
        let w = g.measure(label, f(12.0).b()) + 26.0;
        if bx + w > max_x {
            break;
        }
        let r = Rect::new(bx, y, w, 26.0);
        let hov = app.hover == Some(Hit::Startup(*id));
        if *danger && *enabled {
            g.round(r, 6.0, if hov { HOT_BG } else { PAN2 });
            g.round_stroke(r, 6.0, HOT_LN, 1.0);
            g.text(label, r, f(12.0).c(), HOT_TX);
        } else {
            button(g, r, label, hov, *enabled);
        }
        if *enabled {
            app.hits.push((r, Hit::Startup(*id)));
        }
        bx += w + 8.0;
    }
}

fn details(app: &mut App, g: &mut Gfx, r: Rect, scan: &Scan, cfg: &Config, runv: &run::RunView) {
    panel(g, r);
    let x = r.x + 12.0;
    let iw = r.w - 24.0;
    let mut y = r.y + 8.0;
    let flight_busy = mgr().is_some_and(|m| !m.run.in_flight().is_empty());
    // anything but a proven normal token counts as elevated (unknown fails closed)
    let elevated = crate::osutil::elevation() != Some(false);
    let confirm_for = |app: &App, id: usize| app.startup.confirm.as_ref().is_some_and(|(c, s)| *c == id && *s == app.startup.sel);
    match (app.startup.mode, app.startup.sel.clone()) {
        (Mode::Windows, Some(Sel::Entry(id))) => {
            let Some(e) = scan.entries.iter().find(|e| e.id == id).cloned() else {
                g.text("That entry is gone. Refresh the list.", Rect::new(x, y, iw, 20.0), f(12.0), TX3);
                return;
            };
            let owner = cfg.owner_of(&e.id).cloned();
            g.text(&e.display(), Rect::new(x, y, iw, 20.0), f(13.0).b(), TX1);
            y += 22.0;
            let mut meta = format!("{} · {} · {}", e.id.source_label(), e.id.key, state_word(e.state()));
            if e.id.scope == Scope::AllUsers && e.live.user_override.is_some() {
                meta.push_str(" · this user also has its own setting");
            }
            g.text(&meta, Rect::new(x, y, iw, 16.0), f(11.5), TX3);
            y += 18.0;
            let cmd = e.shown_command();
            for (i, l) in wrap_chars(g, &cmd, iw, f(11.5).m(), 2).iter().enumerate() {
                g.text(l, Rect::new(x, y + i as f32 * 16.0, iw, 16.0), f(11.5).m(), TX2);
            }
            y += 34.0;
            let mut b: Vec<(usize, String, bool, bool)> = Vec::new();
            match &owner {
                Some(a) => {
                    g.text(&format!("Managed by Hayate as \"{}\"; the original is {}.", a.name, state_word(e.state()).to_lowercase()), Rect::new(x, y - 2.0, iw, 16.0), f(11.5), ACC_TX);
                    b.push((D_OPENLIST, "Open in Hayate list".into(), true, false));
                }
                None => {
                    let enabled = e.state() == State::Enabled;
                    let can = matches!(toggle_of(&e, false), Toggle::Go(_)) && !e.is_hayate_blocked();
                    b.push((D_TOGGLE, if enabled { "Disable".into() } else { "Enable".into() }, can, false));
                    b.push((D_MANAGE, "Manage in Hayate…".into(), !e.is_hayate, false));
                    if e.id.machine() && !elevated {
                        g.text("Changing an all-users entry needs administrator rights.", Rect::new(x, y - 2.0, iw, 16.0), f(11.5), WARN_TX);
                        b.push((D_ADMIN, "Restart as administrator".into(), true, false));
                    }
                }
            }
            b.push((D_COPY, "Copy command".into(), true, false));
            buttons(app, g, x, r.b() - 36.0, r.r() - 12.0, &b);
        }
        (Mode::List, Some(Sel::App(id))) => {
            let Some(a) = cfg.app(&id).cloned() else {
                g.text("Select an entry to edit it, run it now, or turn it on or off.", Rect::new(x, y, iw, 20.0), f(12.0), TX3);
                return;
            };
            g.text(&a.name, Rect::new(x, y, iw, 20.0), f(13.0).b(), TX1);
            y += 22.0;
            let mut bits: Vec<String> = vec![if a.enabled { "On".into() } else { "Off".into() }];
            if a.delay_ms > 0 {
                bits.push(format!("delay {} s", delay_text(a.delay_ms)));
            }
            if let Some(d) = a.after.as_deref().and_then(|d| cfg.app(d)) {
                bits.push(format!("after {}", d.name));
            }
            if let Some(c) = a.cpu_below {
                bits.push(format!("CPU below {c}%"));
            }
            if a.admin {
                bits.push("Admin (only on Run now)".into());
            }
            if let Some(p) = &a.process {
                bits.push(format!("skips if {p} runs"));
            }
            g.text(&bits.join(" · "), Rect::new(x, y, iw, 16.0), f(11.5), TX3);
            y += 18.0;
            for (i, l) in wrap_chars(g, &a.command, iw, f(11.5).m(), 2).iter().enumerate() {
                g.text(l, Rect::new(x, y + i as f32 * 16.0, iw, 16.0), f(11.5).m(), TX2);
            }
            y += 34.0;
            let mut line = match run_row(runv, &a.id) {
                Some(rr) => format!("Last run: {}", rr.status),
                None => String::new(),
            };
            if let Some(o) = &a.own {
                let pending = o.state == model::OwnState::Pending;
                line = format!("{}Takes over {}{}", if line.is_empty() { String::new() } else { format!("{line} · ") }, o.source.source_label(), if pending { " (take-over unfinished)" } else { "" });
            }
            g.text(&line, Rect::new(x, y - 2.0, iw, 16.0), f(11.5), TX3);
            let mut b = vec![(D_EDIT, "Edit".to_string(), true, false), (D_RUN, "Run now".to_string(), !runv.active && !flight_busy, false), (D_APP_ON, if a.enabled { "Turn off".to_string() } else { "Turn on".to_string() }, true, false)];
            if a.own.is_some() {
                b.push((D_UNMANAGE, if confirm_for(app, D_UNMANAGE) { "Click again: give back".into() } else { "Unmanage".into() }, true, false));
            }
            b.push((D_REMOVE, if confirm_for(app, D_REMOVE) { "Click again: remove".into() } else { "Remove".into() }, true, true));
            buttons(app, g, x, r.b() - 36.0, r.r() - 12.0, &b);
        }
        (Mode::List, None) if !runv.log.is_empty() => {
            g.text(&format!("{} · {}", if runv.label.is_empty() { "Last run" } else { &runv.label }, if runv.active { "running" } else { "finished" }), Rect::new(x, y, iw, 18.0), f(12.5).b(), TX2);
            y += 22.0;
            let max = ((r.b() - y - 8.0) / 15.0).floor().max(1.0) as usize;
            let start = runv.log.len().saturating_sub(max);
            for (i, l) in runv.log[start..].iter().enumerate() {
                g.text(l, Rect::new(x, y + i as f32 * 15.0, iw, 15.0), f(11.0).m(), TX3);
            }
        }
        (Mode::List, None) => {
            g.text("Select an entry to edit it, run it now, or turn it on or off.", Rect::new(x, y, iw, 20.0), f(12.0), TX3);
        }
        _ => {
            g.text("Select an entry to see where it lives and what it starts.", Rect::new(x, y, iw, 20.0), f(12.0), TX3);
        }
    }
}

trait HayateBlocked {
    fn is_hayate_blocked(&self) -> bool;
}
impl HayateBlocked for Entry {
    /// Hayate's own startup entry may be switched on or off like any other.
    fn is_hayate_blocked(&self) -> bool {
        false
    }
}

/// Break `s` into at most `max` lines of at most `w` pixels, splitting anywhere
/// (paths and commands have no spaces to wrap on).
fn wrap_chars(g: &mut Gfx, s: &str, w: f32, font: Font, max: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut pos = 0;
    while pos < chars.len() && out.len() < max {
        let mut lo = pos + 1;
        let mut hi = chars.len();
        // longest prefix that fits (at least one char)
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let t: String = chars[pos..mid].iter().collect();
            if g.measure(&t, font) <= w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let mut line: String = chars[pos..lo].iter().collect();
        pos = lo;
        if out.len() + 1 == max && pos < chars.len() {
            line.pop();
            line.push('…');
            pos = chars.len();
        }
        out.push(line);
    }
    out
}

// ---- overlays ----------------------------------------------------------------------------------

pub fn overlay(app: &mut App, g: &mut Gfx, body: Rect) {
    if app.tab != Tab::Startup {
        return;
    }
    if app.startup.editor.is_some() || app.startup.import.is_some() {
        // the pointer may still rest on a button under the dialog: its tip must not float over it
        app.tip = None;
    }
    if app.startup.editor.is_some() {
        editor_overlay(app, g, body);
    } else if app.startup.import.is_some() {
        import_overlay(app, g, body);
    }
}

fn text_field(app: &mut App, g: &mut Gfx, r: Rect, id: usize, text: &str, focus: bool, placeholder: &str, mono: bool) {
    g.round(r, 6.0, PAN2);
    g.round_stroke(r, 6.0, if focus { ACC } else { LN }, 1.0);
    let font = if mono { f(12.0).m() } else { f(12.0) };
    let inner = Rect::new(r.x + 8.0, r.y, r.w - 16.0, r.h);
    if text.is_empty() {
        g.text(placeholder, inner, font, TX4);
        if focus {
            g.text("▏", Rect::new(inner.x, inner.y, 8.0, inner.h), font, TX1);
        }
    } else {
        // the end of the text stays visible while typing
        let mut shown: String = text.to_string();
        if g.measure(&shown, font) > inner.w - 8.0 {
            let chars: Vec<char> = text.chars().collect();
            let mut start = 0;
            while start < chars.len() {
                let t: String = chars[start..].iter().collect();
                if g.measure(&t, font) <= inner.w - 8.0 {
                    break;
                }
                start += (chars.len() / 16).max(1);
            }
            shown = format!("…{}", chars[start.min(chars.len())..].iter().collect::<String>());
        }
        g.text(&format!("{shown}{}", if focus { "▏" } else { "" }), inner, font, TX1);
    }
    app.hits.push((r, Hit::Startup(id)));
}

fn check(app: &mut App, g: &mut Gfx, r: Rect, id: usize, on: bool, label: &str) {
    if app.hover == Some(Hit::Startup(id)) {
        g.round(r, 5.0, PAN2);
    }
    let b = Rect::new(r.x + 4.0, r.y + (r.h - 16.0) / 2.0, 16.0, 16.0);
    g.round(b, 4.0, if on { ACC } else { PAN2 });
    g.round_stroke(b, 4.0, if on { ACC } else { LN }, 1.0);
    if on {
        g.text("\u{E73E}", b, f(10.0).icon().c(), BG);
    }
    g.text(label, Rect::new(r.x + 28.0, r.y, r.w - 30.0, r.h), f(12.0), if on { TX1 } else { TX2 });
    app.hits.push((r, Hit::Startup(id)));
}

fn panel_frame(app: &mut App, g: &mut Gfx, body: Rect, want_w: f32, want_h: f32, backdrop: usize, panel_id: usize) -> Rect {
    let w = (body.w - 24.0).min(want_w).max(260.0);
    let h = want_h.min((body.h - 16.0).max(120.0));
    let r = Rect::new(body.x + (body.w - w) / 2.0, body.y + ((body.h - h) / 2.0).clamp(8.0, 60.0), w, h);
    let veil = body;
    g.fill_a(veil, BG, 0.6);
    app.hits.push((veil, Hit::Startup(backdrop)));
    g.round(r, 8.0, PAN);
    g.round_stroke(r, 8.0, LN, 1.0);
    app.hits.push((r, Hit::Startup(panel_id)));
    r
}

fn editor_overlay(app: &mut App, g: &mut Gfx, body: Rect) {
    let Some(mut ed) = app.startup.editor.take() else { return };
    let cfg = mgr().map(|m| m.store.snapshot().cfg).unwrap_or_default();
    let r = panel_frame(app, g, body, 640.0, 560.0, ED_BACKDROP, ED_PANEL);
    let x = r.x + 18.0;
    let iw = r.w - 36.0;
    let mut y = r.y + 14.0;
    let title = match (&ed.adopt, &ed.app_id) {
        (Some(_), _) => "Manage a Windows startup entry",
        (None, Some(_)) => "Edit startup app",
        (None, None) => "Add startup app",
    };
    g.text(title, Rect::new(x, y, iw - 80.0, 26.0), f(15.0).b(), TX1);
    let cr = Rect::new(r.r() - 18.0 - 62.0, y, 62.0, 26.0);
    button(g, cr, "Close", app.hover == Some(Hit::Startup(ED_CLOSE)), !ed.saving);
    if !ed.saving {
        app.hits.push((cr, Hit::Startup(ED_CLOSE)));
    }
    y += 34.0;

    // footer first (pinned): message + buttons
    let foot_h = 70.0;
    let foot_y = r.b() - foot_h;
    let vp = Rect::new(r.x + 1.0, y, r.w - 2.0, (foot_y - y - 4.0).max(40.0));
    let hit_start = app.hits.len();
    g.clip(vp);
    let mut cy = y - ed.scroll;
    let label_w = 112.0;
    let fx = x + label_w;
    let fw = iw - label_w;

    if let Some((_, plan)) = &ed.adopt {
        g.text("What Save will do", Rect::new(x, cy, iw, 18.0), f(12.0).b(), TX2);
        cy += 20.0;
        for l in &plan.lines {
            for (i, w) in wrap(g, l, iw, f(11.5), 3).iter().enumerate() {
                g.text(w, Rect::new(x, cy, iw, 15.0), f(11.5), TX3);
                cy += 15.0;
                let _ = i;
            }
            cy += 3.0;
        }
        if let Some(b) = &plan.blocked {
            for w in wrap(g, &format!("Cannot be taken over: {b}"), iw, f(11.5), 4) {
                g.text(&w, Rect::new(x, cy, iw, 15.0), f(11.5), HOT_TX);
                cy += 15.0;
            }
        }
        cy += 10.0;
    }

    let rows: [(&str, Field, usize, &str, bool); 2] = [("Name", Field::Name, ED_NAME, "Shown in the list", false), ("Command", Field::Command, ED_CMD, "C:\\path\\app.exe args · shortcut.lnk · shell:AppsFolder\\…", true)];
    for (label, fld, id, ph, mono) in rows {
        g.text(label, Rect::new(x, cy, label_w, 28.0), f(12.0), TX2);
        let text = match fld {
            Field::Name => ed.name.clone(),
            _ => ed.command.clone(),
        };
        text_field(app, g, Rect::new(fx, cy, fw, 28.0), id, &text, ed.focus == Some(fld), ph, mono);
        cy += 36.0;
    }
    g.text("Delay (seconds)", Rect::new(x, cy, label_w, 28.0), f(12.0), TX2);
    text_field(app, g, Rect::new(fx, cy, 90.0, 28.0), ED_DELAY, &ed.delay, ed.focus == Some(Field::Delay), "0", true);
    g.text("after the dependency started, or after sign-in", Rect::new(fx + 100.0, cy, fw - 100.0, 28.0), f(11.0), TX3);
    cy += 36.0;
    // After
    g.text("After", Rect::new(x, cy, label_w, 28.0), f(12.0), TX2);
    let others: Vec<String> = cfg.apps.iter().filter(|a| Some(&a.id) != ed.app_id.as_ref()).map(|a| a.id.clone()).collect();
    let after_name = ed.after.as_deref().and_then(|id| cfg.app(id)).map(|a| a.name.clone()).unwrap_or_else(|| if ed.after.is_some() { "(missing entry)".into() } else { "Nothing: start without waiting".into() });
    let pr = Rect::new(fx, cy, 30.0, 28.0);
    let nr = Rect::new(fx + fw - 30.0, cy, 30.0, 28.0);
    button(g, pr, "‹", app.hover == Some(Hit::Startup(ED_AFTER_P)), !others.is_empty());
    button(g, nr, "›", app.hover == Some(Hit::Startup(ED_AFTER_N)), !others.is_empty());
    if !others.is_empty() {
        app.hits.push((pr, Hit::Startup(ED_AFTER_P)));
        app.hits.push((nr, Hit::Startup(ED_AFTER_N)));
    }
    g.text(&after_name, Rect::new(fx + 38.0, cy, fw - 76.0, 28.0), f(12.0).c(), TX1);
    cy += 36.0;
    // CPU below
    g.text("Wait for CPU below", Rect::new(x, cy, label_w, 28.0), f(12.0), TX2);
    let mr = Rect::new(fx, cy, 30.0, 28.0);
    let pr2 = Rect::new(fx + 96.0, cy, 30.0, 28.0);
    button(g, mr, "−", app.hover == Some(Hit::Startup(ED_CPU_M)), ed.cpu.is_some());
    button(g, pr2, "+", app.hover == Some(Hit::Startup(ED_CPU_P)), true);
    if ed.cpu.is_some() {
        app.hits.push((mr, Hit::Startup(ED_CPU_M)));
    }
    app.hits.push((pr2, Hit::Startup(ED_CPU_P)));
    g.text(&ed.cpu.map(|c| format!("{c} %")).unwrap_or_else(|| "off".into()), Rect::new(fx + 34.0, cy, 58.0, 28.0), f(12.0).c(), TX1);
    g.text("steady for 3 s; starts anyway after 90 s", Rect::new(fx + 134.0, cy, fw - 134.0, 28.0), f(11.0), TX3);
    cy += 36.0;
    g.text("Process name", Rect::new(x, cy, label_w, 28.0), f(12.0), TX2);
    text_field(app, g, Rect::new(fx, cy, fw, 28.0), ED_PROC, &ed.process, ed.focus == Some(Field::Process), "app.exe: skip when already running (blank: taken from the command)", true);
    cy += 38.0;
    check(app, g, Rect::new(x, cy, iw, 26.0), ED_ADMIN, ed.admin, "Run as administrator (only when you press Run now; Windows decides whether it asks)");
    cy += 28.0;
    check(app, g, Rect::new(x, cy, iw, 26.0), ED_ENABLED, ed.enabled, "On: included when the list runs");
    cy += 30.0;
    if ed.admin {
        for w in wrap(g, "An administrator entry never starts by itself at sign-in: it waits for you to press Run now, and what depends on it waits too. Windows policy decides whether it asks for consent. It is not started from an elevated Hayate, and a program that is not marked administrator always starts with your normal user rights.", iw, f(11.0), 3) {
            g.text(&w, Rect::new(x, cy, iw, 15.0), f(11.0), WARN_TX);
            cy += 15.0;
        }
    }
    let content_h = cy + ed.scroll - y;
    g.unclip();
    let mut i = hit_start;
    while i < app.hits.len() {
        match visible_rect(app.hits[i].0, vp) {
            Some(v) => {
                app.hits[i].0 = v;
                i += 1;
            }
            None => {
                app.hits.remove(i);
            }
        }
    }
    let max_scroll = (content_h - vp.h).max(0.0);
    ed.scroll = ed.scroll.clamp(0.0, max_scroll);
    super::nav::paint_bar(app, g, super::nav::BAR_STARTUP_EDIT, vp, content_h, ed.scroll);

    // pinned footer
    let mut fy = foot_y;
    if let Some(e) = &ed.err {
        for w in wrap(g, e, iw, f(11.5), 2) {
            g.text(&w, Rect::new(x, fy, iw, 16.0), f(11.5), HOT_TX);
            fy += 16.0;
        }
    } else if ed.saving {
        g.text("Saving…", Rect::new(x, fy, iw, 16.0), f(11.5), TX2);
    }
    let by = r.b() - 44.0;
    let blocked = ed.adopt.as_ref().is_some_and(|(_, p)| p.blocked.is_some());
    let label = if ed.adopt.is_some() { "Save and manage" } else { "Save" };
    let bw = g.measure(label, f(12.0).b()) + 36.0;
    let sr = Rect::new(r.r() - 18.0 - bw, by, bw, 30.0);
    if ed.saving || blocked {
        button(g, sr, label, false, false);
    } else {
        accent(g, sr, label, app.hover == Some(Hit::Startup(ED_SAVE)), false);
        app.hits.push((sr, Hit::Startup(ED_SAVE)));
    }
    app.startup.editor = Some(ed);
}

fn import_overlay(app: &mut App, g: &mut Gfx, body: Rect) {
    let Some(mut im) = app.startup.import.take() else { return };
    let r = panel_frame(app, g, body, 720.0, 600.0, IM_BACKDROP, IM_PANEL);
    let x = r.x + 18.0;
    let iw = r.w - 36.0;
    let mut y = r.y + 14.0;
    g.text("Import from Launchpad", Rect::new(x, y, iw - 80.0, 26.0), f(15.0).b(), TX1);
    let cr = Rect::new(r.r() - 18.0 - 62.0, y, 62.0, 26.0);
    button(g, cr, "Close", app.hover == Some(Hit::Startup(IM_CLOSE)), !im.importing);
    if !im.importing {
        app.hits.push((cr, Hit::Startup(IM_CLOSE)));
    }
    y += 34.0;
    for w in wrap(g, "Choose the Launchpad config.toml to read. Hayate only reads it: nothing of Launchpad's is changed, started or turned off, and no Windows startup entry is touched. You review everything before anything is added.", iw, f(11.5), 3) {
        g.text(&w, Rect::new(x, y, iw, 15.0), f(11.5), TX3);
        y += 15.0;
    }
    y += 6.0;
    // path row
    let lw = g.measure("Load", f(12.0).b()) + 28.0;
    let fillw = g.measure("Usual place", f(12.0)) + 24.0;
    let pr = Rect::new(x, y, iw - lw - fillw - 16.0, 28.0);
    text_field(app, g, pr, IM_PATH, &im.path, im.focus, "Path to config.toml (paste with Ctrl+V)", true);
    let fr = Rect::new(pr.r() + 8.0, y, fillw, 28.0);
    button(g, fr, "Usual place", app.hover == Some(Hit::Startup(IM_FILL)), !im.loading && !im.importing);
    let lr = Rect::new(fr.r() + 8.0, y, lw, 28.0);
    let can_load = !im.path.trim().is_empty() && !im.loading && !im.importing;
    button(g, lr, "Load", app.hover == Some(Hit::Startup(IM_LOAD)), can_load);
    if !im.loading && !im.importing {
        app.hits.push((fr, Hit::Startup(IM_FILL)));
    }
    if can_load {
        app.hits.push((lr, Hit::Startup(IM_LOAD)));
    }
    y += 38.0;
    let foot_h = 54.0;
    let foot_y = r.b() - foot_h;
    let vp = Rect::new(r.x + 1.0, y, r.w - 2.0, (foot_y - y - 4.0).max(40.0));
    let hit_start = app.hits.len();
    g.clip(vp);
    let mut cy = y - im.scroll;
    if im.loading {
        g.text("Reading…", Rect::new(x, cy, iw, 20.0), f(12.0), TX2);
        cy += 22.0;
    }
    if let Some(e) = &im.err {
        for w in wrap(g, e, iw, f(11.5), 3) {
            g.text(&w, Rect::new(x, cy, iw, 15.0), f(11.5), HOT_TX);
            cy += 15.0;
        }
        cy += 6.0;
    }
    if let Some(pv) = &im.preview {
        for n in &pv.notes {
            for w in wrap(g, n, iw, f(11.5), 4) {
                g.text(&w, Rect::new(x, cy, iw, 15.0), f(11.5), if n.contains("twice") || n.contains("starts these programs") { WARN_TX } else { TX3 });
                cy += 15.0;
            }
            cy += 4.0;
        }
        cy += 4.0;
        for (i, it) in pv.items.iter().enumerate() {
            let row = Rect::new(x, cy, iw, 46.0);
            let ok = it.importable();
            let on = im.picked.get(i).copied().unwrap_or(false);
            let hit = Hit::Startup(IM_ITEM + i);
            if app.hover == Some(hit) && ok {
                g.round(row, 5.0, PAN2);
            }
            let b = Rect::new(row.x + 6.0, row.y + 6.0, 16.0, 16.0);
            g.round(b, 4.0, if on { ACC } else { PAN2 });
            g.round_stroke(b, 4.0, if !ok { TX4 } else if on { ACC } else { LN }, 1.0);
            if on {
                g.text("\u{E73E}", b, f(10.0).icon().c(), BG);
            }
            g.text(&it.app.name, Rect::new(row.x + 32.0, row.y + 2.0, row.w * 0.35, 20.0), f(12.5).b(), if ok { TX1 } else { TX3 });
            g.text(&it.app.command, Rect::new(row.x + 32.0 + row.w * 0.35, row.y + 2.0, row.w * 0.65 - 38.0, 20.0), f(11.5).m(), TX3);
            let mut note: Vec<String> = Vec::new();
            for p in &it.problems {
                note.push(format!("Cannot import: {p}"));
            }
            for w in &it.warnings {
                note.push(w.clone());
            }
            if let Some((id, st)) = &it.original {
                note.push(format!("its Windows entry {} \"{}\" is {st}; import leaves it alone", id.source_label(), id.key));
            }
            let t = note.join(" · ");
            g.text(&t, Rect::new(row.x + 32.0, row.y + 24.0, row.w - 38.0, 18.0), f(11.0), if ok { TX3 } else { HOT_TX });
            if ok {
                app.hits.push((row, hit));
            }
            if app.hovering(hit) || (!ok && app.hover == Some(Hit::Startup(IM_PANEL)) && false) {
                app.set_tip(row, vec![it.app.name.clone(), it.app.command.clone(), t.clone()]);
            }
            cy += 50.0;
        }
        if pv.items.is_empty() {
            g.text("Nothing to import in this file.", Rect::new(x, cy, iw, 20.0), f(12.0), TX3);
            cy += 22.0;
        }
    }
    let content_h = cy + im.scroll - y;
    g.unclip();
    let mut i = hit_start;
    while i < app.hits.len() {
        match visible_rect(app.hits[i].0, vp) {
            Some(v) => {
                app.hits[i].0 = v;
                i += 1;
            }
            None => {
                app.hits.remove(i);
            }
        }
    }
    im.scroll = im.scroll.clamp(0.0, (content_h - vp.h).max(0.0));
    super::nav::paint_bar(app, g, super::nav::BAR_STARTUP_IMPORT, vp, content_h, im.scroll);

    // footer
    let by = r.b() - 44.0;
    let n_pick = im.picked.iter().filter(|p| **p).count();
    let any_valid = im.preview.as_ref().is_some_and(|p| p.items.iter().any(|i| i.importable()));
    let mut bx = x;
    let all = Rect::new(bx, by, g.measure("Select all valid", f(12.0)) + 28.0, 30.0);
    button(g, all, "Select all valid", app.hover == Some(Hit::Startup(IM_ALL)), any_valid && !im.importing);
    if any_valid && !im.importing {
        app.hits.push((all, Hit::Startup(IM_ALL)));
    }
    bx = all.r() + 10.0;
    let _ = bx;
    let label = if n_pick == 0 { "Import selected".to_string() } else { format!("Import {n_pick} selected (added off)") };
    let w = g.measure(&label, f(12.0).b()) + 36.0;
    let ir = Rect::new(r.r() - 18.0 - w, by, w, 30.0);
    if n_pick > 0 && !im.importing {
        accent(g, ir, &label, app.hover == Some(Hit::Startup(IM_DO)), false);
        app.hits.push((ir, Hit::Startup(IM_DO)));
    } else {
        button(g, ir, &label, false, false);
    }
    if im.importing {
        g.text("Saving…", Rect::new(all.r() + 12.0, by, 100.0, 30.0), f(11.5), TX2);
    }
    app.startup.import = Some(im);
}

// ---- input -------------------------------------------------------------------------------------

pub fn wants_chars(app: &App) -> bool {
    app.tab == Tab::Startup && (app.startup.inline.as_ref().is_some_and(|i| !i.saving) || app.startup.editor.as_ref().is_some_and(|e| e.focus.is_some() && !e.saving) || app.startup.import.as_ref().is_some_and(|i| i.focus && !i.importing))
}

pub fn char(app: &mut App, c: char) -> Action {
    let s = &mut app.startup;
    if let Some(i) = s.inline.as_mut().filter(|i| !i.saving) {
        if accepts(Field::Delay, c) && i.text.chars().count() < 8 {
            i.text.push(c);
            i.err = None;
        }
        return Action::Repaint;
    }
    if let Some(ed) = s.editor.as_mut() {
        if let Some(fld) = ed.focus {
            if accepts(fld, c) {
                let (t, max) = ed.field_mut(fld);
                if t.chars().count() < max {
                    t.push(c);
                    ed.err = None;
                }
            }
        }
    } else if let Some(im) = s.import.as_mut() {
        if im.focus && !c.is_control() && im.path.chars().count() < 520 {
            im.path.push(c);
        }
    }
    Action::Repaint
}

fn paste_into(app: &mut App) {
    let Some(text) = crate::win::paste_text() else { return };
    let line = text.lines().next().unwrap_or("").to_string();
    if let Some(i) = app.startup.inline.as_mut().filter(|i| !i.saving) {
        for c in line.chars() {
            if accepts(Field::Delay, c) && i.text.chars().count() < 8 {
                i.text.push(c);
            }
        }
        i.err = None;
        return;
    }
    if let Some(ed) = app.startup.editor.as_mut() {
        if let Some(fld) = ed.focus {
            let (t, max) = ed.field_mut(fld);
            for c in line.chars() {
                if accepts(fld, c) && t.chars().count() < max {
                    t.push(c);
                }
            }
            ed.err = None;
        }
    } else if let Some(im) = app.startup.import.as_mut() {
        if im.focus {
            for c in line.trim().trim_matches('"').chars() {
                if !c.is_control() && im.path.chars().count() < 520 {
                    im.path.push(c);
                }
            }
        }
    }
}

pub fn key(app: &mut App, vk: u16, ctrl: bool, shift: bool) -> Option<Action> {
    const BACK: u16 = 0x08;
    const TAB: u16 = 0x09;
    const RET: u16 = 0x0D;
    const ESC: u16 = 0x1B;
    if app.tab != Tab::Startup {
        return None;
    }
    if let Some(saving) = app.startup.inline.as_ref().map(|i| i.saving) {
        match vk {
            ESC if !saving => app.startup.inline = None,
            ESC => {}
            RET if !saving => return Some(inline_commit(app)),
            BACK if !saving => {
                if let Some(i) = app.startup.inline.as_mut() {
                    if ctrl {
                        i.text.clear();
                    } else {
                        i.text.pop();
                    }
                    i.err = None;
                }
            }
            v if ctrl && v == b'V' as u16 && !saving => paste_into(app),
            _ => return Some(Action::None),
        }
        return Some(Action::Repaint);
    }
    if app.startup.editor.is_some() {
        let saving = app.startup.editor.as_ref().is_some_and(|e| e.saving);
        match vk {
            ESC if !saving => app.startup.editor = None,
            ESC => {}
            TAB => {
                if let Some(ed) = app.startup.editor.as_mut() {
                    let order = [Field::Name, Field::Command, Field::Delay, Field::Process];
                    let i = ed.focus.and_then(|f| order.iter().position(|o| *o == f));
                    let n = order.len();
                    ed.focus = Some(order[match (i, shift) {
                        (None, _) => 0,
                        (Some(i), false) => (i + 1) % n,
                        (Some(i), true) => (i + n - 1) % n,
                    }]);
                }
            }
            RET => return Some(editor_save(app)),
            BACK => {
                if let Some(ed) = app.startup.editor.as_mut().filter(|e| !e.saving) {
                    if let Some(fld) = ed.focus {
                        let (t, _) = ed.field_mut(fld);
                        if ctrl {
                            t.clear();
                        } else {
                            t.pop();
                        }
                        ed.err = None;
                    }
                }
            }
            v if ctrl && v == b'V' as u16 && !saving => paste_into(app),
            _ => return Some(Action::None),
        }
        return Some(Action::Repaint);
    }
    if app.startup.import.is_some() {
        let busy = app.startup.import.as_ref().is_some_and(|i| i.importing);
        match vk {
            ESC if !busy => app.startup.import = None,
            ESC => {}
            RET => return Some(import_load(app)),
            BACK => {
                if let Some(im) = app.startup.import.as_mut().filter(|i| i.focus && !i.importing) {
                    if ctrl {
                        im.path.clear();
                    } else {
                        im.path.pop();
                    }
                }
            }
            v if ctrl && v == b'V' as u16 && !busy => paste_into(app),
            _ => return Some(Action::None),
        }
        return Some(Action::Repaint);
    }
    // the table's arrows go through App::key (ui/nav.rs) like every list
    None
}

/// A paint listed `order`: remember where the selection is and how tall the list is.
fn seen_at(app: &mut App, list_h: f32) {
    let s = &mut app.startup;
    s.list_h = list_h;
    if let Some(i) = s.sel.as_ref().and_then(|sel| s.order.iter().position(|o| o == sel)) {
        s.nav_last = Some(i);
    }
}

/// The editor, the import panel or a two-step button waiting for its second press owns the keys.
pub fn modal(app: &App) -> bool {
    app.tab == Tab::Startup && (app.startup.inline.is_some() || app.startup.editor.is_some() || app.startup.import.is_some() || app.startup.confirm.is_some())
}

/// Up / Down / Page / Home / End: select by entry / app id from the rows of the last paint,
/// scrolled into view. The details panel follows the selection.
pub fn nav(app: &mut App, m: super::nav::Move) {
    let s = &mut app.startup;
    let cur = s.sel.as_ref().and_then(|sel| s.order.iter().position(|o| o == sel));
    let gone = if s.sel.is_some() && cur.is_none() { s.nav_last } else { None };
    let page = super::nav::page_rows(s.list_h, ROW_H);
    let Some(i) = super::nav::target(s.order.len(), cur, gone, m, page) else { return };
    s.sel = Some(s.order[i].clone());
    s.nav_last = Some(i);
    s.confirm = None;
    s.scroll = super::nav::reveal(s.scroll, i as f32 * ROW_H, ROW_H, s.list_h);
}

/// Esc on the table: a pending two-step press first, then the selection. True when it did something.
pub fn esc(app: &mut App) -> bool {
    if app.startup.confirm.take().is_some() {
        return true;
    }
    app.startup.sel.take().is_some()
}

/// The scroll offset of a Startup scroll bar (`nav::BAR_STARTUP*`): the lists, the editor, the import panel.
pub fn bar_scroll(app: &mut App, id: u8) -> Option<&mut f32> {
    match id {
        super::nav::BAR_STARTUP => Some(&mut app.startup.scroll),
        super::nav::BAR_STARTUP_EDIT => app.startup.editor.as_mut().map(|e| &mut e.scroll),
        _ => app.startup.import.as_mut().map(|i| &mut i.scroll),
    }
}

pub fn wheel(app: &mut App, delta: f32) -> bool {
    if app.tab != Tab::Startup {
        return false;
    }
    if let Some(ed) = app.startup.editor.as_mut() {
        ed.scroll = (ed.scroll - delta * 0.5).max(0.0);
        return true;
    }
    if let Some(im) = app.startup.import.as_mut() {
        im.scroll = (im.scroll - delta * 0.5).max(0.0);
        return true;
    }
    false
}

fn editor_save(app: &mut App) -> Action {
    let Some(ed) = app.startup.editor.as_mut() else { return Action::None };
    if ed.saving {
        return Action::None;
    }
    if let Some((_, p)) = &ed.adopt {
        if let Some(b) = &p.blocked {
            ed.err = Some(format!("Cannot be taken over: {b}"));
            return Action::Repaint;
        }
    }
    let a = match ed.to_app() {
        Ok(a) => a,
        Err(e) => {
            ed.err = Some(e);
            return Action::Repaint;
        }
    };
    let Some(m) = mgr() else {
        ed.err = Some("The startup manager is not available.".into());
        return Action::Repaint;
    };
    ed.err = None;
    let (app_id, adopt_of) = (ed.app_id.clone(), ed.adopt.as_ref().map(|(e, _)| e.clone()));
    if adopt_of.is_none() {
        if let Some(id) = app_id.clone() {
            let accepted = save_existing(app, id, a);
            return finish_editor_save(app, accepted);
        }
    }
    let accepted = app.startup.job("startup: save entry", move || match (adopt_of, app_id) {
        (Some(entry), _) => adopt::adopt(&m.loc, &m.store, &entry, a.clone()).map(|r| format!("Saved: {} is managed by Hayate. {}", a.name, r.note)),
        (None, Some(_)) => Err("internal: saved through save_existing".into()),
        (None, None) => m
            .store
            .update_sync(|c| {
                let mut n = a.clone();
                n.id = c.fresh_id();
                c.apps.push(n);
            })
            .map(|_| format!("Saved: {} was added to the Hayate list. Nothing was started.", a.name)),
    });
    finish_editor_save(app, accepted)
}

fn finish_editor_save(app: &mut App, accepted: Option<u64>) -> Action {
    // only an ACCEPTED job puts the editor into "saving"; a busy refusal leaves the edit as it is, for another Save
    if let Some(ed) = app.startup.editor.as_mut() {
        match accepted {
            Some(token) => {
                ed.saving = true;
                ed.token = token;
            }
            None => ed.err = Some("Another change is still being saved. Press Save again in a moment; your edit is kept.".into()),
        }
    }
    Action::Repaint
}

fn import_load(app: &mut App) -> Action {
    let (Some(m), Some(im)) = (mgr(), app.startup.import.as_mut()) else { return Action::None };
    if im.loading || im.importing || im.path.trim().is_empty() {
        return Action::None;
    }
    im.loading = true;
    im.err = None;
    im.preview = None;
    let path = std::path::PathBuf::from(im.path.trim().trim_matches('"'));
    let out = im.loaded.clone();
    crate::bgio::submit("startup: read Launchpad config", move || {
        let r = import::preview_file(&path, &m.loc, &m.store);
        *lock(&out) = Some(r);
        super::icons::poke();
    });
    Action::Repaint
}

pub fn click(app: &mut App, h: Hit) -> Action {
    let Hit::Startup(id) = h else { return Action::None };
    app.search_focus = false;
    // overlays first
    if app.startup.editor.is_some() {
        return editor_click(app, id);
    }
    if app.startup.import.is_some() {
        return import_click(app, id);
    }
    if app.startup.inline.is_some() {
        if id == INLINE_FIELD || app.startup.inline.as_ref().is_some_and(|i| i.saving) {
            return Action::None;
        }
        // a click anywhere else leaves the cell as it was
        app.startup.inline = None;
    }
    if app.startup.confirm.as_ref().is_some_and(|(c, s)| *c != id || *s != app.startup.sel) {
        app.startup.confirm = None;
    }
    let sel_app = |app: &App| match &app.startup.sel {
        Some(Sel::App(i)) => Some(i.clone()),
        _ => None,
    };
    match id {
        SEG_WIN | SEG_LIST => {
            app.startup.mode = if id == SEG_WIN { Mode::Windows } else { Mode::List };
            app.startup.sel = None;
            app.startup.scroll = 0.0;
        }
        i if (FILTER0..FILTER0 + FILTERS.len()).contains(&i) => {
            app.startup.filter = FILTERS[i - FILTER0].0;
            app.startup.scroll = 0.0;
        }
        i if (HDR0..HDR0 + 10).contains(&i) => {
            let key = match i - HDR0 {
                0 => SortKey::Name,
                1 => SortKey::Status,
                2 => SortKey::Scope,
                3 => SortKey::Managed,
                _ => return Action::None,
            };
            let (k, asc) = app.startup.sort;
            app.startup.sort = if k == key { (k, !asc) } else { (key, true) };
        }
        REFRESH => {
            app.startup.msg = None;
            app.startup.load();
        }
        ADD => {
            app.startup.editor = Some(Editor::blank());
        }
        IMPORT => {
            app.startup.import = Some(ImportPanel { path: String::new(), focus: true, preview: None, picked: Vec::new(), err: None, loading: false, importing: false, token: 0, scroll: 0.0, loaded: Arc::new(Mutex::new(None)) });
        }
        RUN_ALL => {
            if let Some(m) = mgr() {
                app.startup.msg = Some(match m.start(None, "Run all now") {
                    Ok(()) => (true, "Started the list. Progress shows in Last run.".into()),
                    Err(e) => (false, e),
                });
            }
        }
        CANCEL => {
            if let Some(m) = mgr() {
                let n = m.run.cancel();
                app.startup.msg = Some((true, if n == 0 { "Cancelling: what has not started will not start. Programs already started stay.".to_string() } else { format!("Cancelling: what has not started will not start. {n} program{} already handed to Windows cannot be recalled and may still start.", if n == 1 { " was" } else { "s were" }) }));
            }
        }
        MANAGE => {
            if let Some(m) = mgr() {
                let on = !m.store.snapshot().cfg.manage;
                let elevation = crate::osutil::elevation();
                // pressing ON that does not stick (no usable Start with Windows shortcut) leaves the offer up; OFF clears it
                app.startup.want_signin = on && elevation == Some(false);
                if on && elevation != Some(false) {
                    app.startup.msg = Some((false, "Hayate is running as administrator (or could not tell), so it will not change its Start with Windows shortcut: any program of yours can edit that file. Turn the list on from a normal Hayate.".into()));
                    return Action::Repaint;
                }
                let _ = app.startup.job("startup: switch manager", move || {
                    let was = m.store.snapshot().cfg.manage;
                    m.store.update_sync(|c| c.manage = on)?;
                    if on {
                        // the opt-in: Hayate's own Start with Windows shortcut gets one private argument so a start that came
                        // from it can be told from a manual or update start. Unrelated arguments stay. If it cannot be saved,
                        // the switch goes back off.
                        if let Err(e) = crate::startup::own::set_flag(&m.loc, true) {
                            let back = m.store.update_sync(|c| c.manage = was);
                            return Err(format!("{e} The list stays off.{}", if back.is_err() { " Hayate could not save that either; check the switch." } else { "" }));
                        }
                        Ok("Saved: the list is on. Hayate's Start with Windows shortcut now carries a private flag, so the list runs once per sign-in, only when that shortcut starts Hayate. Nothing starts now. Entries that run as administrator wait for you to press Run now.".to_string())
                    } else {
                        let note = if elevation == Some(false) {
                            match crate::startup::own::set_flag(&m.loc, false) {
                                Ok(()) => String::new(),
                                Err(e) => format!(" The shortcut keeps its private flag ({e}); with the list off it does nothing."),
                            }
                        } else {
                            " The shortcut keeps its private flag (Hayate is running as administrator and will not edit it); with the list off it does nothing.".to_string()
                        };
                        Ok(format!("Saved: the list is off. Hayate will not start anything at sign-in.{note}"))
                    }
                });
            }
        }
        AUTOSTART => {
            let Some(m) = mgr() else { return Action::Repaint };
            let Some(e) = app.startup.scan.entries.iter().find(|e| e.is_hayate && e.id.scope == Scope::User).cloned() else { return Action::Repaint };
            let on = e.state() != State::Enabled;
            app.startup.job("startup: Start with Windows", move || {
                adopt::set_enabled(&m.loc, &e, on, &m.store).map(|_| if on { "Saved: Hayate starts with Windows.".to_string() } else { "Saved: Hayate no longer starts with Windows (the list cannot run at sign-in).".to_string() })
            });
        }
        SIGNIN_FIX => {
            let Some(m) = mgr() else { return Action::Repaint };
            // the click is the consent; the job reads the shortcut again and does only what is missing
            app.startup.job("startup: Start Hayate with Windows", move || {
                // (make_ready itself refuses a copy that is not the per-user installed one, or not provably unelevated)
                let list_on = m.store.snapshot().cfg.manage;
                crate::startup::own::make_ready(&m.loc, list_on, crate::setup::enable_autostart)?;
                Ok(if list_on {
                    "Saved: Hayate starts with Windows, hidden in the tray. The list runs once at the next sign-in. Nothing starts now.".to_string()
                } else {
                    "Saved: Hayate starts with Windows, hidden in the tray. Run at sign-in is still off: switch it on to use the list.".to_string()
                })
            });
        }
        D_TOGGLE => toggle_selected(app),
        D_MANAGE => {
            let (Some(m), Some(e)) = (mgr(), app.startup.selected_entry()) else { return Action::Repaint };
            let plan = adopt::preview(&m.loc, &e, &m.store);
            app.startup.editor = Some(Editor::from_entry(&e, plan));
        }
        D_COPY => {
            let text = match (&app.startup.sel, app.startup.mode) {
                (Some(Sel::Entry(_)), _) => app.startup.selected_entry().map(|e| e.shown_command()),
                (Some(Sel::App(i)), _) => mgr().and_then(|m| m.store.snapshot().cfg.app(i).map(|a| a.command.clone())),
                _ => None,
            };
            if let Some(t) = text {
                return Action::Clip(t);
            }
        }
        D_ADMIN => return Action::RestartAdmin,
        D_OPENLIST => {
            let owner = app.startup.selected_entry().and_then(|e| mgr().and_then(|m| m.store.snapshot().cfg.owner_of(&e.id).map(|a| a.id.clone())));
            if let Some(a) = owner {
                app.startup.mode = Mode::List;
                app.startup.sel = Some(Sel::App(a));
                app.startup.scroll = 0.0;
            }
        }
        D_EDIT => {
            if let (Some(m), Some(i)) = (mgr(), sel_app(app)) {
                if let Some(a) = m.store.snapshot().cfg.app(&i) {
                    app.startup.editor = Some(Editor::from_app(a));
                }
            }
        }
        D_RUN => {
            if let (Some(m), Some(i)) = (mgr(), sel_app(app)) {
                app.startup.msg = Some(match m.start(Some(&i), "Run now") {
                    Ok(()) => (true, "Started. Progress shows in Last run.".into()),
                    Err(e) => (false, e),
                });
            }
        }
        D_APP_ON => {
            if let (Some(m), Some(i)) = (mgr(), sel_app(app)) {
                app.startup.job("startup: turn entry on or off", move || {
                    let mut now = false;
                    m.store
                        .update_sync(|c| {
                            if let Some(a) = c.apps.iter_mut().find(|a| a.id == i) {
                                a.enabled = !a.enabled;
                                now = a.enabled;
                            }
                        })
                        .map(|_| format!("Saved: the entry is now {}.", if now { "on" } else { "off" }))
                });
            }
        }
        D_UNMANAGE | D_REMOVE => {
            let sel = app.startup.sel.clone();
            if app.startup.confirm != Some((id, sel.clone())) {
                app.startup.confirm = Some((id, sel));
                return Action::Repaint;
            }
            app.startup.confirm = None;
            if let (Some(m), Some(i)) = (mgr(), sel_app(app)) {
                if id == D_UNMANAGE {
                    app.startup.job("startup: unmanage entry", move || match adopt::release(&m.loc, &m.store, &i)? {
                        adopt::Released::Restored => Ok("Saved: the original Windows entry is back exactly as it was. Hayate still lists the app.".to_string()),
                        adopt::Released::ChangedSince(why) => Ok(format!("Done. The original was not touched because {why}.")),
                    });
                } else {
                    app.startup.job("startup: remove entry", move || {
                        adopt::remove(&m.loc, &m.store, &i).map(|r| match r {
                            Some(adopt::Released::Restored) => "Saved: removed. The original Windows entry is back exactly as it was.".to_string(),
                            Some(adopt::Released::ChangedSince(why)) => format!("Saved: removed. The original was not touched because {why}."),
                            None => "Saved: removed from the Hayate list.".to_string(),
                        })
                    });
                }
            }
        }
        i if i >= ROW => {
            let i = i - ROW;
            if let Some(s) = app.startup.order.get(i).cloned() {
                app.startup.sel = Some(s);
            }
        }
        _ => return Action::None,
    }
    Action::Repaint
}

/// Enter in the Delay cell: the editor's own validation (`to_app`), then its own save.
fn inline_commit(app: &mut App) -> Action {
    let Some(i) = app.startup.inline.as_ref() else { return Action::None };
    let (id, text) = (i.app_id.clone(), i.text.clone());
    let Some(m) = mgr() else { return Action::None };
    let Some(cur) = m.store.snapshot().cfg.app(&id).cloned() else {
        app.startup.inline = None;
        return Action::Repaint;
    };
    match with_delay(&cur, &text) {
        Err(e) => {
            if let Some(i) = app.startup.inline.as_mut() {
                i.err = Some(e);
            }
        }
        Ok(a) => match save_existing(app, id, a) {
            Some(token) => {
                if let Some(i) = app.startup.inline.as_mut() {
                    i.saving = true;
                    i.token = token;
                    i.err = None;
                }
            }
            None => {
                if let Some(i) = app.startup.inline.as_mut() {
                    i.err = Some("Another change is still being saved. Press Enter again in a moment.".into());
                }
            }
        },
    }
    Action::Repaint
}

/// The saved app with only its delay replaced, validated like the editor's Save.
fn with_delay(cur: &MApp, text: &str) -> Result<MApp, String> {
    let mut ed = Editor::from_app(cur);
    ed.delay = text.to_string();
    ed.to_app()
}

/// The saved app with only its After replaced, validated like the editor's Save.
fn with_after(cur: &MApp, after: Option<String>) -> Result<MApp, String> {
    let mut ed = Editor::from_app(cur);
    ed.after = after;
    ed.to_app()
}

/// Double-click on a row: an editable cell acts, any other cell keeps the plain behaviour (nothing).
pub fn double_click(app: &mut App, x: f32, y: f32) -> Action {
    let s = &app.startup;
    if app.tab != Tab::Startup || s.editor.is_some() || s.import.is_some() || s.inline.is_some() {
        return Action::None;
    }
    let Some(Hit::Startup(id)) = app.hit(x, y) else { return Action::None };
    if id < ROW {
        return Action::None;
    }
    let (Some(sel), Some(cell)) = (app.startup.order.get(id - ROW).cloned(), cell_at(&app.startup.cols, x)) else { return Action::None };
    // the first click of the pair selected this row; make sure
    app.startup.sel = Some(sel.clone());
    app.startup.confirm = None;
    match (cell, sel) {
        (Cell::Status, Sel::Entry(_)) => toggle_selected(app),
        (Cell::AppState, Sel::App(_)) => return click(app, Hit::Startup(D_APP_ON)),
        (Cell::Delay, Sel::App(i)) => {
            let Some(a) = mgr().and_then(|m| m.store.snapshot().cfg.app(&i).cloned()) else { return Action::Repaint };
            app.startup.inline = Some(Inline { app_id: i, text: delay_text(a.delay_ms), err: None, saving: false, token: 0 });
        }
        (Cell::After, Sel::App(i)) => {
            let Some(cfg) = mgr().map(|m| m.store.snapshot().cfg) else { return Action::Repaint };
            let Some(a) = cfg.app(&i) else { return Action::Repaint };
            return Action::StartupMenu(x, y, Box::new(after_menu(a, &cfg)));
        }
        _ => return Action::None,
    }
    Action::Repaint
}

/// Right-click on a row: select it, and open the menu for THAT row.
pub fn right_click(app: &mut App, id: usize, x: f32, y: f32) -> Action {
    let s = &app.startup;
    if app.tab != Tab::Startup || id < ROW || s.editor.is_some() || s.import.is_some() || s.inline.is_some() {
        return Action::None;
    }
    let Some(sel) = s.order.get(id - ROW).cloned() else { return Action::None };
    app.startup.sel = Some(sel.clone());
    app.startup.confirm = None;
    let Some(m) = mgr() else { return Action::Repaint };
    let cfg = m.store.snapshot().cfg;
    let ctx = match &sel {
        Sel::Entry(eid) => {
            let Some(e) = app.startup.scan.entries.iter().find(|e| &e.id == eid) else { return Action::Repaint };
            let owned = cfg.owner_of(&e.id).is_some();
            let locate = locate_of(e);
            let facts = WinFacts { toggle: toggle_of(e, owned), owned, is_hayate: e.is_hayate, needs_admin: e.id.machine() && !is_elevated() && !owned, locate: locate.is_some() };
            MenuCtx { sel: sel.clone(), name: e.display(), locate, entries: win_menu(&facts), after: None }
        }
        Sel::App(aid) => {
            let Some(a) = cfg.app(aid) else { return Action::Repaint };
            let flight = !m.run.in_flight().is_empty();
            let runv = m.run.view();
            let locate = crate::startup::launch::icon_source(&a.command);
            let facts = AppFacts { enabled: a.enabled, own: a.own.is_some(), can_run: !runv.active && !flight, locate: locate.is_some() };
            MenuCtx { sel: sel.clone(), name: a.name.clone(), locate, entries: app_menu(&facts), after: None }
        }
    };
    Action::StartupMenu(x, y, Box::new(ctx))
}

/// What a context menu knows: captured when it opens, so the pick goes to THAT row.
pub struct MenuCtx {
    sel: Sel,
    name: String,
    locate: Option<String>,
    entries: Vec<super::procmenu::Entry>,
    /// the After choice menu: (app id, the ids behind choices M_AFTER0..)
    after: Option<(String, Vec<String>)>,
}

impl MenuCtx {
    pub fn entries(&self) -> &[super::procmenu::Entry] {
        &self.entries
    }
    pub fn locate(&self) -> Option<&str> {
        self.locate.as_deref()
    }
}

const M_NOTE: usize = 1;
pub const M_LOCATE: usize = 60;
const M_COPYNAME: usize = 61;
const M_SETDELAY: usize = 62;
const M_AFTER_NONE: usize = 200;
const M_AFTER0: usize = 201;

use super::procmenu::Entry as ME;

fn mi(id: usize, text: &str, enabled: bool) -> ME {
    ME::Item { id, text: text.to_string(), checked: false, enabled }
}

struct WinFacts {
    toggle: Toggle,
    owned: bool,
    is_hayate: bool,
    needs_admin: bool,
    locate: bool,
}

/// Items of the menu of a Windows entry (pure, unit-tested).
fn win_menu(f: &WinFacts) -> Vec<ME> {
    let mut m = Vec::new();
    match &f.toggle {
        Toggle::Go(on) => m.push(mi(D_TOGGLE, if *on { "Enable" } else { "Disable" }, true)),
        Toggle::No(why) => {
            m.push(mi(D_TOGGLE, "Enable / Disable", false));
            m.push(mi(M_NOTE, why, false));
        }
    }
    if f.owned {
        m.push(mi(D_OPENLIST, "Open in Hayate list", true));
    } else {
        m.push(mi(D_MANAGE, "Manage in Hayate…", !f.is_hayate));
    }
    if f.needs_admin {
        m.push(mi(D_ADMIN, "Restart as administrator", true));
    }
    m.push(ME::Sep);
    m.push(mi(M_LOCATE, "Open file location", f.locate));
    m.push(mi(D_COPY, "Copy command", true));
    m.push(mi(M_COPYNAME, "Copy name", true));
    m
}

struct AppFacts {
    enabled: bool,
    own: bool,
    can_run: bool,
    locate: bool,
}

/// Items of the menu of a Hayate list app (pure, unit-tested). Unmanage and Remove only arm the
/// button's own two-step confirm.
fn app_menu(f: &AppFacts) -> Vec<ME> {
    let mut m = vec![mi(D_EDIT, "Edit…", true), mi(M_SETDELAY, "Set delay…", true), mi(D_RUN, "Run now", f.can_run), mi(D_APP_ON, if f.enabled { "Turn off" } else { "Turn on" }, true), ME::Sep];
    if f.own {
        m.push(mi(D_UNMANAGE, "Unmanage… (give back the original)", true));
    }
    m.push(mi(D_REMOVE, "Remove from the Hayate list…", true));
    m.push(ME::Sep);
    m.push(mi(M_LOCATE, "Open file location", f.locate));
    m.push(mi(D_COPY, "Copy command", true));
    m.push(mi(M_COPYNAME, "Copy name", true));
    m
}

/// The After choice: nothing, or one of the other apps (current one checked).
fn after_menu(a: &MApp, cfg: &Config) -> MenuCtx {
    let others: Vec<&MApp> = cfg.apps.iter().filter(|o| o.id != a.id).collect();
    let mut entries = vec![ME::Item { id: M_AFTER_NONE, text: "Nothing (start right away)".into(), checked: a.after.is_none(), enabled: true }];
    for (i, o) in others.iter().enumerate() {
        entries.push(ME::Item { id: M_AFTER0 + i, text: o.name.clone(), checked: a.after.as_deref() == Some(o.id.as_str()), enabled: true });
    }
    MenuCtx { sel: Sel::App(a.id.clone()), name: a.name.clone(), locate: None, entries, after: Some((a.id.clone(), others.iter().map(|o| o.id.clone()).collect())) }
}

/// The user picked `id` in a menu opened by `right_click` / `double_click`. Everything runs
/// through the same handlers as the buttons and the editor.
pub fn menu_pick(app: &mut App, ctx: &MenuCtx, id: usize) -> Action {
    if app.tab != Tab::Startup || id == 0 {
        return Action::None;
    }
    if let Some((aid, ids)) = &ctx.after {
        let Some(cur) = mgr().and_then(|m| m.store.snapshot().cfg.app(aid).cloned()) else { return Action::Repaint };
        let after = match id {
            M_AFTER_NONE => None,
            n if n >= M_AFTER0 => match ids.get(n - M_AFTER0) {
                Some(x) => Some(x.clone()),
                None => return Action::None,
            },
            _ => return Action::None,
        };
        match with_after(&cur, after) {
            Err(e) => app.startup.msg = Some((false, e)),
            Ok(a) => {
                save_existing(app, aid.clone(), a);
            }
        }
        return Action::Repaint;
    }
    // the menu was for this row; act on it even if the list moved meanwhile
    app.startup.sel = Some(ctx.sel.clone());
    match id {
        M_COPYNAME => Action::Clip(ctx.name.clone()),
        M_SETDELAY => {
            if let Sel::App(i) = &ctx.sel {
                if let Some(a) = mgr().and_then(|m| m.store.snapshot().cfg.app(i).cloned()) {
                    app.startup.inline = Some(Inline { app_id: i.clone(), text: delay_text(a.delay_ms), err: None, saving: false, token: 0 });
                }
            }
            Action::Repaint
        }
        M_LOCATE | M_NOTE => Action::None,
        D_UNMANAGE | D_REMOVE => {
            let a = click(app, Hit::Startup(id));
            app.startup.msg = Some((true, "Press the highlighted button below again to confirm.".into()));
            a
        }
        D_TOGGLE | D_MANAGE | D_COPY | D_ADMIN | D_OPENLIST | D_EDIT | D_RUN | D_APP_ON => click(app, Hit::Startup(id)),
        _ => Action::None,
    }
}

/// Enable or disable the selected Windows entry: the one path of the button, the context menu and a
/// double-click on Status. A gated entry shows its reason instead of acting.
fn toggle_selected(app: &mut App) {
    let (Some(m), Some(e)) = (mgr(), app.startup.selected_entry()) else { return };
    let owned = m.store.snapshot().cfg.owner_of(&e.id).is_some();
    match toggle_of(&e, owned) {
        Toggle::No(why) => app.startup.msg = Some((false, why)),
        Toggle::Go(on) => {
            let name = e.display();
            app.startup.job("startup: enable or disable entry", move || adopt::set_enabled(&m.loc, &e, on, &m.store).map(|_| format!("Saved: {name} is now {}.", if on { "enabled" } else { "disabled" })));
        }
    }
}

/// Write the validated app over the saved one with the same id: the editor's save and the in-place edits.
fn save_existing(app: &mut App, id: String, a: MApp) -> Option<u64> {
    let m = mgr()?;
    app.startup.job("startup: save entry", move || {
        m.store
            .update_sync(|c| {
                if let Some(t) = c.apps.iter_mut().find(|t| t.id == id) {
                    t.name = a.name.clone();
                    t.command = a.command.clone();
                    t.enabled = a.enabled;
                    t.delay_ms = a.delay_ms;
                    t.after = a.after.clone();
                    t.cpu_below = a.cpu_below;
                    t.admin = a.admin;
                    t.process = a.process.clone();
                }
            })
            .map(|_| format!("Saved: {}.", a.name))
    })
}

fn editor_click(app: &mut App, id: usize) -> Action {
    let cfg = mgr().map(|m| m.store.snapshot().cfg).unwrap_or_default();
    let Some(ed) = app.startup.editor.as_mut() else { return Action::None };
    if ed.saving {
        return Action::None;
    }
    match id {
        ED_CLOSE => app.startup.editor = None,
        ED_NAME => ed.focus = Some(Field::Name),
        ED_CMD => ed.focus = Some(Field::Command),
        ED_DELAY => ed.focus = Some(Field::Delay),
        ED_PROC => ed.focus = Some(Field::Process),
        ED_AFTER_P | ED_AFTER_N => {
            let others: Vec<String> = cfg.apps.iter().filter(|a| Some(&a.id) != ed.app_id.as_ref()).map(|a| a.id.clone()).collect();
            ed.after = after_step(ed.after.as_deref(), &others, id == ED_AFTER_N);
            ed.focus = None;
        }
        ED_CPU_M => ed.cpu = cpu_step(ed.cpu, false),
        ED_CPU_P => ed.cpu = cpu_step(ed.cpu, true),
        ED_ADMIN => ed.admin = !ed.admin,
        ED_ENABLED => ed.enabled = !ed.enabled,
        ED_SAVE => return editor_save(app),
        _ => {
            ed.focus = None;
            return Action::None;
        }
    }
    Action::Repaint
}

fn import_click(app: &mut App, id: usize) -> Action {
    let Some(im) = app.startup.import.as_mut() else { return Action::None };
    if im.importing {
        return Action::None;
    }
    match id {
        IM_CLOSE => app.startup.import = None,
        IM_PATH => im.focus = true,
        IM_FILL => {
            // only fills the text box; reading happens on Load
            if let Ok(base) = std::env::var("LOCALAPPDATA") {
                im.path = format!(r"{base}\launchpad\config.toml");
            }
            im.focus = true;
        }
        IM_LOAD => return import_load(app),
        IM_ALL => {
            if let Some(pv) = &im.preview {
                im.picked = pv.items.iter().map(|i| i.importable()).collect();
            }
        }
        IM_DO => {
            let (Some(m), Some(pv)) = (mgr(), im.preview.clone()) else { return Action::None };
            let picked = im.picked.clone();
            im.err = None;
            let accepted = app.startup.job("startup: import Launchpad entries", move || {
                import::apply(&m.store, &pv, &picked).map(|r| {
                    let mut t = format!("Saved: added {} (all switched off). {}", r.added.join(", "), import::DUAL_LAUNCH_NOTE);
                    if !r.notes.is_empty() {
                        t.push(' ');
                        t.push_str(&r.notes.join(" "));
                    }
                    t
                })
            });
            if let Some(im) = app.startup.import.as_mut() {
                match accepted {
                    Some(token) => {
                        im.importing = true;
                        im.token = token;
                    }
                    None => im.err = Some("Another change is still being saved. Press Import again in a moment; your selection is kept.".into()),
                }
            }
        }
        i if i >= IM_ITEM && i < ROW => {
            let k = i - IM_ITEM;
            if im.preview.as_ref().and_then(|p| p.items.get(k)).is_some_and(|it| it.importable()) {
                if let Some(p) = im.picked.get_mut(k) {
                    *p = !*p;
                }
            }
        }
        _ => {
            im.focus = false;
            return Action::None;
        }
    }
    Action::Repaint
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::startup::approved::{self, Live};
    use crate::startup::id::View;

    fn entry(name: &str, scope: Scope, approved_bytes: Option<Vec<u8>>) -> Entry {
        Entry { id: EntryId::new(Kind::Run, scope, View::Native, name), raw: format!(r"C:\x\{name}.exe"), command: format!(r"C:\x\{name}.exe"), target: None, icon: String::new(), live: Live::new(true, approved_bytes, None), is_hayate: false, sig: None }
    }

    fn panel(importing: bool, token: u64) -> ImportPanel {
        ImportPanel { path: String::new(), focus: false, preview: None, picked: Vec::new(), err: None, loading: false, importing, token, scroll: 0.0, loaded: Arc::new(Mutex::new(None)) }
    }

    /// RED against the old code: `saving` was set before `job()` and a busy refusal left
    /// it set, so ANOTHER job's completion closed the editor and threw the edit away.
    #[test]
    fn a_completion_of_another_job_never_closes_or_fails_an_open_editor() {
        let mut v = StartupView::default();
        let mut ed = Editor::blank();
        ed.name = "my edit".into();
        ed.saving = true;
        ed.token = 5;
        v.editor = Some(ed);
        lock(&v.inbox).push((4, true, "Saved: something else".into()));
        assert!(v.apply_done());
        assert!(v.editor.as_ref().is_some_and(|e| e.saving && e.name == "my edit"), "an unrelated success must not close the editor");
        lock(&v.inbox).push((3, false, "Failed: something else".into()));
        v.apply_done();
        assert!(v.editor.as_ref().is_some_and(|e| e.saving && e.err.is_none()), "an unrelated failure must not fail the editor");
        // its own completion does
        lock(&v.inbox).push((5, false, "disk full".into()));
        v.apply_done();
        let e = v.editor.as_ref().unwrap();
        assert!(!e.saving && e.err.as_deref() == Some("disk full") && e.name == "my edit", "its own failure re-opens it for another Save, edit kept");
        v.editor.as_mut().unwrap().saving = true;
        lock(&v.inbox).push((5, true, "Saved".into()));
        v.apply_done();
        assert!(v.editor.is_none(), "its own success closes it");
    }

    #[test]
    fn a_completion_only_closes_the_import_panel_that_owns_it() {
        let mut v = StartupView::default();
        v.import = Some(panel(true, 9));
        v.editor = Some(Editor::blank());
        lock(&v.inbox).push((8, true, "other".into()));
        v.apply_done();
        assert!(v.import.is_some() && v.editor.is_some(), "neither overlay is touched by someone else's job");
        lock(&v.inbox).push((9, true, "imported".into()));
        v.apply_done();
        assert!(v.import.is_none() && v.editor.is_some(), "only the importer closes");
        assert_eq!(v.msg, Some((true, "imported".to_string())));
    }

    #[test]
    fn job_refuses_while_busy_and_hands_out_distinct_tokens() {
        let mut v = StartupView::default();
        v.busy.store(true, Ordering::SeqCst);
        assert_eq!(v.job("test busy", || Ok(String::new())), None, "a busy refusal is reported to the caller");
        assert!(v.msg.as_ref().is_some_and(|(ok, t)| !ok && t.contains("still being saved")));
        assert!(lock(&v.inbox).is_empty(), "nothing was started");
        v.busy.store(false, Ordering::SeqCst);
        let a = v.job("test job a", || Ok("a".into()));
        assert!(a.is_some());
        let t0 = std::time::Instant::now();
        while v.busy.load(Ordering::SeqCst) && t0.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let b = v.job("test job b", || Err("no".into()));
        assert!(b.is_some() && a != b);
        let t0 = std::time::Instant::now();
        while v.busy.load(Ordering::SeqCst) && t0.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let got: Vec<(u64, bool)> = lock(&v.inbox).iter().map(|(t, ok, _)| (*t, *ok)).collect();
        assert_eq!(got, vec![(a.unwrap(), true), (b.unwrap(), false)]);
    }

    #[test]
    fn filters_pick_the_right_entries() {
        let on = entry("a", Scope::User, None);
        let off = entry("b", Scope::AllUsers, Some(approved::disabled_bytes(5)));
        assert!(keep(Filter::All, &on, false) && keep(Filter::All, &off, true));
        assert!(keep(Filter::Enabled, &on, false) && !keep(Filter::Enabled, &off, false));
        assert!(keep(Filter::Disabled, &off, false) && !keep(Filter::Disabled, &on, false));
        assert!(keep(Filter::Managed, &on, true) && !keep(Filter::Managed, &on, false));
        assert!(keep(Filter::AllUsers, &off, false) && !keep(Filter::AllUsers, &on, false));
    }

    #[test]
    fn sorting_is_stable_and_total() {
        let a1 = entry("Beta", Scope::User, None);
        let a2 = entry("alpha", Scope::AllUsers, None);
        let a3 = entry("Beta", Scope::AllUsers, None); // same name, other identity
        let mut rows: Vec<(&Entry, Option<&MApp>)> = vec![(&a1, None), (&a2, None), (&a3, None)];
        for key in [SortKey::Name, SortKey::Status, SortKey::Scope, SortKey::Managed] {
            sort_entries(&mut rows, key, true);
            let asc: Vec<String> = rows.iter().map(|(e, _)| e.id.encode()).collect();
            let mut again = rows.clone();
            again.reverse();
            sort_entries(&mut again, key, true);
            assert_eq!(asc, again.iter().map(|(e, _)| e.id.encode()).collect::<Vec<_>>(), "{key:?}: same order whatever the input order");
            sort_entries(&mut rows, key, false);
            let desc: Vec<String> = rows.iter().map(|(e, _)| e.id.encode()).collect();
            assert_eq!(desc, asc.iter().rev().cloned().collect::<Vec<_>>(), "{key:?}: descending is the exact reverse");
        }
        sort_entries(&mut rows, SortKey::Name, true);
        assert_eq!(rows[0].0.display(), "alpha");
    }

    #[test]
    fn delay_text_round_trips_and_rejects_nonsense() {
        assert_eq!(parse_delay(""), Ok(0));
        assert_eq!(parse_delay(" 2.5 "), Ok(2500));
        assert_eq!(parse_delay("3600"), Ok(3_600_000));
        for bad in ["3601", "-1", "abc", "1e3", "NaN", "inf", "1.2.3"] {
            assert!(parse_delay(bad).is_err(), "{bad}");
        }
        for ms in [0u32, 1000, 2500, 90_000, 3_600_000] {
            assert_eq!(parse_delay(&delay_text(ms)), Ok(ms));
        }
        assert!(accepts(Field::Delay, '7') && accepts(Field::Delay, '.') && !accepts(Field::Delay, 'e') && !accepts(Field::Delay, '-'));
        assert!(!accepts(Field::Name, '\n') && accepts(Field::Command, ' '));
    }

    #[test]
    fn cpu_stepper_stays_in_bounds_and_can_turn_off() {
        assert_eq!(cpu_step(None, false), None);
        assert_eq!(cpu_step(None, true), Some(30));
        assert_eq!(cpu_step(Some(5), false), None);
        assert_eq!(cpu_step(Some(95), true), Some(95));
        let mut v = None;
        for _ in 0..40 {
            v = cpu_step(v, true);
            assert!(v.is_some_and(|p| (1..=99).contains(&p)));
        }
        for _ in 0..40 {
            v = cpu_step(v, false);
        }
        assert_eq!(v, None);
    }

    #[test]
    fn after_cycles_through_the_others_and_back_to_nothing() {
        let o = vec!["a".to_string(), "b".to_string()];
        assert_eq!(after_step(None, &o, true), Some("a".into()));
        assert_eq!(after_step(Some("a"), &o, true), Some("b".into()));
        assert_eq!(after_step(Some("b"), &o, true), None);
        assert_eq!(after_step(None, &o, false), Some("b".into()));
        assert_eq!(after_step(Some("gone"), &o, true), Some("a".into()), "a missing target restarts the cycle");
        assert_eq!(after_step(None, &[], true), None);
    }

    #[test]
    fn the_editor_builds_only_valid_entries() {
        let mut ed = Editor::blank();
        assert!(ed.to_app().is_err(), "empty name and command");
        ed.name = "Steam".into();
        ed.command = r#""C:\Steam\steam.exe" -silent"#.into();
        ed.delay = "2.5".into();
        ed.cpu = Some(30);
        ed.process = " steam.exe ".into();
        ed.admin = true;
        let a = ed.to_app().unwrap();
        assert_eq!((a.delay_ms, a.cpu_below, a.admin, a.process.as_deref()), (2500, Some(30), true, Some("steam.exe")));
        ed.process = r"C:\x\steam.exe".into();
        assert!(ed.to_app().unwrap_err().to_lowercase().contains("process name"), "a path is not a process name");
        ed.process.clear();
        ed.delay = "9999".into();
        assert!(ed.to_app().is_err());
        ed.delay.clear();
        ed.name = "x".repeat(model::MAX_NAME + 1);
        assert!(ed.to_app().is_err());
    }

    #[test]
    fn taking_over_a_shortcut_quotes_its_path_and_keeps_run_values_raw() {
        let mut folder = entry("Disc ord.lnk", Scope::User, None);
        folder.id.kind = Kind::Folder;
        folder.raw = r"C:\Users\u\Start Menu\Startup\Disc ord.lnk".into();
        let ed = Editor::from_entry(&folder, adopt::Plan { lines: vec![], blocked: None });
        assert_eq!(ed.command, r#""C:\Users\u\Start Menu\Startup\Disc ord.lnk""#);
        assert_eq!(ed.name, "Disc ord");
        let mut run_e = entry("Steam", Scope::User, None);
        run_e.raw = r"%ProgramFiles%\Steam\steam.exe -silent".into();
        assert_eq!(Editor::from_entry(&run_e, adopt::Plan { lines: vec![], blocked: None }).command, run_e.raw, "%VARS% kept unexpanded");
    }

    #[test]
    fn typed_text_is_bounded_and_cleaned() {
        assert_eq!(clamp_text("a\tb\nc", 10), "abc");
        assert_eq!(clamp_text(&"x".repeat(500), model::MAX_NAME).chars().count(), model::MAX_NAME);
        assert_eq!(first_upper("name must be 1-80 characters"), "Name must be 1-80 characters.");
    }

    // ---- double-click cells, status toggle, context menu ----

    #[test]
    fn cell_hit_test_picks_the_column_by_x_and_nothing_outside() {
        let cols = [(Cell::AppState, 100.0, 170.0), (Cell::Delay, 170.0, 230.0), (Cell::After, 230.0, 340.0)];
        assert_eq!(cell_at(&cols, 100.0), Some(Cell::AppState));
        assert_eq!(cell_at(&cols, 169.9), Some(Cell::AppState));
        assert_eq!(cell_at(&cols, 170.0), Some(Cell::Delay));
        assert_eq!(cell_at(&cols, 339.0), Some(Cell::After));
        assert_eq!(cell_at(&cols, 340.0), None, "Last run / Command are not editable");
        assert_eq!(cell_at(&cols, 50.0), None, "Name is not editable in place");
        assert_eq!(cell_at(&[], 120.0), None);
    }

    #[test]
    fn toggle_decision_follows_every_gate() {
        use State::*;
        assert_eq!(toggle_for(Enabled, false, false, false, false), Toggle::Go(false));
        assert_eq!(toggle_for(Disabled, false, false, false, false), Toggle::Go(true));
        assert!(matches!(toggle_for(Enabled, false, false, true, true), Toggle::No(w) if w.contains("manages")), "managed: unmanage first");
        assert!(matches!(toggle_for(Unknown, false, false, false, true), Toggle::No(_)), "unrecognised state is never overwritten");
        assert!(matches!(toggle_for(Enabled, true, false, false, false), Toggle::No(w) if w.contains("administrator")), "all-users needs admin");
        assert_eq!(toggle_for(Enabled, true, false, false, true), Toggle::Go(false), "all-users with admin is allowed");
        assert!(matches!(toggle_for(Enabled, true, true, false, true), Toggle::No(w) if w.contains("Task Manager")), "user override on top");
        assert_eq!(status_hint(&Toggle::Go(false)), "Double-click to disable");
        assert_eq!(status_hint(&Toggle::Go(true)), "Double-click to enable");
        assert!(status_hint(&Toggle::No("x".into())).starts_with("Cannot change"));
    }

    fn texts(m: &[ME]) -> Vec<(String, bool)> {
        m.iter().filter_map(|e| if let ME::Item { text, enabled, .. } = e { Some((text.clone(), *enabled)) } else { None }).collect()
    }

    fn facts(toggle: Toggle) -> WinFacts {
        WinFacts { toggle, owned: false, is_hayate: false, needs_admin: false, locate: true }
    }

    #[test]
    fn windows_row_menu_per_state() {
        let t = texts(&win_menu(&facts(Toggle::Go(false))));
        assert_eq!(t[0], ("Disable".to_string(), true));
        assert!(t.contains(&("Manage in Hayate…".to_string(), true)));
        assert!(t.contains(&("Open file location".to_string(), true)) && t.contains(&("Copy command".to_string(), true)) && t.contains(&("Copy name".to_string(), true)));
        assert_eq!(texts(&win_menu(&facts(Toggle::Go(true))))[0].0, "Enable");
        // managed: toggle greyed with the reason, "Open in Hayate list" instead of Manage
        let m = texts(&win_menu(&WinFacts { owned: true, ..facts(toggle_for(State::Enabled, false, false, true, true)) }));
        assert_eq!(m[0], ("Enable / Disable".to_string(), false));
        assert!(m[1].0.contains("manages") && !m[1].1);
        assert!(m.contains(&("Open in Hayate list".to_string(), true)) && !m.iter().any(|(t, _)| t.starts_with("Manage")));
        // all-users, not admin: greyed with reason + the restart offer
        let m = texts(&win_menu(&WinFacts { needs_admin: true, ..facts(toggle_for(State::Enabled, true, false, false, false)) }));
        assert!(!m[0].1 && m[1].0.contains("administrator") && m.contains(&("Restart as administrator".to_string(), true)));
        // Hayate's own entry cannot be taken over; no path = no locate
        let m = texts(&win_menu(&WinFacts { is_hayate: true, locate: false, ..facts(Toggle::Go(false)) }));
        assert!(m.contains(&("Manage in Hayate…".to_string(), false)) && m.contains(&("Open file location".to_string(), false)));
    }

    #[test]
    fn app_row_menu_per_state() {
        let t = texts(&app_menu(&AppFacts { enabled: true, own: false, can_run: true, locate: true }));
        assert!(t.contains(&("Turn off".to_string(), true)) && t.contains(&("Set delay…".to_string(), true)) && t.contains(&("Run now".to_string(), true)));
        assert!(!t.iter().any(|(x, _)| x.starts_with("Unmanage")), "only a taken-over app can be given back");
        let t = texts(&app_menu(&AppFacts { enabled: false, own: true, can_run: false, locate: false }));
        assert!(t.contains(&("Turn on".to_string(), true)) && t.contains(&("Run now".to_string(), false)));
        assert!(t.iter().any(|(x, on)| x.starts_with("Unmanage") && *on) && t.contains(&("Open file location".to_string(), false)));
    }

    #[test]
    fn delay_input_validation_is_the_editors() {
        let a = MApp::new("a".into(), "A".into(), r"C:\x\a.exe".into());
        assert_eq!(with_delay(&a, "").unwrap().delay_ms, 0, "empty means no delay");
        assert_eq!(with_delay(&a, "2.5").unwrap().delay_ms, 2500);
        assert_eq!(with_delay(&a, "3600").unwrap().delay_ms, 3_600_000);
        assert!(with_delay(&a, "3601").is_err(), "above the range");
        assert!(with_delay(&a, "abc").is_err(), "non-numeric");
        assert!(with_delay(&a, "-1").is_err());
        assert!(with_delay(&a, "1.2.3").is_err());
        // only the delay changed
        let ok = with_delay(&a, "5").unwrap();
        assert_eq!((ok.name.as_str(), ok.command.as_str(), ok.enabled), ("A", r"C:\x\a.exe", true));
    }

    #[test]
    fn after_choice_menu_marks_the_current_and_skips_self() {
        let mut cfg = Config::default();
        let mut a = MApp::new("a".into(), "Alpha".into(), r"C:\x\a.exe".into());
        let b = MApp::new("b".into(), "Beta".into(), r"C:\x\b.exe".into());
        a.after = Some("b".into());
        cfg.apps = vec![a.clone(), b];
        let ctx = after_menu(&a, &cfg);
        let t: Vec<(String, bool)> = ctx.entries.iter().filter_map(|e| if let ME::Item { text, checked, .. } = e { Some((text.clone(), *checked)) } else { None }).collect();
        assert_eq!(t, vec![("Nothing (start right away)".to_string(), false), ("Beta".to_string(), true)]);
        assert_eq!(with_after(&a, None).unwrap().after, None);
        assert_eq!(with_after(&a, Some("b".into())).unwrap().after.as_deref(), Some("b"));
    }

    #[test]
    fn locate_paths() {
        let mut e = entry("x", Scope::User, None);
        e.command = r"C:\x\x.exe /s".into();
        assert!(locate_of(&e).is_some_and(|p| p.to_lowercase().ends_with("x.exe")));
        e.id = EntryId::new(Kind::Folder, Scope::User, View::Native, "y.lnk");
        e.command = r"C:\Users\u\Startup\y.lnk".into();
        assert_eq!(locate_of(&e).as_deref(), Some(r"C:\Users\u\Startup\y.lnk"));
        e.command.clear();
        assert_eq!(locate_of(&e), None);
    }
}
