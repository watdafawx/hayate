//! "Who is failing" inside NETWORK HEALTH, and the two-lane marks strip under the throughput graph.
//! Port of docs/mock-net-offenders.html (part 1). The data is `netetw::Feed::conn` (see `netconn.rs`
//! for what each number means and why every kind is an inference); nothing here talks to Windows.

use super::nethealth::{band, tag_chip, BAND_H};
use super::*;
use crate::netconn::{ConnPair, Counts, Kind, NOTICEABLE, WIN};
use crate::netetw::{classify, Scope, Status};
use std::net::IpAddr;

const ROW_H: f32 = 24.0;
const HEAD_H: f32 = 22.0;
/// Rows shown in the table.
pub(super) const MAX_ROWS: usize = 6;
/// Height of the marks strip under the throughput graph (two lanes).
pub(super) const MARKS_H: f32 = 34.0;
const NOTICE_H: f32 = 98.0;
const COLLAPSED_H: f32 = 44.0;
/// First hit id of the table rows (`Hit::VitSeg(ROW_HIT + i)`).
pub(super) const ROW_HIT: usize = 100;
/// Hit id of the "Show apps" / "Hide apps" button.
pub(super) const SHOW_HIT: usize = 13;
const INFERRED: &str = "inferred from timing, not an error code";

/// One app + remote, merged over the processes of the app.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Row {
    /// lower-cased app name: the selection key
    pub key: String,
    pub name: String,
    /// failing processes merged into the row
    pub count: usize,
    /// process whose icon stands for the app
    pub pi: usize,
    pub ip: IpAddr,
    pub port: u16,
    pub scope: Scope,
    pub c60: Counts,
    pub c600: Counts,
    pub kind: Option<Kind>,
}

impl Row {
    /// Failed connects + dropped connections in the last minute.
    pub fn per_min(&self) -> u32 {
        self.c60.failed() + self.c60.mid
    }
    pub fn noticeable(&self) -> bool {
        self.per_min() >= NOTICEABLE
    }
    pub fn remote(&self) -> String {
        match self.ip {
            IpAddr::V6(a) => format!("[{a}]:{}", self.port),
            IpAddr::V4(a) => format!("{a}:{}", self.port),
        }
    }
    fn tag(&self) -> &'static str {
        if self.ip.is_loopback() {
            return "LOCAL";
        }
        match self.scope {
            Scope::Lan => "LAN",
            Scope::Wan => "WAN",
            Scope::Other => "OTHER",
        }
    }
}

pub(super) enum State {
    /// not elevated: no per-process network events
    Admin,
    /// collecting, or the trace is unavailable (the text says why)
    Wait(String),
    Live,
}

/// Per second (failed, retransmits) of every app, and of the selected one.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Marks {
    pub all: [[u16; 2]; WIN],
    pub sel: Option<[[u16; 2]; WIN]>,
    pub sel_name: String,
}

impl Default for Marks {
    fn default() -> Self {
        Marks { all: [[0; 2]; WIN], sel: None, sel_name: String::new() }
    }
}

pub(super) struct View {
    pub state: State,
    /// every offender, worst first
    pub rows: Vec<Row>,
    pub marks: Marks,
    /// "Show apps" pressed
    pub show: bool,
    pub sel: Option<String>,
}

impl View {
    /// Nothing noticeable and not expanded: one collapsed row.
    fn collapsed(&self) -> bool {
        !self.show && !self.rows.iter().any(|r| r.noticeable())
    }
    /// The offender the verdict may name: the worst one, when noticeable and its kind is known.
    pub fn culprit(&self) -> Option<&Row> {
        self.rows.first().filter(|r| r.noticeable() && r.kind.is_some())
    }
}

fn scope_of(ip: &IpAddr) -> Scope {
    match ip {
        IpAddr::V4(a) => classify(&a.octets()),
        IpAddr::V6(a) => classify(&a.octets()),
    }
}

/// Merge pairs into app rows. `who(pid, create)` names the app and its icon process (None: the
/// process is gone). Pure.
pub(super) fn build_rows(pairs: &[ConnPair], who: &dyn Fn(u32, i64) -> Option<(String, usize)>) -> Vec<Row> {
    let mut map: HashMap<(String, IpAddr, u16), Row> = HashMap::new();
    for p in pairs {
        let Some((name, pi)) = who(p.pid, p.create) else { continue };
        let key = name.to_lowercase();
        let e = map.entry((key.clone(), p.remote, p.port)).or_insert_with(|| Row { key, name, count: 0, pi, ip: p.remote, port: p.port, scope: scope_of(&p.remote), c60: Counts::default(), c600: Counts::default(), kind: None });
        e.count += 1;
        e.c60.add(&p.c60);
        e.c600.add(&p.c600);
    }
    let mut rows: Vec<Row> = map.into_values().collect();
    for r in &mut rows {
        r.kind = r.c60.dominant().or(r.c600.dominant());
    }
    rows.sort_by(|a, b| b.c60.score().cmp(&a.c60.score()).then(b.c600.score().cmp(&a.c600.score())).then_with(|| a.name.cmp(&b.name)).then_with(|| a.port.cmp(&b.port)));
    rows
}

/// Per-second marks of all pairs, and of the pairs of app `sel`. Pure.
pub(super) fn build_marks(pairs: &[ConnPair], more: &[[u8; 2]; WIN], who: &dyn Fn(u32, i64) -> Option<(String, usize)>, sel: Option<&str>) -> Marks {
    let mut m = Marks::default();
    let mut s = [[0u16; 2]; WIN];
    for p in pairs {
        let mine = sel.is_some_and(|k| who(p.pid, p.create).is_some_and(|(n, _)| n.to_lowercase() == k));
        for i in 0..WIN {
            for lane in 0..2 {
                m.all[i][lane] += p.tl[i][lane] as u16;
                if mine {
                    s[i][lane] += p.tl[i][lane] as u16;
                }
            }
        }
    }
    for i in 0..WIN {
        for lane in 0..2 {
            m.all[i][lane] += more[i][lane] as u16;
        }
    }
    if let Some(k) = sel {
        m.sel = Some(s);
        m.sel_name = k.to_string();
    }
    m
}

/// What the table and the strip show for this snapshot. Resolves processes to apps with the
/// same groups TOP BY NETWORK uses.
pub(super) fn view(app: &mut App, s: &Snapshot) -> View {
    let feed = &s.vit.net_top;
    let state = match &feed.status {
        Status::NeedsAdmin => State::Admin,
        Status::Live => State::Live,
        Status::Unavailable(w) => State::Wait(format!("{w} · Hayate retries automatically.")),
        _ => State::Wait("Collecting the first interval…".into()),
    };
    let mut v = View { state, rows: Vec::new(), marks: Marks::default(), show: app.vit.off_show, sel: app.vit.off_sel.clone() };
    if !matches!(v.state, State::Live) {
        return v;
    }
    let conn = &feed.conn;
    if app.vit.groups_seq != s.seq {
        app.vit.groups = procs::build_groups(s);
        app.vit.groups_seq = s.seq;
    }
    let mut group_of = vec![usize::MAX; s.procs.len()];
    for (k, gr) in app.vit.groups.iter().enumerate() {
        for &m in &gr.members {
            if let Some(slot) = group_of.get_mut(m) {
                *slot = k;
            }
        }
    }
    let idx: HashMap<(u32, i64), usize> = if conn.pairs.is_empty() { HashMap::new() } else { s.procs.iter().enumerate().map(|(i, p)| ((p.pid, p.create), i)).collect() };
    let groups = &app.vit.groups;
    let who = |pid: u32, create: i64| -> Option<(String, usize)> {
        let i = *idx.get(&(pid, create))?;
        match groups.get(group_of[i]) {
            Some(gr) => Some((gr.name.clone(), group_icon(s, gr))),
            None => Some((s.procs[i].name.to_string(), i)),
        }
    };
    v.rows = build_rows(&conn.pairs, &who);
    // a selection that no longer fails anywhere is dropped
    if v.sel.as_ref().is_some_and(|k| !v.rows.iter().any(|r| &r.key == k)) {
        v.sel = None;
    }
    v.marks = build_marks(&conn.pairs, &conn.more_tl, &who, v.sel.as_deref());
    if let Some(k) = v.sel.as_deref() {
        v.marks.sel_name = display_name(&v.rows, k);
    }
    let sel = v.sel.clone();
    app.vit.off_sel = sel;
    v
}

/// The app name as shown in the table for selection key `key` (the lower-cased name): "PowerShell 7",
/// not "powershell 7". Falls back to the key when no row has it.
pub(super) fn display_name(rows: &[Row], key: &str) -> String {
    rows.iter().find(|r| r.key == key).map_or_else(|| key.to_string(), |r| r.name.clone())
}

/// What the incident tracker sees of the current verdict: bad or not, and the named offender (the
/// one the verdict names), with the app's exe path for its icon in the Watch feed.
pub(super) fn incident_obs(bad: bool, label: &str, culprit: Option<&Row>, path: Option<&str>) -> crate::netincident::Obs {
    let mut info = crate::netincident::Info { label: label.to_string(), ..Default::default() };
    if let Some(r) = culprit {
        info.app = r.name.clone();
        info.remote = r.remote();
        info.per_min = r.per_min();
        info.midstream = r.kind == Some(Kind::Midstream);
        info.kind = r.kind.map(|k| k.label().to_lowercase()).unwrap_or_default();
        info.path = path.unwrap_or_default().to_string();
    }
    crate::netincident::Obs { bad, info }
}

/// Width of the "Test host" button at the right end of a row.
const TEST_W: f32 = 78.0;
/// First hit id of the "Test host" buttons (`Hit::VitSeg(TEST_HIT + i)`).
pub(super) const TEST_HIT: usize = 200;

/// Height of the whole "Who is failing" section (band included).
pub(super) fn height(v: &View) -> f32 {
    BAND_H
        + match v.state {
            State::Admin => NOTICE_H,
            State::Wait(_) => COLLAPSED_H,
            State::Live if v.collapsed() => COLLAPSED_H,
            State::Live => HEAD_H + v.rows.len().min(MAX_ROWS) as f32 * ROW_H + if v.rows.len() > MAX_ROWS { 20.0 } else { 0.0 } + 8.0,
        }
}

/// Column x ranges of the table inside `r`: (app right edge, remote, failed, retrans, resets, kind).
struct Cols {
    app_r: f32,
    remote: (f32, f32),
    failed: (f32, f32),
    retr: (f32, f32),
    resets: (f32, f32),
    kind: (f32, f32),
}

fn cols(r: Rect) -> Cols {
    let right = r.r() - 14.0 - TEST_W - 10.0;
    let kind = (right - 124.0, 124.0);
    let resets = (kind.0 - 8.0 - 72.0, 72.0);
    let retr = (resets.0 - 8.0 - 80.0, 80.0);
    let failed = (retr.0 - 8.0 - 72.0, 72.0);
    let remote = (failed.0 - 14.0 - 196.0, 196.0);
    Cols { app_r: remote.0 - 12.0, remote, failed, retr, resets, kind }
}

fn num_col(g: &mut Gfx, c: (f32, f32), y: f32, v: u32, hot: bool) {
    let (t, col, font) = if v == 0 { ("—".to_string(), TX4, f(11.5).m().r()) } else { (group_num(v as u64), if hot { WARN_TX } else { TX2 }, if hot { f(11.5).m().b().r() } else { f(11.5).m().r() }) };
    g.text(&t, Rect::new(c.0, y, c.1, ROW_H), font, col);
}

/// The table section at `y` in panel `r`.
pub(super) fn paint(app: &mut App, g: &mut Gfx, s: &Snapshot, r: Rect, y: f32, v: &View) -> Option<Tip> {
    band(app, g, r, y, "Who is failing", "apps whose connections fail or lose packets · last 60 s", "ETW", false);
    let y0 = y + BAND_H;
    let x = r.x + 14.0;
    app.vit.off_keys.clear();
    app.vit.off_targets.clear();
    let mut tip = None;
    match &v.state {
        State::Admin => {
            // same style and wording as the all-adapter graph notice
            g.text("Which app is failing needs administrator rights", Rect::new(x, y0 + 12.0, r.w - 28.0, 20.0), f(12.0), TX2);
            g.text("Windows only lets an elevated program read network events per process. The system-wide counters work without it.", Rect::new(x, y0 + 34.0, r.w - 28.0, 18.0), f(11.5), TX3);
            let br = Rect::new(x, y0 + 60.0, 150.0, 28.0);
            let hov = app.hover == Some(Hit::VitAdmin);
            button(g, br, "Restart as admin", hov, true);
            app.hits.push((br, Hit::VitAdmin));
        }
        State::Wait(t) => g.text(t, Rect::new(x, y0 + 12.0, r.w - 28.0, 20.0), f(12.0), TX3),
        State::Live if v.collapsed() => {
            g.text("No app is failing noticeably", Rect::new(x, y0 + 6.0, r.w - 200.0, 20.0), f(12.0), TX2);
            let sub = if v.rows.is_empty() { "No failed connections or lost packets in the last 10 minutes.".to_string() } else { format!("Every app is under {NOTICEABLE} failed or reset connections per minute.") };
            g.text(&sub, Rect::new(x, y0 + 24.0, r.w - 200.0, 16.0), f(11.0), TX3);
            if !v.rows.is_empty() {
                toggle_button(app, g, r, y0, "Show apps");
            }
        }
        State::Live => tip = table(app, g, s, r, y0, v),
    }
    tip
}

fn toggle_button(app: &mut App, g: &mut Gfx, r: Rect, y0: f32, label: &str) {
    let bw = app.vit.wd.get(g, label, f(11.5)) + 26.0;
    let br = Rect::new(r.r() - 14.0 - bw, y0 + 8.0, bw, 24.0);
    let hov = app.hover == Some(Hit::VitSeg(SHOW_HIT));
    g.round(br, 12.0, if hov { PAN3 } else { PAN2 });
    g.round_stroke(br, 12.0, LN, 1.0);
    g.text(label, br, f(11.5).c(), TX1);
    app.hits.push((br, Hit::VitSeg(SHOW_HIT)));
}

fn table(app: &mut App, g: &mut Gfx, s: &Snapshot, r: Rect, y0: f32, v: &View) -> Option<Tip> {
    let c = cols(r);
    let x = r.x + 14.0;
    let hy = y0 + 2.0;
    for (t, rect, right) in [
        ("APP", (x, 120.0), false),
        ("REMOTE", c.remote, false),
        ("FAILED/MIN", c.failed, true),
        ("RETRANS/MIN", c.retr, true),
        ("RESETS/MIN", c.resets, true),
        ("KIND", c.kind, false),
    ] {
        let font = if right { f(10.0).r() } else { f(10.0) };
        g.text(t, Rect::new(rect.0, hy, rect.1, HEAD_H), font, TX4);
    }
    if v.show && !v.rows.iter().any(|r| r.noticeable()) {
        toggle_button(app, g, r, y0 - 6.0, "Hide apps");
    }
    let mut tip: Option<Tip> = None;
    let top = y0 + HEAD_H;
    for (k, row) in v.rows.iter().take(MAX_ROWS).enumerate() {
        let ry = top + k as f32 * ROW_H;
        let rr = Rect::new(r.x + 1.0, ry, r.w - 2.0, ROW_H);
        let selected = v.sel.as_deref() == Some(row.key.as_str());
        let hit = Hit::VitSeg(ROW_HIT + k);
        let hovered = app.hover == Some(hit) && !app.menu_open;
        if selected {
            g.fill(rr, ACC_BG);
            g.fill(Rect::new(rr.x, rr.y + 3.0, 2.0, rr.h - 6.0), ACC);
        } else {
            if k % 2 == 1 {
                g.fill(rr, ZEBRA);
            }
            if row.noticeable() {
                g.fill(Rect::new(rr.x, rr.y + 3.0, 2.0, rr.h - 6.0), if row.per_min() >= 30 { HOT } else { WARN });
            }
            if hovered {
                g.fill_a(rr, 0xffffff, 0.035);
            }
        }
        app.hits.push((rr, hit));
        app.vit.off_keys.push(row.key.clone());
        app.vit.off_targets.push((row.ip, row.port));
        let thit = Hit::VitSeg(TEST_HIT + k);
        let thov = app.hover == Some(thit) && !app.menu_open;
        if hovered || thov {
            tip = Some(row_tip(row));
        }
        // app: icon, name, ×N
        let ir = Rect::new(x, ry + 4.0, 16.0, 16.0);
        if let Some(p) = s.procs.get(row.pi) {
            let svc = if p.name.eq_ignore_ascii_case("svchost.exe") { "svchost.exe" } else { "" };
            if !super::super::icons::draw(&mut app.icons, g, p.meta.path.as_deref().unwrap_or(svc), p.pid, ir) {
                g.round(ir.inset(1.0, 1.0), 3.0, procs::name_color(&p.name));
                let ch: String = row.name.chars().take(1).collect::<String>().to_uppercase();
                g.text(&ch, ir, f(9.5).b().c(), BG);
            }
        }
        let nw = if row.count > 1 { 30.0 } else { 0.0 };
        let name_w = (c.app_r - x - 24.0 - nw).max(0.0);
        g.text(&row.name, Rect::new(x + 24.0, ry, name_w, ROW_H), f(12.0), TX1);
        if row.count > 1 {
            g.text(&format!("×{}", row.count), Rect::new(c.app_r - nw, ry, nw, ROW_H), f(10.5).m().r(), TX3);
        }
        // remote ip:port + scope tag
        let rt = row.remote();
        let tg = row.tag();
        let tgw = app.vit.wd.get(g, tg, f(9.5).b()) + 10.0;
        let rw = (c.remote.1 - tgw - 6.0).max(0.0);
        let tw = app.vit.wd.get(g, &rt, f(11.5).m()).min(rw);
        g.text(&rt, Rect::new(c.remote.0, ry, rw, ROW_H), f(11.5).m(), TX1);
        tag_chip(g, Rect::new(c.remote.0 + tw + 6.0, ry + 5.0, tgw, 14.0), tg, false);
        num_col(g, c.failed, ry, row.c60.failed(), row.c60.failed() >= NOTICEABLE);
        num_col(g, c.retr, ry, row.c60.retr, false);
        num_col(g, c.resets, ry, row.c60.mid, row.c60.mid >= NOTICEABLE);
        match row.kind {
            Some(kd) => g.text(kd.label(), Rect::new(c.kind.0, ry, c.kind.1, ROW_H), f(11.5), TX2),
            None => g.text("—", Rect::new(c.kind.0, ry, c.kind.1, ROW_H), f(11.5), TX4),
        }
        // "Test host": click only, never automatic. Pushed after the row hit so it wins the hit test.
        let br = Rect::new(r.r() - 14.0 - TEST_W, ry + 3.0, TEST_W, ROW_H - 6.0);
        let (label, on) = match crate::nettest::status(row.ip, row.port) {
            Some(crate::nettest::Slot::Running { .. }) => ("Testing…", false),
            Some(crate::nettest::Slot::Done { outcome, .. }) => (outcome.short(), true),
            None => ("Test host", false),
        };
        g.round(br, 11.0, if thov { PAN3 } else { PAN2 });
        g.round_stroke(br, 11.0, LN, 1.0);
        g.text(label, br, f(10.5).c(), if on { TX1 } else { TX2 });
        app.hits.push((br, thit));
    }
    if v.rows.len() > MAX_ROWS {
        g.text(&format!("+{} more", v.rows.len() - MAX_ROWS), Rect::new(x, top + MAX_ROWS as f32 * ROW_H + 2.0, 200.0, 16.0), f(10.5), TX3);
    }
    tip
}

/// The hover card of a row. Painted by the caller AFTER everything below the table, so the
/// probe band never covers it.
pub(super) fn paint_late(app: &mut App, g: &mut Gfx, t: Tip) {
    let win = (app.w, app.h);
    let m = (app.mx, app.my);
    let tp = app.vit.clip.y.max(TITLE_H + 4.0);
    let bottom = app.h - STATUS_H - 4.0;
    paint_tip(app, g, &t, win.0 - 16.0, |w, h| tip_pos(m, (w, h), win, tp, bottom));
}

/// Hover card of one row: last 60 s and 10 min.
fn row_tip(r: &Row) -> Tip {
    let two = |a: u32, b: u32| format!("{} / {}", group_num(a as u64), group_num(b as u64));
    let line = |label: &str, value: String| TipLine { swatch: None, label: label.into(), value, hot: false };
    let mut lines = vec![
        line("last 60 s / 10 min", String::new()),
        line("Connect attempts", two(r.c60.att, r.c600.att)),
        line("Failed connects", two(r.c60.failed(), r.c600.failed())),
        line("Retransmits", two(r.c60.retr, r.c600.retr)),
        line("Resets", two(r.c60.mid, r.c600.mid)),
        line("Kind", r.kind.map_or("—".to_string(), |k| k.label().to_string())),
    ];
    match crate::nettest::status(r.ip, r.port) {
        Some(crate::nettest::Slot::Running { .. }) => lines.push(line("Host test", "running…".into())),
        Some(crate::nettest::Slot::Done { outcome, .. }) => lines.push(line("Host test", outcome.text().into())),
        None => {}
    }
    Tip { head: format!("{} → {}", r.name, r.remote()), lines, foot: Some(INFERRED.into()) }
}

/// Tick height by the count that second: 3 px for one, +2 per extra, at most 11.
pub(super) fn tick_h(count: u16) -> f32 {
    (3.0 + count.saturating_sub(1) as f32 * 2.0).min(11.0)
}

/// Two lanes under the throughput graph on its time axis: red = failed connects, amber =
/// retransmits. With an app selected its ticks stay bright and the rest drop to 28%.
pub(super) fn strip(app: &mut App, g: &mut Gfx, p: &Plot, y: f32, m: &Marks) {
    let cw = p.w / HIST as f32;
    let base = [y + 14.0, y + 30.0];
    for (lane, (lab, col)) in [("FAILED", HOT), ("RETRANS", WARN)].into_iter().enumerate() {
        g.fill(Rect::new(p.x, base[lane], p.w, 1.0), SEP);
        g.text(lab, Rect::new(p.lab_r - 70.0, base[lane] - 12.0, 70.0, 12.0), f(9.5).m().r(), TX4);
        let tw = (cw * 0.55).clamp(2.0, 4.0);
        let tick = |i: usize, n: u16| Rect::new((p.x + (i as f32 + 0.5) * cw - tw / 2.0).round(), base[lane] - tick_h(n), tw, tick_h(n));
        for i in 0..WIN {
            let n = m.all[i][lane];
            if n > 0 {
                g.fill_a(tick(i, n), col, if m.sel.is_some() { 0.28 } else { 1.0 });
            }
        }
        if let Some(sel) = &m.sel {
            for i in 0..WIN {
                let n = sel[i][lane];
                if n > 0 {
                    g.fill(tick(i, n), col);
                }
            }
        }
    }
    // synced readout at the hovered second
    if let (Some(c), 0) = (app.vit.hover_col, app.vit.hover_slot) {
        let c = c.min(WIN - 1);
        let (cnt, who) = match &m.sel {
            Some(sel) => (sel[c], format!(" · {}", m.sel_name)),
            None => (m.all[c], String::new()),
        };
        let t = format!("{} failed, {} retransmits{who}", cnt[0], cnt[1]);
        let tw = app.vit.wd.get(g, &t, f(11.0).m()) + 14.0;
        let x = (p.x + (c as f32 + 0.5) * cw).round();
        let bx = if x + 8.0 + tw > p.x + p.w { x - 8.0 - tw } else { x + 8.0 };
        let b = Rect::new(bx.max(p.x), y + 8.0, tw, 18.0);
        g.round(b, 4.0, PAN3);
        g.round_stroke(b, 4.0, LN, 1.0);
        g.text(&t, b, f(11.0).m().c(), TX1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn pair(pid: u32, ip: [u8; 4], port: u16, failed: u32, retr: u32, mid: u32) -> ConnPair {
        let c = Counts { att: failed + 5, noreply: failed, refused: 0, mid, retr };
        let mut tl = [[0u8; 2]; WIN];
        tl[WIN - 1] = [failed.min(255) as u8, retr.min(255) as u8];
        ConnPair { pid, create: 1, remote: IpAddr::V4(Ipv4Addr::from(ip)), port, c60: c, c600: c, kind: c.dominant(), tl }
    }

    fn who(pid: u32, _c: i64) -> Option<(String, usize)> {
        match pid {
            1 | 2 => Some(("OpenJDK Platform binary".into(), 7)),
            3 => Some(("chrome".into(), 9)),
            _ => None,
        }
    }

    #[test]
    fn the_readout_names_the_app_as_shown_not_the_lowercase_key() {
        let mut ps = vec![pair(1, [192, 168, 50, 2], 25565, 40, 0, 0)];
        let who2 = |pid: u32, _c: i64| (pid == 1).then(|| ("PowerShell 7".to_string(), 7usize));
        let rows = build_rows(&ps, &who2);
        assert_eq!(rows[0].key, "powershell 7");
        assert_eq!(display_name(&rows, "powershell 7"), "PowerShell 7");
        assert_eq!(display_name(&rows, "gone app"), "gone app", "no row: the key, never a panic");
        assert_eq!(display_name(&[], "x"), "x");
        ps.clear();
    }

    #[test]
    fn the_incident_observation_names_the_culprit_with_a_lowercase_kind() {
        let pairs = vec![pair(1, [192, 168, 50, 2], 25565, 61, 0, 0)];
        let rows = build_rows(&pairs, &who);
        let o = incident_obs(true, "Degraded: connections failing", rows.first(), Some("C:\\x\\java.exe"));
        assert!(o.bad);
        assert_eq!((o.info.app.as_str(), o.info.remote.as_str(), o.info.per_min), ("OpenJDK Platform binary", "10.0.0.2:25565", 61));
        assert_eq!((o.info.kind.as_str(), o.info.midstream, o.info.path.as_str()), ("no reply", false, "C:\\x\\java.exe"));
        let none = incident_obs(true, "Degraded: connections failing", None, None);
        assert!(!none.info.has_offender());
        assert_eq!(none.info.label, "Degraded: connections failing");
    }

    #[test]
    fn rows_merge_by_app_and_remote_and_rank_by_score() {
        let pairs = vec![pair(1, [192, 168, 50, 2], 25565, 40, 0, 0), pair(2, [192, 168, 50, 2], 25565, 21, 8, 0), pair(3, [8, 8, 8, 8], 443, 2, 40, 0), pair(4, [8, 8, 8, 8], 443, 99, 0, 0)];
        let rows = build_rows(&pairs, &who);
        assert_eq!(rows.len(), 2, "pid 4 is gone and skipped, the two java processes merge");
        let j = &rows[0];
        assert_eq!((j.name.as_str(), j.count, j.c60.failed(), j.c60.retr), ("OpenJDK Platform binary", 2, 61, 8));
        assert_eq!((j.scope, j.kind, j.per_min()), (Scope::Lan, Some(Kind::NoReply), 61));
        assert!(j.noticeable());
        assert_eq!(j.remote(), "10.0.0.2:25565");
        assert_eq!(rows[1].scope, Scope::Wan);
        // retransmits only: score retr/4 = 10 vs failed 2: chrome is under the noticeable bar
        assert!(!rows[1].noticeable());
    }

    #[test]
    fn ipv6_remote_is_bracketed_and_loopback_is_local() {
        let mut p = pair(1, [127, 0, 0, 1], 1, 12, 0, 0);
        let rows = build_rows(std::slice::from_ref(&p), &who);
        assert_eq!(rows[0].tag(), "LOCAL");
        p.remote = "2001:db8::5".parse().unwrap();
        let rows = build_rows(std::slice::from_ref(&p), &who);
        assert_eq!(rows[0].remote(), "[2001:db8::5]:1");
    }

    #[test]
    fn culprit_needs_a_noticeable_row() {
        let mk = |rows| View { state: State::Live, rows, marks: Marks::default(), show: false, sel: None };
        let quiet = mk(build_rows(&[pair(3, [8, 8, 8, 8], 443, 9, 0, 0)], &who));
        assert!(quiet.culprit().is_none());
        assert!(quiet.collapsed());
        let loud = mk(build_rows(&[pair(3, [8, 8, 8, 8], 443, 10, 0, 0)], &who));
        assert_eq!(loud.culprit().map(|r| r.name.as_str()), Some("chrome"));
        assert!(!loud.collapsed());
        // heights: collapsed < table < table with the "+N more" line
        assert!(height(&quiet) < height(&loud));
        let many: Vec<ConnPair> = (0..8).map(|i| pair(3, [10, 0, 0, i], 80, 20, 0, 0)).collect();
        assert!(height(&mk(build_rows(&many, &who))) > height(&loud) + 4.0 * ROW_H);
        // Show apps expands a quiet list
        let mut shown = mk(build_rows(&[pair(3, [8, 8, 8, 8], 443, 9, 0, 0)], &who));
        shown.show = true;
        assert!(!shown.collapsed());
    }

    #[test]
    fn marks_sum_pairs_and_select_one_app() {
        let pairs = vec![pair(1, [10, 0, 0, 2], 80, 3, 1, 0), pair(3, [8, 8, 8, 8], 443, 5, 2, 0)];
        let more = [[0u8; 2]; WIN];
        let m = build_marks(&pairs, &more, &who, None);
        assert_eq!(m.all[WIN - 1], [8, 3]);
        assert!(m.sel.is_none());
        let m = build_marks(&pairs, &more, &who, Some("chrome"));
        assert_eq!(m.sel.unwrap()[WIN - 1], [5, 2]);
        assert_eq!(m.all[WIN - 1], [8, 3], "the unselected total is kept for the dimmed ticks");
    }

    #[test]
    fn tick_height_is_3_to_11_px() {
        assert_eq!((tick_h(1), tick_h(2), tick_h(4), tick_h(5), tick_h(500)), (3.0, 5.0, 9.0, 11.0, 11.0));
    }

    #[test]
    fn row_tip_has_both_windows_and_the_inferred_note() {
        let r = &build_rows(&[pair(1, [10, 0, 0, 2], 80, 12, 4, 1)], &who)[0];
        let t = row_tip(r);
        assert_eq!(t.head, "OpenJDK Platform binary → 10.0.0.2:80");
        assert!(t.lines.iter().any(|l| l.label == "Failed connects" && l.value == "12 / 12"));
        assert!(t.foot.unwrap().contains("not an error code"));
    }
}
