//! Immediate-mode UI: every paint redraws the frame and records clickable
//! regions (`hits`); input handlers look those up and return an `Action`
//! for the window layer to run outside the state borrow (menus are modal).

pub mod cellhover;
pub mod cpu;
pub mod disallow;
pub mod history;
pub mod hwfacts;
pub mod mark;
pub mod icons;
pub mod nav;
pub mod order;
pub mod procmenu;
pub mod procs;
pub mod rulepeek;
pub mod rules;
pub mod sensors;
pub mod services;
pub mod svcprops;
pub mod setup;
pub mod startup;
pub mod vitals;
pub mod watch;

use crate::gfx::*;
use crate::power;
use crate::sample::Snapshot;
use std::sync::Arc;

// ---- palette v2 (docs/design-tokens.md). Solid colours only: tints are
// precomputed against PAN, so nothing here needs alpha.

// surfaces
pub const BG: u32 = 0x0f1217;
pub const PAN: u32 = 0x151a21;
/// every other row inside a panel
pub const ZEBRA: u32 = 0x181d24;
/// group / hour bands, hover, buttons, unread icon tile
pub const PAN2: u32 = 0x1b212a;
/// active chip / segment, count badges
pub const PAN3: u32 = 0x232a35;
/// panel borders, chip outlines
pub const LN: u32 = 0x29303b;
/// row separators, gridlines, header underlines
pub const SEP: u32 = 0x1e242d;
/// empty part of bars and meters
pub const TRACK: u32 = 0x222831;

// text tiers
/// names, primary values, unread titles
pub const TX1: u32 = 0xe8ebf0;
/// secondary: read rows, start type, panel titles
pub const TX2: u32 = 0xb3bac6;
/// meta: sublines, units, times, publisher
pub const TX3: u32 = 0x8a93a1;
/// column headers, axis ticks; never body text (fails AA by design)
pub const TX4: u32 = 0x626b78;
/// legacy names: TX is TX1, DIM is the meta tier
pub const TX: u32 = TX1;
pub const DIM: u32 = TX3;

// accent + components
pub const ACC: u32 = 0x56c8d8;
pub const C_CPU: u32 = ACC;
/// CCD0 / V-Cache
pub const C_CPU0: u32 = 0x9be3ec;
pub const C_MEM: u32 = 0xa993f0;
pub const C_GPU: u32 = 0x6cc79a;
pub const C_DSK: u32 = 0xe3a857;
pub const C_NET: u32 = 0x78aaf2;

// semantic: small badges, dots and 2 px rails only, never runs of text
pub const OK: u32 = 0x6cc79a;
pub const OK_BG: u32 = 0x213232;
#[allow(dead_code)] // full token sheet
pub const OK_LN: u32 = 0x33574b;
pub const OK_TX: u32 = 0xa9dcc2;
pub const WARN: u32 = 0xe3a857;
pub const WARN_BG: u32 = 0x322e29;
pub const WARN_LN: u32 = 0x5d4c34;
pub const WARN_TX: u32 = 0xecc890;
/// legacy name for WARN
pub const WARM: u32 = WARN;
pub const HOT: u32 = 0xef7070;
pub const HOT_BG: u32 = 0x34262c;
pub const HOT_LN: u32 = 0x61383d;
/// not in the token sheet: light red for text on HOT_BG (bad signature badge)
pub const HOT_TX: u32 = 0xf3b4b4;
/// camera / mic / location
pub const PRIV: u32 = 0xad97f2;
#[allow(dead_code)]
pub const PRIV_BG: u32 = 0x2a2c3e;
#[allow(dead_code)]
pub const PRIV_LN: u32 = 0x4a466a;
/// selected row fill (plus a 2 px ACC rail), selected tile border, text on it
pub const ACC_BG: u32 = 0x1e323b;
pub const ACC_LN: u32 = 0x2c5761;
pub const ACC_TX: u32 = 0xa6e0e8;
/// the unread-count badge on the rail
pub const BADGE: u32 = 0xd9534f;

/// 2 px severity rail on a row's left edge, inset 8 px top and bottom
pub fn rail(g: &Gfx, r: Rect, c: u32) {
    g.round(Rect::new(r.x, r.y + 8.0, 2.0, (r.h - 16.0).max(2.0)), 1.0, c);
}

/// Text widths remembered across frames, so per-row layout (a pill, a name
/// followed by a key) does not build a DirectWrite layout every paint.
#[derive(Default)]
pub struct Widths(std::collections::HashMap<(Font, String), f32>);

impl Widths {
    pub fn get(&mut self, g: &mut Gfx, s: &str, font: Font) -> f32 {
        if let Some(w) = self.0.get(&(font, s.to_string())) {
            return *w;
        }
        if self.0.len() > 4000 {
            self.0.clear();
        }
        let w = g.measure(s, font);
        self.0.insert((font, s.to_string()), w);
        w
    }
}

/// Width of a `chip`.
pub fn chip_w(wd: &mut Widths, g: &mut Gfx, label: &str, count: Option<usize>) -> f32 {
    let lw = wd.get(g, label, f(12.0));
    let cw = count.map(|n| wd.get(g, &n.to_string(), f(11.0).m()) + 6.0).unwrap_or(0.0);
    lw + cw + 21.0
}

/// Rounded filter chip at `x` (24 px tall); `count` is drawn in mono after the label.
#[allow(clippy::too_many_arguments)]
pub fn chip(wd: &mut Widths, g: &mut Gfx, x: f32, y: f32, label: &str, count: Option<usize>, on: bool, hov: bool) -> Rect {
    let r = Rect::new(x, y, chip_w(wd, g, label, count), 24.0);
    let lw = wd.get(g, label, f(12.0));
    g.round(r, 12.0, if on { PAN3 } else if hov { PAN2 } else { BG });
    g.round_stroke(r, 12.0, if on { 0x343c49 } else { LN }, 1.0);
    g.text(label, Rect::new(r.x + 10.0, r.y, lw + 2.0, r.h), f(12.0), if on { TX1 } else { TX2 });
    if let Some(n) = count {
        g.text(&n.to_string(), Rect::new(r.x + 10.0 + lw + 6.0, r.y, r.r() - (r.x + lw + 16.0), r.h), f(11.0).m(), if on { TX2 } else { TX3 });
    }
    r
}

/// Flat button: PAN2 fill, LN border, TX1 label.
pub fn button(g: &mut Gfx, r: Rect, label: &str, hov: bool, enabled: bool) {
    g.round(r, 6.0, if hov && enabled { PAN3 } else { PAN2 });
    g.round_stroke(r, 6.0, LN, 1.0);
    g.text(label, r, f(12.0).c(), if enabled { TX1 } else { TX4 });
}

/// Panel: PAN fill, 1 px LN border, radius 7.
pub fn panel(g: &Gfx, r: Rect) {
    g.round(r, 7.0, PAN);
    g.round_stroke(r, 7.0, LN, 1.0);
}

pub const TITLE_H: f32 = 36.0;
pub const RAIL_W: f32 = 46.0;
pub const STATUS_H: f32 = 24.0;
const CAP_W: f32 = 46.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Vitals,
    Processes,
    History,
    Startup,
    Services,
    Rules,
    Watch,
}
// Relaunch IDs retain the old eight-tab numbering across upgrades.
impl Tab {
    pub fn restore_id(self) -> usize {
        match self {
            Self::Vitals => 0, Self::Processes => 1, Self::History => 3,
            Self::Startup => 4, Self::Services => 5, Self::Rules => 6, Self::Watch => 7,
        }
    }
    pub fn from_restore_id(id: usize) -> Option<Self> {
        match id {
            0 | 2 => Some(Self::Vitals), 1 => Some(Self::Processes),
            3 => Some(Self::History), 4 => Some(Self::Startup),
            5 => Some(Self::Services), 6 => Some(Self::Rules), 7 => Some(Self::Watch),
            _ => None,
        }
    }
}
pub const TABS: [(Tab, &str, &str); 7] = [
    (Tab::Vitals, "\u{E9D9}", "Vitals"),
    (Tab::Processes, "\u{E8FD}", "Processes"),
    (Tab::History, "\u{E81C}", "History"),
    (Tab::Startup, "\u{E945}", "Startup"),
    (Tab::Services, "\u{E9F5}", "Services"),
    (Tab::Rules, "\u{E7C1}", "Rules"),
    (Tab::Watch, "\u{E7B3}", "Watch"),
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Hit {
    Caption,
    Min,
    Max,
    Close,
    Search,
    Power,
    Tab(usize),
    /// Vitals: device list entry, segmented-control option
    Device(usize),
    VitSeg(usize),
    /// Vitals > Network: "Restart as admin" in the per-process panel
    VitAdmin,
    Header(usize),
    Resize(usize),
    Row(usize),
    Chevron(usize),
    Scrollbar,
    /// a scroll bar drawn by `nav::paint_bar` (the id is one of `nav::BAR_*`)
    Bar(u8),
    Setting(usize),
    RuleRow(usize),
    RuleToggle(usize),
    Chip(usize),
    Button(usize),
    Alert(usize),
    Event(usize),
    Range(usize),
    Sensor(usize),
    Svc(usize),
    SvcBtn(usize),
    /// quick startup type: segment i of services::QUICK_ORDER
    SvcKind(usize),
    /// the Undo link on the header line after a startup change
    SvcUndo,
    SvcChip(usize),
    SvcGroup(usize),
    /// service Properties panel (ui/svcprops.rs)
    SvcProp(usize),
    /// status bar: "Update ready · … · restart" (elevated Hayate)
    Update,
    /// status bar: "… rolled back · click to try again" (manual update check)
    UpdateCheck,
    /// install bar, install panel, Settings (ui/setup.rs)
    Setup(usize),
    /// CPU affinity / CPU sets panel (ui/cpu.rs)
    Cpu(usize),
    /// Disallow running panel (ui/disallow.rs)
    Disallow(usize),
    /// Vitals > Network: the (i) next to the scope totals
    VitInfo,
    /// Processes: the rules badge of visible row i (ui/rulepeek.rs)
    RuleBadge(usize),
    /// Vitals: row i of the last paint's Top lists (right-click opens the process menu)
    TopRow(usize),
    /// rule peek panel: close, backdrop, buttons (ui/rulepeek.rs)
    Peek(usize),
    /// Processes hover card: background (0), copy command (1), copy path (2) (ui/cellhover.rs)
    Card(usize),
    /// Startup tab: toolbar, rows, details, editor and import panels (ui/startup.rs)
    Startup(usize),
}

/// Hover help: drawn last, near `anchor`, only while the pointer is on the control that set it.
pub struct Tip {
    pub anchor: Rect,
    /// first paragraph is the heading
    pub paras: Vec<String>,
}

pub enum Action {
    None,
    Repaint,
    Minimize,
    ToggleMax,
    Hide,
    PowerMenu(f32, f32),
    ColumnMenu(f32, f32),
    /// the process menu, for the target captured when it was asked for
    RowMenu(f32, f32, Box<procmenu::Target>),
    RuleMenu(f32, f32),
    EventMenu(f32, f32, usize),
    /// the Services menu, for the service captured (by name) when it was asked for
    SvcMenu(f32, f32, Box<crate::svcctl::Target>),
    StartupMenu(f32, f32, Box<startup::MenuCtx>),
    EndSelected,
    Capture,
    /// elevated relaunch of Hayate (UAC prompt), then close this instance
    RestartAdmin,
    /// run the PawnIO installer through winget (UAC prompt)
    InstallPawnIO,
    /// Watch: show a test toast
    TestToast,
    /// elevated: swap in the staged build, start it through UAC (tab index to restore)
    ApplyUpdate(usize),
    /// install panel: copy, shortcuts, key; then start the installed copy
    Install(crate::setup::logic::Choice),
    /// Settings: uninstall (true: also the data dir)
    Uninstall(bool),
    /// Settings, all-users install: the native confirm flow (UAC first when not elevated)
    UninstallAll,
    /// "Install for everyone on this PC" (UAC)
    InstallAll(crate::setup::logic::Choice),
    /// Settings: turn the Task Manager redirect on (true) or off (UAC, elevated helper)
    TaskMgr(bool),
    /// Settings: replace the protected launcher copy with this Hayate (UAC, elevated helper)
    TaskMgrRefresh,
    /// copy this text to the clipboard
    Clip(String),
    /// UI zoom step / reset (Ctrl +/-/0, Ctrl+wheel, Settings)
    Zoom(crate::zoom::Cmd),
}

pub struct App {
    pub tab: Tab,
    pub search: String,
    pub search_focus: bool,
    pub mx: f32,
    pub my: f32,
    pub hover: Option<Hit>,
    pub down: Option<Hit>,
    pub hits: Vec<(Rect, Hit)>,
    /// the scroll bars of the last paint and the drag in progress (ui/nav.rs)
    pub bars: nav::Bars,
    pub snap: Arc<Snapshot>,
    /// fingerprint of what the Processes tab shows + ticks since last repaint
    fp: u64,
    paint_ms: f32,
    quiet_ticks: u32,
    pub procs: procs::ProcView,
    pub icons: icons::Icons,
    pub vit: vitals::VitState,
    pub rules: rules::RulesView,
    pub startup: startup::StartupView,
    pub services: services::ServicesView,
    pub sensors: sensors::SensorsView,
    pub history: history::HistoryView,
    pub watch: watch::WatchView,
    pub setup: setup::SetupView,
    pub cpu: Option<cpu::CpuPanel>,
    pub disallow: Option<disallow::DisallowPanel>,
    /// service Properties panel (ui/svcprops.rs)
    pub svcprops: Option<svcprops::Props>,
    /// rules quick view for one process / group (ui/rulepeek.rs)
    pub peek: Option<rulepeek::Peek>,
    pub tip: Option<Tip>,
    /// when the "Zoom 125%" indicator was last asked for (shown ~1.5 s)
    pub zoom_shown: Option<std::time::Instant>,
    pub plans: Vec<power::Plan>,
    pub plan_active: Option<windows::core::GUID>,
    pub maximized: bool,
    pub w: f32,
    pub h: f32,
    pub own_pid: u32,
    /// a context menu is open (modal loop): list orders hold under it
    pub menu_open: bool,
    /// Processes hover help: dwell, open card (ui/cellhover.rs)
    pub hov: cellhover::HoverState,
    /// a paint asks to be repainted after this many ms (the hover dwell); taken by the window
    pub timer_ms: Option<u32>,
}

impl App {
    pub fn new() -> Self {
        App {
            tab: start_tab(),
            search: String::new(),
            search_focus: false,
            mx: -1.0,
            my: -1.0,
            hover: None,
            down: None,
            hits: Vec::new(),
            bars: nav::Bars::default(),
            snap: Arc::new(Snapshot::default()),
            fp: 0,
            paint_ms: 0.0,
            quiet_ticks: 0,
            procs: procs::ProcView::new(),
            icons: icons::Icons::default(),
            vit: vitals::VitState::default(),
            rules: rules::RulesView::default(),
            startup: startup::StartupView::default(),
            services: services::ServicesView::default(),
            sensors: sensors::SensorsView::default(),
            history: history::HistoryView::default(),
            watch: watch::WatchView::default(),
            setup: setup::SetupView::new(),
            cpu: None,
            disallow: None,
            svcprops: None,
            peek: None,
            tip: None,
            zoom_shown: None,
            plans: power::plans(),
            plan_active: power::active(),
            maximized: false,
            w: 0.0,
            h: 0.0,
            own_pid: std::process::id(),
            menu_open: false,
            hov: cellhover::HoverState::default(),
            timer_ms: None,
        }
    }

    /// A context menu opens / closes. The close starts the 2 s thaw delay of every held list order.
    pub fn set_menu(&mut self, open: bool) {
        self.menu_open = open;
        if !open {
            self.procs.touch();
            self.vit.touch();
            self.vit.pick = None;
        }
    }

    /// Takes the new sample; returns whether the window needs a repaint.
    /// On the Processes tab an unchanged table (quantised to what the columns
    /// show) is skipped, with a refresh at least every 4 ticks for the status bar.
    pub fn set_snapshot(&mut self, s: Arc<Snapshot>) -> bool {
        use std::hash::{Hash, Hasher};
        self.snap = s;
        self.procs.dirty = true;
        if self.tab != Tab::Processes {
            self.fp = 0;
            self.quiet_ticks = 0;
            return true;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let s = &self.snap;
        (s.nprocs, s.threads, s.handles, s.uptime / 60, s.cpu as u32, s.mem.avail >> 20).hash(&mut h);
        for p in &s.procs {
            (p.pid, (p.cpu * 10.0) as u32, p.mem >> 16, (p.io_bps as u64) >> 10, p.threads, p.handles, (p.gpu * 10.0) as u32).hash(&mut h);
        }
        let fp = h.finish();
        self.quiet_ticks += 1;
        // a held row order that is now due to update must show even when no value moved
        if fp != self.fp || self.quiet_ticks >= 4 || self.procs.due(order::now_ms()) {
            self.fp = fp;
            self.quiet_ticks = 0;
            true
        } else {
            false
        }
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    /// For WM_NCHITTEST: true when the point is plain title bar (drag area).
    pub fn is_caption(&self, x: f32, y: f32) -> bool {
        y < TITLE_H && matches!(self.hit(x, y), Some(Hit::Caption) | None)
    }

    pub fn paint(&mut self, g: &mut Gfx) {
        let t0 = std::time::Instant::now();
        self.paint_inner(g);
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        self.paint_ms = if self.paint_ms == 0.0 { ms } else { self.paint_ms * 0.9 + ms * 0.1 };
    }

    fn paint_inner(&mut self, g: &mut Gfx) {
        let (w, h) = g.size();
        self.w = w;
        self.h = h;
        self.hits.clear();
        self.bars.begin_paint();
        self.tip = None;
        if !g.begin(BG) {
            return;
        }
        let full = Rect::new(RAIL_W, TITLE_H, w - RAIL_W, h - TITLE_H - STATUS_H);
        let body = setup::bar(self, g, full);
        // per-process GPU is sampled only while it is on screen
        crate::sample::PROC_GPU.store(
            (self.tab == Tab::Processes && self.procs.wants_gpu()) || (self.tab == Tab::Vitals && matches!(self.vit.sel, vitals::Dev::Gpu(_))),
            std::sync::atomic::Ordering::Relaxed,
        );
        crate::sample::VIT_SEL.store(vitals::sel_code(self), std::sync::atomic::Ordering::Relaxed);
        crate::sample::VIT_NIC.store(if let vitals::Dev::Nic(id) = self.vit.sel { id } else { 0 }, std::sync::atomic::Ordering::Relaxed);
        // CPU temperature reads every second only while the CPU detail (its Sensors panel) is shown
        crate::sample::SENSORS_TAB.store(self.tab == Tab::Vitals && matches!(self.vit.sel, vitals::Dev::Cpu), std::sync::atomic::Ordering::Relaxed);
        // finished icon extractions (every tab with names draws icons)
        self.icons.pump();
        match self.tab {
            Tab::Vitals => vitals::paint(self, g, body),
            Tab::Processes => procs::paint(self, g, body),
            Tab::Startup => startup::paint(self, g, body),
            Tab::Services => services::paint(self, g, body),
            Tab::History => history::paint(self, g, body),
            Tab::Rules => rules::paint(self, g, body),
            Tab::Watch => watch::paint(self, g, body),
        }
        self.paint_title(g, w);
        self.paint_rail(g, h);
        self.paint_status(g, w, h);
        setup::overlay(self, g, full);
        cpu::overlay(self, g, full);
        disallow::overlay(self, g, full);
        svcprops::overlay(self, g, full);
        rulepeek::overlay(self, g, full);
        startup::overlay(self, g, full);
        cellhover::overlay(self, g);
        self.draw_tip(g);
        self.draw_zoom_toast(g, w, h);
        g.end();
    }

    /// Transient "Zoom 125%" pill above the status bar, 1.5 s after the last change.
    fn draw_zoom_toast(&mut self, g: &mut Gfx, w: f32, h: f32) {
        let Some(t) = self.zoom_shown else { return };
        if t.elapsed() > std::time::Duration::from_millis(1500) {
            self.zoom_shown = None;
            return;
        }
        let txt = format!("Zoom {}%", crate::zoom::percent());
        let tw = g.measure(&txt, f(13.0).b()) + 28.0;
        let r = Rect::new((w - tw) / 2.0, h - STATUS_H - 44.0, tw, 30.0);
        g.round(r, 8.0, PAN3);
        g.round_stroke(r, 8.0, LN, 1.0);
        g.text(&txt, r, f(13.0).b().c(), TX1);
    }

    /// Ask for a hover tip: only when the pointer is on `hit` (so nothing is built otherwise).
    pub fn hovering(&self, hit: Hit) -> bool {
        self.hover == Some(hit) && self.down.is_none()
    }

    pub fn set_tip(&mut self, anchor: Rect, paras: Vec<String>) {
        self.tip = Some(Tip { anchor, paras });
    }

    /// Tooltip: 300 px at most, under the anchor (above when there is no room), kept inside the window.
    fn draw_tip(&mut self, g: &mut Gfx) {
        let Some(t) = self.tip.take() else { return };
        let (w, h) = (self.w, self.h);
        let tw = (w - 24.0).clamp(160.0, 320.0);
        let iw = tw - 24.0;
        let mut lines: Vec<(String, bool)> = Vec::new();
        for (i, p) in t.paras.iter().enumerate() {
            for l in cpu::wrap(g, p, iw, f(11.5), 6) {
                lines.push((l, i == 0));
            }
        }
        if lines.is_empty() {
            return;
        }
        let th = lines.len() as f32 * 16.0 + 18.0 + (t.paras.len().saturating_sub(1)) as f32 * 4.0;
        let a = t.anchor;
        let mut x = a.x;
        x = x.min(w - tw - 8.0).max(8.0);
        let mut y = a.b() + 6.0;
        if y + th > h - STATUS_H - 4.0 {
            y = (a.y - th - 6.0).max(TITLE_H + 4.0);
        }
        let r = Rect::new(x, y, tw, th);
        g.round(r, 7.0, PAN3);
        g.round_stroke(r, 7.0, LN, 1.0);
        let mut ly = r.y + 9.0;
        let mut prev_head = true;
        for (l, head) in lines {
            if prev_head && !head {
                ly += 4.0;
            }
            prev_head = head;
            g.text(&l, Rect::new(r.x + 12.0, ly, iw, 16.0), if head { f(11.5).b() } else { f(11.5) }, if head { TX1 } else { TX2 });
            ly += 16.0;
        }
        self.tip = Some(t);
    }

    fn paint_title(&mut self, g: &mut Gfx, w: f32) {
        let bar = Rect::new(0.0, 0.0, w, TITLE_H);
        g.fill(bar, BG);
        g.fill(Rect::new(0.0, TITLE_H - 1.0, w, 1.0), SEP);
        self.hits.push((bar, Hit::Caption));
        // Hayate mark: tile + italic H, origin snapped to a device pixel
        let sc = g.dpi / 96.0;
        let (mx, my) = ((11.0 * sc).round() / sc, (9.0 * sc).round() / sc);
        g.round(Rect::new(mx, my, 18.0, 18.0), 3.5, PAN);
        for p in mark::polys(mx, my, 18.0) {
            g.fill_poly(&p, ACC);
        }
        g.text(crate::osutil::APP_NAME, Rect::new(36.0, 0.0, 47.0, TITLE_H), f(12.5).b(), TX);

        // search box
        let sr = Rect::new(84.0, 6.0, 260.0f32.min(w - 400.0).max(120.0), 24.0);
        let focus = self.search_focus;
        g.round(sr, 6.0, PAN);
        g.round_stroke(sr, 6.0, if focus { ACC } else { LN }, 1.0);
        g.text("\u{E721}", Rect::new(sr.x + 8.0, sr.y, 16.0, sr.h), Font { mono: false, ..f(11.0) }.icon(), DIM);
        if self.search.is_empty() {
            g.text(if focus { "Type to filter…" } else { "Name, publisher, PID or command line" }, Rect::new(sr.x + 26.0, sr.y, sr.w - 70.0, sr.h), f(12.0), DIM);
        } else {
            let caret = if focus { "▏" } else { "" };
            g.text(&format!("{}{caret}", self.search), Rect::new(sr.x + 26.0, sr.y, sr.w - 70.0, sr.h), f(12.0), TX);
        }
        let kr = Rect::new(sr.r() - 46.0, sr.y + 4.0, 40.0, 16.0);
        g.round_stroke(kr, 3.0, LN, 1.0);
        g.text("Ctrl F", kr, f(10.5).m().c(), DIM);
        self.hits.push((sr, Hit::Search));

        // caption buttons
        let caps = [(Hit::Close, "\u{E8BB}"), (Hit::Max, if self.maximized { "\u{E923}" } else { "\u{E922}" }), (Hit::Min, "\u{E921}")];
        let mut x = w;
        for (hit, glyph) in caps {
            x -= CAP_W;
            let r = Rect::new(x, 0.0, CAP_W, TITLE_H - 1.0);
            let hov = self.hover == Some(hit);
            if hov {
                // close keeps the stock Windows caption red
                g.fill(r, if hit == Hit::Close { 0xc42b1c } else { PAN2 });
            }
            g.text(glyph, r, f(10.0).icon().c(), if hov && hit == Hit::Close { 0xffffff } else { TX });
            self.hits.push((r, hit));
        }

        // power plan
        let name = self
            .plan_active
            .and_then(|a| self.plans.iter().find(|p| p.guid == a))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Power".into());
        let tw = g.measure(&name, f(12.0)) + 46.0;
        let pr = Rect::new(x - tw - 10.0, 6.0, tw, 24.0);
        let hov = self.hover == Some(Hit::Power);
        g.round(pr, 6.0, if hov { PAN2 } else { BG });
        g.round_stroke(pr, 6.0, LN, 1.0);
        g.text("\u{E7E8}", Rect::new(pr.x + 8.0, pr.y, 15.0, pr.h), f(12.0).icon().c(), power::color(&name));
        g.text(&name, Rect::new(pr.x + 27.0, pr.y, pr.w - 40.0, pr.h), f(12.0), TX2);
        g.text("\u{E70D}", Rect::new(pr.r() - 16.0, pr.y, 10.0, pr.h), f(8.0).icon(), TX3);
        self.hits.push((pr, Hit::Power));
        if self.hovering(Hit::Power) {
            self.set_tip(pr, vec!["Windows power plan".into(), format!("Current: {name}. Click to choose a power plan.")]);
        }
    }

    fn paint_rail(&mut self, g: &mut Gfx, h: f32) {
        let r = Rect::new(0.0, TITLE_H, RAIL_W, h - TITLE_H - STATUS_H);
        g.fill(r, BG);
        g.fill(Rect::new(RAIL_W - 1.0, TITLE_H, 1.0, r.h), SEP);
        let mut tip: Option<(f32, &str)> = None;
        let unread = watch::unread();
        for (i, (tab, glyph, label)) in TABS.iter().enumerate() {
            // Rules + Watch sit in a second group below a divider
            let gap = if i >= 5 { 14.0 } else { 0.0 };
            let br = Rect::new(0.0, TITLE_H + 6.0 + i as f32 * 40.0 + gap, RAIL_W - 1.0, 38.0);
            if i == 5 {
                g.fill(Rect::new(12.0, br.y - 8.0, RAIL_W - 25.0, 1.0), LN);
            }
            if *tab == Tab::Watch && unread > 0 {
                let t = if unread > 99 { "99+".to_string() } else { unread.to_string() };
                let bw = g.measure(&t, f(9.5).b()) + 8.0;
                let bx = Rect::new(br.x + 24.0, br.y + 4.0, bw.max(15.0), 15.0);
                g.round(bx, 7.5, BADGE);
                g.text(&t, bx, f(9.5).b().c(), 0xffffff);
            }
            let on = self.tab == *tab;
            let hov = self.hover == Some(Hit::Tab(i));
            if on {
                g.round(Rect::new(0.0, br.y + 10.0, 2.0, 18.0), 1.0, ACC);
            }
            g.text(glyph, br, f(15.0).icon().c(), if on || hov { TX1 } else { TX3 });
            if hov {
                tip = Some((br.y, label));
            }
            self.hits.push((br, Hit::Tab(i)));
        }
        if let Some((y, label)) = tip {
            let mut txt = format!("{label}  ·  {}", TABS.iter().position(|t| t.2 == label).unwrap() + 1);
            if nav::arrows_switch_tabs(self.tab) {
                txt.push_str("  ·  ← →");
            }
            let tw = g.measure(&txt, f(11.5)) + 16.0;
            let tr = Rect::new(RAIL_W + 2.0, y + 8.0, tw, 22.0);
            g.round(tr, 5.0, PAN3);
            g.round_stroke(tr, 5.0, LN, 1.0);
            g.text(&txt, tr, f(11.5).c(), TX1);
        }
        setup::rail_button(self, g, h);
    }

    fn paint_status(&mut self, g: &mut Gfx, w: f32, h: f32) {
        let r = Rect::new(0.0, h - STATUS_H, w, STATUS_H);
        g.fill(r, BG);
        g.fill(Rect::new(0.0, r.y, w, 1.0), SEP);
        let s = &self.snap;
        let up = s.uptime;
        let items = [
            ("Up", format!("{}d {:02}:{:02}", up / 86400, up / 3600 % 24, up / 60 % 60)),
            ("Procs", format!("{}", s.nprocs)),
            ("Threads", group_num(s.threads as u64)),
            ("Handles", group_num(s.handles as u64)),
        ];
        // right-hand text: the richest variant that leaves room for the left items; none if even the shortest does not fit
        let me = s.procs.iter().find(|p| p.pid == self.own_pid);
        let mut variants: Vec<String> = Vec::new();
        match me {
            Some(p) => {
                variants.push(format!("Hayate {:.1}% · {:.1} MB · sample {:.1} ms · paint {:.1} ms", p.cpu, p.mem as f64 / 1048576.0, s.sample_ms, self.paint_ms));
                variants.push(format!("Hayate {:.1}% · {:.1} MB", p.cpu, p.mem as f64 / 1048576.0));
            }
            None => variants.push(format!("sample {:.1} ms · paint {:.1} ms", s.sample_ms, self.paint_ms)),
        }
        let left_w: f32 = items.iter().map(|(k, v)| g.measure(k, f(11.0)) + 5.0 + g.measure(v, f(11.0).b().m()) + 14.0).sum::<f32>() + 10.0;
        let mut own = String::new();
        let mut limit = w - 10.0;
        for t in variants {
            let tw = g.measure(&t, f(11.0).m());
            if w - 10.0 - tw - 12.0 >= left_w {
                limit = w - 10.0 - tw - 12.0;
                own = t;
                break;
            }
        }
        let mut x = 10.0;
        for (k, v) in items {
            let kw = g.measure(k, f(11.0));
            let vw = g.measure(&v, f(11.0).b().m());
            if x + kw + 5.0 + vw > limit {
                break;
            }
            g.text(k, Rect::new(x, r.y, kw + 2.0, r.h), f(11.0), TX3);
            x += kw + 5.0;
            g.text(&v, Rect::new(x, r.y, vw + 2.0, r.h), f(11.0).b().m(), TX2);
            x += vw + 14.0;
        }
        // auto-update: version + last check, or a one-hour "Updated to …" note
        if let Some((t, hi)) = crate::update::status_line(w > 1300.0) {
            let c = match hi {
                Some(true) => OK,
                Some(false) => WARM,
                None => DIM,
            };
            let room = (w - 470.0 - x).min(limit - x);
            if room > 120.0 {
                g.text(&t, Rect::new(x, r.y, room, r.h), f(11.0), c);
                let act = if crate::update::offer() {
                    Some(Hit::Update)
                } else if crate::update::retry_offer() {
                    Some(Hit::UpdateCheck)
                } else {
                    None
                };
                if let Some(hit) = act {
                    let tw = g.measure(&t, f(11.0)).min(room);
                    self.hits.push((Rect::new(x - 4.0, r.y, tw + 8.0, r.h), hit));
                    if self.hover == Some(hit) {
                        g.fill(Rect::new(x, r.y + r.h - 6.0, tw, 1.0), c);
                    }
                }
            }
        }
        if !own.is_empty() {
            g.text(&own, Rect::new(w - 460.0, r.y, 450.0, r.h), f(11.0).m().r(), DIM);
        }
    }

    // ---------- input ----------

    pub fn mouse_move(&mut self, x: f32, y: f32) -> Action {
        self.mx = x;
        self.my = y;
        if let Some(Hit::Resize(i)) = self.down {
            self.procs.drag_resize(i, x);
            return Action::Repaint;
        }
        if self.down == Some(Hit::Scrollbar) {
            self.procs.drag_scroll(y);
            return Action::Repaint;
        }
        if let Some(Hit::Bar(id)) = self.down {
            self.set_bar_scroll(id, self.bars.drag_to(id, y));
            return Action::Repaint;
        }
        let h = self.hit(x, y);
        let h = if h == Some(Hit::Caption) { None } else { h };
        let hover_plot = vitals::hover(self, x, y);
        // History crosshair follows the mouse (throttled to 3 px steps)
        if self.tab == Tab::History && ((x - self.history.hover_x).abs() >= 3.0 || (y - self.history.hover_y).abs() >= 3.0) {
            self.history.hover_x = x;
            self.history.hover_y = y;
            self.hover = h;
            return Action::Repaint;
        }
        // Vitals Sensors graph crosshair follows the mouse (2 px steps)
        if self.tab == Tab::Vitals && sensors::hover_moved(self, h, x) {
            self.hover = h;
            return Action::Repaint;
        }
        // Processes: crossing into another cell of the same row restarts the hover dwell
        let cell = if self.tab == Tab::Processes { self.procs.cell_at(x, y) } else { None };
        if h != self.hover || hover_plot || cell != self.hov.last_cell {
            self.hover = h;
            return Action::Repaint;
        }
        Action::None
    }

    pub fn mouse_leave(&mut self) -> Action {
        self.mx = -1.0;
        self.hover = None;
        vitals::leave(self);
        Action::Repaint
    }

    pub fn mouse_down(&mut self, x: f32, y: f32) -> Action {
        // a press anywhere but on the hover card closes it (the press itself still acts)
        let closed = !matches!(self.hit(x, y), Some(Hit::Card(_))) && self.hov.press();
        match self.mouse_down_inner(x, y) {
            Action::None if closed => Action::Repaint,
            a => a,
        }
    }

    fn mouse_down_inner(&mut self, x: f32, y: f32) -> Action {
        let h = self.hit(x, y);
        self.down = h;
        match h {
            Some(Hit::Resize(i)) => {
                self.procs.begin_resize(i, x);
                Action::Capture
            }
            Some(Hit::Scrollbar) => {
                self.procs.begin_scroll(y);
                Action::Capture
            }
            Some(Hit::Bar(id)) => {
                let to = self.bars.press(id, y);
                self.set_bar_scroll(id, to);
                Action::Capture
            }
            Some(Hit::Search) => {
                self.search_focus = true;
                Action::Repaint
            }
            Some(Hit::Row(i)) => {
                self.search_focus = false;
                self.procs.select_row(i);
                Action::Repaint
            }
            Some(Hit::Chevron(i)) => {
                // remember what is under the press: the release toggles only that same row
                self.procs.press_row(i);
                Action::None
            }
            Some(Hit::RuleBadge(i)) => {
                // like a row press: select what is under the pointer now; the release acts only on that same process
                self.search_focus = false;
                self.procs.select_row(i);
                Action::Repaint
            }
            _ => {
                if self.search_focus {
                    self.search_focus = false;
                    return Action::Repaint;
                }
                Action::None
            }
        }
    }

    pub fn mouse_up(&mut self, x: f32, y: f32) -> Action {
        let d = self.down.take();
        let h = self.hit(x, y);
        if matches!(d, Some(Hit::Bar(_))) {
            self.bars.end_drag();
            // a Vitals Top order thaws 2 s after the last interaction, the drag included
            self.vit.touch();
        }
        if matches!(d, Some(Hit::Resize(_)) | Some(Hit::Scrollbar) | Some(Hit::Bar(_))) {
            return Action::Repaint;
        }
        if d != h {
            return Action::None;
        }
        // same row index is not enough: the list may have changed between press and release
        if let Some(Hit::Chevron(i) | Hit::RuleBadge(i)) = h {
            if !self.procs.same_press(i) {
                return Action::None;
            }
        }
        match h {
            Some(Hit::Close) => Action::Hide,
            Some(Hit::Min) => Action::Minimize,
            Some(Hit::Max) => Action::ToggleMax,
            Some(Hit::Tab(i)) => {
                self.switch_tab(TABS[i].0);
                Action::Repaint
            }
            Some(Hit::Power) => {
                let r = self.hits.iter().find(|(_, h)| *h == Hit::Power).map(|(r, _)| *r).unwrap_or_default();
                Action::PowerMenu(r.x, r.b() + 4.0)
            }
            Some(h @ (Hit::Device(_) | Hit::VitSeg(_) | Hit::VitAdmin)) => vitals::click(self, h),
            Some(Hit::VitInfo) => Action::None,
            Some(Hit::RuleBadge(i)) => rulepeek::badge_click(self, i),
            Some(h @ Hit::Peek(_)) => rulepeek::click(self, h),
            Some(h @ Hit::Card(_)) => cellhover::click(self, h),
            Some(Hit::Header(i)) => {
                self.procs.sort_by(i);
                Action::Repaint
            }
            Some(Hit::Chevron(i)) => {
                self.procs.toggle_row(i);
                Action::Repaint
            }
            Some(h @ (Hit::Setting(_) | Hit::RuleRow(_) | Hit::RuleToggle(_))) => rules::click(self, h),
            Some(h @ (Hit::Chip(_) | Hit::Button(_) | Hit::Alert(_) | Hit::Event(_))) => watch::click(self, h),
            Some(h @ Hit::Range(_)) => history::click(self, h),
            Some(h @ (Hit::Svc(_) | Hit::SvcBtn(_) | Hit::SvcKind(_) | Hit::SvcUndo | Hit::SvcChip(_) | Hit::SvcGroup(_))) => services::click(self, h),
            Some(Hit::Sensor(i)) => sensors::click(self, i),
            Some(h @ Hit::Setup(_)) => setup::click(self, h),
            Some(h @ Hit::Startup(_)) => startup::click(self, h),
            Some(h @ Hit::Cpu(_)) => cpu::click(self, h),
            Some(h @ Hit::Disallow(_)) => disallow::click(self, h),
            Some(h @ Hit::SvcProp(_)) => svcprops::click(self, h),
            Some(Hit::Update) => Action::ApplyUpdate(self.tab.restore_id()),
            Some(Hit::UpdateCheck) => {
                crate::update::check_now();
                Action::Repaint
            }
            _ => Action::None,
        }
    }

    pub fn double_click(&mut self, x: f32, y: f32) -> Action {
        match self.hit(x, y) {
            Some(Hit::Row(i)) => {
                // only the row the first click of the pair landed on
                if self.procs.same_press(i) {
                    self.procs.toggle_row(i);
                }
                Action::Repaint
            }
            Some(Hit::Startup(_)) => startup::double_click(self, x, y),
            None | Some(Hit::Caption) if y < TITLE_H => Action::ToggleMax,
            _ => Action::None,
        }
    }

    pub fn right_click(&mut self, x: f32, y: f32) -> Action {
        match self.hit(x, y) {
            Some(Hit::Header(_)) | Some(Hit::Resize(_)) => Action::ColumnMenu(x, y),
            Some(Hit::Row(i)) | Some(Hit::Chevron(i)) | Some(Hit::RuleBadge(i)) => {
                // select what is under the pointer now; an exited slot / header opens no menu
                self.procs.select_row(i);
                if self.procs.sel.is_none() {
                    return Action::Repaint;
                }
                // the menu's target is fixed here; a row whose processes are gone opens none
                let snap = self.snap.clone();
                match self.procs.menu_target(&snap) {
                    Some(t) => Action::RowMenu(x, y, Box::new(t)),
                    None => Action::Repaint,
                }
            }
            Some(Hit::TopRow(i)) => match vitals::right_click(self, i) {
                Some(t) => Action::RowMenu(x, y, Box::new(t)),
                None => Action::Repaint,
            },
            Some(Hit::RuleRow(i)) | Some(Hit::RuleToggle(i)) => {
                rules::click(self, Hit::RuleRow(i));
                Action::RuleMenu(x, y)
            }
            Some(Hit::Event(i)) => Action::EventMenu(x, y, i),
            Some(Hit::Svc(i)) => services::right_click(self, i, x, y),
            Some(Hit::Startup(i)) => startup::right_click(self, i, x, y),
            _ => Action::None,
        }
    }

    /// The scroll offset a `Hit::Bar(id)` drives.
    fn bar_scroll(&mut self, id: u8) -> Option<&mut f32> {
        Some(match id {
            nav::BAR_HISTORY => &mut self.history.scroll,
            nav::BAR_SERVICES => &mut self.services.scroll,
            nav::BAR_STARTUP | nav::BAR_STARTUP_EDIT | nav::BAR_STARTUP_IMPORT => return startup::bar_scroll(self, id),
            nav::BAR_RULES => &mut self.rules.scroll,
            nav::BAR_WATCH => &mut self.watch.scroll,
            nav::BAR_VIT_LIST => &mut self.vit.list_scroll,
            nav::BAR_VIT_DETAIL => &mut self.vit.scroll,
            nav::BAR_SETTINGS => &mut self.setup.scroll,
            _ => return None,
        })
    }

    /// A bar press or drag moved to `to`: the view clamps it at its next paint, like a wheel notch.
    fn set_bar_scroll(&mut self, id: u8, to: Option<f32>) {
        let Some(to) = to else { return };
        if let Some(s) = self.bar_scroll(id) {
            *s = to;
        }
        if self.tab == Tab::Vitals {
            self.vit.touch();
        }
    }

    pub fn wheel(&mut self, delta: f32, shift: bool) -> Action {
        if cellhover::wheel(self, delta) || rulepeek::wheel(self, delta) || cpu::wheel(self, delta) || disallow::wheel(self, delta) || svcprops::wheel(self, delta) || setup::wheel(self, delta) || startup::wheel(self, delta) {
            return Action::Repaint;
        }
        match self.tab {
            Tab::Processes => {
                if shift {
                    self.procs.hscroll(-delta);
                } else {
                    self.procs.scroll(-delta);
                }
                Action::Repaint
            }
            Tab::Rules => {
                self.rules.scroll -= delta * 0.5;
                Action::Repaint
            }
            Tab::Startup => {
                self.startup.scroll -= delta * 0.6;
                Action::Repaint
            }
            Tab::Services => {
                self.services.scroll -= delta * 0.6;
                Action::Repaint
            }
            Tab::History => {
                self.history.scroll -= delta * 0.6;
                Action::Repaint
            }
            Tab::Watch => {
                self.watch.scroll -= delta * 0.6;
                Action::Repaint
            }
            Tab::Vitals => {
                if vitals::wheel(self, delta) {
                    Action::Repaint
                } else {
                    Action::None
                }
            }
        }
    }

    /// WM_KEYDOWN. Returns None when the key was not handled.
    pub fn key(&mut self, vk: u16, ctrl: bool, shift: bool) -> Option<Action> {
        const VK_BACK: u16 = 0x08;
        const VK_RETURN: u16 = 0x0D;
        const VK_ESC: u16 = 0x1B;
        const VK_PRIOR: u16 = 0x21;
        const VK_NEXT: u16 = 0x22;
        const VK_END: u16 = 0x23;
        const VK_HOME: u16 = 0x24;
        const VK_LEFT: u16 = 0x25;
        const VK_UP: u16 = 0x26;
        const VK_RIGHT: u16 = 0x27;
        const VK_DOWN: u16 = 0x28;
        const VK_DELETE: u16 = 0x2E;
        if let Some(c) = crate::zoom::key_cmd(vk, ctrl) {
            return Some(Action::Zoom(c));
        }
        // the Startup editor / import panel own the keyboard while open
        if let Some(a) = startup::key(self, vk, ctrl, shift) {
            return Some(a);
        }
        if ctrl && vk == b'F' as u16 {
            self.search_focus = true;
            return Some(Action::Repaint);
        }
        // Processes: the detail card of the selected row from the keyboard
        if ctrl && !shift && vk == b'I' as u16 && self.tab == Tab::Processes && self.peek.is_none() && self.cpu.is_none() && self.setup.panel.is_none() {
            return Some(cellhover::toggle_key(self));
        }
        if !ctrl && self.hov.card.as_ref().is_some_and(|c| c.via_key) && [VK_PRIOR, VK_NEXT].contains(&vk) {
            cellhover::wheel(self, if vk == VK_PRIOR { 240.0 } else { -240.0 });
            return Some(Action::Repaint);
        }
        if ctrl && shift && vk == b'P' as u16 {
            self.cycle_plan();
            return Some(Action::Repaint);
        }
        if !ctrl {
            if let Some(a) = self.nav_key(vk) {
                return Some(a);
            }
        }
        match vk {
            VK_ESC => {
                if self.peek.is_some() {
                    rulepeek::close(self);
                    return Some(Action::Repaint);
                }
                if self.cpu.is_some() {
                    cpu::close(self);
                    return Some(Action::Repaint);
                }
                if self.disallow.is_some() {
                    disallow::close(self);
                    return Some(Action::Repaint);
                }
                if self.svcprops.is_some() {
                    svcprops::close(self);
                    return Some(Action::Repaint);
                }
                if self.setup.panel.is_some() {
                    setup::close(self);
                    return Some(Action::Repaint);
                }
                if self.hov.dismiss_card() {
                    return Some(Action::Repaint);
                }
                if !self.search.is_empty() {
                    self.search.clear();
                    self.procs.dirty = true;
                    return Some(Action::Repaint);
                }
                if self.search_focus {
                    self.search_focus = false;
                    return Some(Action::Repaint);
                }
                if self.clear_selection() {
                    return Some(Action::Repaint);
                }
                Some(Action::Hide)
            }
            VK_BACK if self.search_focus || !self.search.is_empty() => {
                if ctrl {
                    self.search.clear();
                } else {
                    self.search.pop();
                }
                self.procs.dirty = true;
                Some(Action::Repaint)
            }
            VK_DELETE if self.tab == Tab::Processes => Some(Action::EndSelected),
            VK_UP | VK_DOWN | VK_PRIOR | VK_NEXT | VK_HOME | VK_END if self.tab == Tab::Processes => {
                let d = match vk {
                    VK_UP => -1,
                    VK_DOWN => 1,
                    VK_PRIOR => -20,
                    VK_NEXT => 20,
                    VK_HOME => -100000,
                    _ => 100000,
                };
                self.procs.move_sel(d);
                Some(Action::Repaint)
            }
            VK_LEFT | VK_RIGHT | VK_RETURN if self.tab == Tab::Processes => {
                self.procs.expand_sel(match vk {
                    VK_LEFT => Some(false),
                    VK_RIGHT => Some(true),
                    _ => None,
                });
                Some(Action::Repaint)
            }
            _ => None,
        }
    }

    /// Alt+1..4 (`d` = 0..3 of `services::QUICK_ORDER`): the quick startup type of the selected
    /// service. Only on the Services tab, and never while the search box, a context menu or a
    /// panel has the keyboard. None: not taken (the system keeps the key). `repeat`: the key is held
    /// down; it is swallowed without a second change.
    pub fn alt_digit(&mut self, d: usize, repeat: bool) -> Option<Action> {
        if self.tab != Tab::Services || self.search_focus || self.modal_open() {
            return None;
        }
        if repeat {
            return self.services.selected.is_some().then_some(Action::None);
        }
        services::quick_key(self, d)
    }

    /// A panel, a context menu or a two-step press owns the keyboard: list keys do nothing.
    fn modal_open(&self) -> bool {
        self.menu_open
            || self.peek.is_some()
            || self.cpu.is_some()
            || self.disallow.is_some()
            || self.svcprops.is_some()
            || self.setup.panel.is_some()
            || startup::modal(self)
    }

    /// Arrows, Page Up / Down, Home / End and Enter for the lists (ui/nav.rs). None: not
    /// taken here (Processes keeps its own tree keys in `key`). Never ends, stops, disables,
    /// deletes or runs anything: Enter opens a details view at most.
    fn nav_key(&mut self, vk: u16) -> Option<Action> {
        const VK_RETURN: u16 = 0x0D;
        const VK_LEFT: u16 = 0x25;
        const VK_RIGHT: u16 = 0x27;
        let mv = nav::Move::from_vk(vk);
        if mv.is_none() && ![VK_LEFT, VK_RIGHT, VK_RETURN].contains(&vk) {
            return None;
        }
        // the search box keeps its caret keys; Enter leaves it with the filter kept
        if self.search_focus {
            if vk == VK_RETURN {
                self.search_focus = false;
                return Some(Action::Repaint);
            }
            return Some(Action::None);
        }
        if self.modal_open() {
            return Some(Action::None);
        }
        if self.tab == Tab::Processes {
            return None;
        }
        if vk == VK_LEFT || vk == VK_RIGHT {
            if nav::arrows_switch_tabs(self.tab) {
                let cur = TABS.iter().position(|t| t.0 == self.tab).unwrap_or(0);
                let next = nav::side_tab(cur, TABS.len(), vk == VK_RIGHT);
                if next != cur {
                    self.switch_tab(TABS[next].0);
                }
            }
            return Some(Action::Repaint);
        }
        let Some(m) = mv else {
            // Enter: the selected row's details, nothing that changes the system
            match self.tab {
                Tab::Services => services::open_selected(self),
                Tab::Watch => watch::open_selected(self),
                _ => return Some(Action::None),
            }
            return Some(Action::Repaint);
        };
        match self.tab {
            Tab::Processes => return None,
            Tab::Services => services::nav(self, m),
            Tab::Rules => rules::nav(self, m),
            Tab::Watch => watch::nav(self, m),
            Tab::Startup => startup::nav(self, m),
            Tab::Vitals => vitals::nav(self, m),
            Tab::History => history::key_scroll(self, m),
        }
        Some(Action::Repaint)
    }

    /// Esc after panels and search: drop the current tab's selection. True when there was one.
    fn clear_selection(&mut self) -> bool {
        match self.tab {
            Tab::Processes => self.procs.sel.take().is_some(),
            Tab::Services => self.services.selected.take().is_some(),
            Tab::Rules => self.rules.sel.take().is_some(),
            Tab::Watch => self.watch.sel.take().is_some(),
            Tab::Startup => startup::esc(self),
            Tab::Vitals | Tab::History => false,
        }
    }

    /// WM_CHAR: digits switch tabs unless typing a filter; anything else filters.
    pub fn char(&mut self, c: char) -> Action {
        if c.is_control() {
            return Action::None;
        }
        if startup::wants_chars(self) {
            return startup::char(self, c);
        }
        if !self.search_focus && self.search.is_empty() {
            if let Some(d) = c.to_digit(10) {
                if (1..=TABS.len() as u32).contains(&d) {
                    self.switch_tab(TABS[d as usize - 1].0);
                    return Action::Repaint;
                }
            }
            if c == ' ' {
                return Action::None;
            }
        }
        self.search.push(c);
        self.search_focus = true;
        self.procs.dirty = true;
        // Watch filters its own list; everything else filters processes
        if self.tab != Tab::Processes && self.tab != Tab::Watch && self.tab != Tab::Services {
            self.tab = Tab::Processes;
        }
        Action::Repaint
    }

    /// Tab switch; the Startup tab re-reads its list each time it is shown.
    pub fn switch_tab(&mut self, t: Tab) {
        self.cpu = None;
        self.disallow = None;
        svcprops::close(self);
        self.peek = None;
        if t == Tab::Startup && self.tab != Tab::Startup {
            self.startup.load();
        }
        if t == Tab::Services && self.tab != Tab::Services {
            self.services.reload_config();
        }
        self.tab = t;
    }

    pub fn cycle_plan(&mut self) {
        if self.plans.is_empty() {
            return;
        }
        let i = self.plan_active.and_then(|a| self.plans.iter().position(|p| p.guid == a)).map(|i| (i + 1) % self.plans.len()).unwrap_or(0);
        self.set_plan(i);
    }
    pub fn set_plan(&mut self, i: usize) {
        if let Some(p) = self.plans.get(i) {
            if power::set(&p.guid) {
                self.plan_active = Some(p.guid);
            }
        }
    }
}

/// Vitals, or (debug builds only) the 1-based tab in `HAYATE_TAB`, so an
/// elevated test run, which cannot be clicked (UIPI), can open on any tab.
fn start_tab() -> Tab {
    #[cfg(debug_assertions)]
    if let Some(i) = crate::osutil::env_var("TAB").ok().and_then(|v| v.parse::<usize>().ok()).filter(|i| (1..=TABS.len()).contains(i)) {
        return TABS[i - 1].0;
    }
    Tab::Vitals
}

pub fn group_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}


#[cfg(test)]
mod navigation_tests {
    use super::*;
    #[test]
    fn relaunch_ids_survive_sensor_fold() {
        for (tab, _, _) in TABS { assert_eq!(Tab::from_restore_id(tab.restore_id()), Some(tab)); }
        assert_eq!(Tab::from_restore_id(2), Some(Tab::Vitals));
        assert_eq!(Tab::from_restore_id(8), None);
    }

    const LEFT: u16 = 0x25;
    const RIGHT: u16 = 0x27;
    const DOWN: u16 = 0x28;
    const RET: u16 = 0x0D;
    const ESC: u16 = 0x1B;

    #[test]
    fn left_right_switch_tabs_where_unused_and_clamp() {
        let mut app = App::new();
        app.tab = Tab::Vitals;
        app.key(LEFT, false, false);
        assert_eq!(app.tab, Tab::Vitals, "clamped at the first tab");
        app.key(RIGHT, false, false);
        assert_eq!(app.tab, Tab::Processes);
        // Processes keeps Left / Right for its tree
        app.key(RIGHT, false, false);
        assert_eq!(app.tab, Tab::Processes);
        app.tab = Tab::Watch;
        app.key(RIGHT, false, false);
        assert_eq!(app.tab, Tab::Watch, "clamped at the last tab");
        app.key(LEFT, false, false);
        assert_eq!(app.tab, Tab::Rules);
        // Ctrl+arrows are not tab keys
        app.key(LEFT, true, false);
        assert_eq!(app.tab, Tab::Rules);
    }

    #[test]
    fn focused_search_and_open_menus_keep_the_keys() {
        let mut app = App::new();
        app.tab = Tab::Services;
        app.search_focus = true;
        app.key(RIGHT, false, false);
        assert_eq!(app.tab, Tab::Services, "caret key in the search box");
        app.key(DOWN, false, false);
        assert!(app.services.selected.is_none());
        // Enter leaves the box, the filter stays
        app.search = "x".into();
        app.key(RET, false, false);
        assert!(!app.search_focus);
        assert_eq!(app.search, "x");
        app.set_menu(true);
        app.key(LEFT, false, false);
        assert_eq!(app.tab, Tab::Services, "a context menu owns the keys");
        app.set_menu(false);
        app.key(LEFT, false, false);
        assert_eq!(app.tab, Tab::Startup);
    }

    #[test]
    fn esc_clears_selection_before_hiding() {
        let mut app = App::new();
        app.tab = Tab::Rules;
        app.rules.sel = Some(7);
        assert!(matches!(app.key(ESC, false, false), Some(Action::Repaint)));
        assert!(app.rules.sel.is_none());
        assert!(matches!(app.key(ESC, false, false), Some(Action::Hide)));
    }

    #[test]
    fn enter_never_returns_a_destructive_action() {
        for (tab, _, _) in TABS {
            if tab == Tab::Processes {
                continue; // Enter toggles the tree there (unchanged)
            }
            let mut app = App::new();
            app.tab = tab;
            let a = app.key(RET, false, false);
            assert!(matches!(a, Some(Action::None) | Some(Action::Repaint)), "{tab:?}");
        }
    }
}
