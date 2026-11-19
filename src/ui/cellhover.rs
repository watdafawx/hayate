//! Hover help for the Processes list.
//!
//! - **Name cell** (after a short dwell, or Ctrl+I for the selected row): a detail card for
//!   the row: the workload a runtime process is running, executable identity and PID, the
//!   full image path and command line, the launching parent (only when its identity checks
//!   out) and the account, and the rules that match it with what is CONFIGURED kept apart
//!   from what was last APPLIED. The pointer can move onto the card; its buttons copy the
//!   full command or path through the existing clipboard action.
//! - **Every other clipped cell**: a plain tooltip with the exact full string the cell draws.
//!
//! Nothing here selects, reorders, acts on or even touches a process. The card is keyed by
//! row identity (pid + create time), never by index, and the row freeze of the list holds
//! while the pointer is on the card. Account lookup is lazy and asynchronous
//! (`sample::who`), everything else reads the snapshot the list already holds.

use super::procs::{cell_text, Col, ProcView, RowId, Row};
use super::rulepeek::{configured_text, last_text, member_states, summary_line};
use super::*;
use crate::rules::view::{Mark, RuleView};
use crate::sample::who::{self, User};
use crate::sample::{Proc, Snapshot};
use std::time::{Duration, Instant};

/// How long the pointer rests on a cell before its help appears.
pub const DWELL_MS: u64 = 350;
const CARD_W: f32 = 460.0;
const LABEL_W: f32 = 98.0;
const LINE_H: f32 = 15.0;

// ---------- state ----------

/// A cell by what it shows, not where it is: the row's identity and the column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellKey {
    pub id: RowId,
    pub col: Col,
}

pub struct OpenCard {
    pub id: RowId,
    /// opened from the keyboard: stays until Esc / Ctrl+I / the selection goes
    pub via_key: bool,
    /// last painted rectangle (the pointer on it keeps the card open)
    pub rect: Rect,
    /// list scroll (vertical, horizontal) when opened: moving the list closes a mouse card
    scroll: (f32, f32),
    /// which copy button was used (shown as "Copied")
    pub copied: Option<usize>,
    body_scroll: f32,
    body_max: f32,
}

/// Keep wheel input on the card, including when its body is shorter than the viewport.
pub fn wheel(app: &mut App, delta: f32) -> bool {
    let Some(c) = app.hov.card.as_mut() else { return false };
    if !c.via_key && !c.rect.contains(app.mx, app.my) {
        return false;
    }
    c.body_scroll = (c.body_scroll - delta * 0.5).clamp(0.0, c.body_max);
    true
}

#[derive(Default)]
pub struct HoverState {
    cand: Option<(CellKey, Instant)>,
    /// help the user dismissed (Esc / click): not shown again until the pointer leaves that cell
    dismissed: Option<CellKey>,
    pub card: Option<OpenCard>,
    /// the cell under the pointer at the last paint, to repaint when it changes
    pub last_cell: Option<(usize, usize)>,
}

impl HoverState {
    /// The pointer is on the open card.
    pub fn card_contains(&self, x: f32, y: f32) -> bool {
        self.card.as_ref().is_some_and(|c| c.rect.contains(x, y))
    }

    /// A press anywhere but on the card: close what is open and do not bring it back while
    /// the pointer stays on the same cell. True when something was open.
    pub fn press(&mut self) -> bool {
        if let Some(c) = self.card.take() {
            self.dismissed = Some(CellKey { id: c.id, col: Col::Name });
            return true;
        }
        match self.cand {
            Some((k, _)) => {
                self.dismissed = Some(k);
                false
            }
            None => false,
        }
    }

    /// Esc: close the card; true when there was one.
    pub fn dismiss_card(&mut self) -> bool {
        match self.card.take() {
            Some(c) => {
                self.dismissed = Some(CellKey { id: c.id, col: Col::Name });
                true
            }
            None => false,
        }
    }
}

/// What the next paint should show.
#[derive(Debug, PartialEq)]
pub enum Show {
    Nothing,
    /// keep waiting: repaint after this long
    Wait(Duration),
    Card(RowId),
    Tip(CellKey),
}

/// The dwell rule. `wants`: the cell has help to give (a Name cell always, another cell
/// only when clipped). `blocked`: a panel, menu or drag owns the pointer.
pub fn step(st: &mut HoverState, key: Option<CellKey>, wants: bool, blocked: bool, now: Instant) -> Show {
    let Some(k) = key else {
        st.cand = None;
        st.dismissed = None;
        return Show::Nothing;
    };
    if blocked {
        st.cand = None;
        return Show::Nothing;
    }
    if st.dismissed.is_some() && st.dismissed != Some(k) {
        st.dismissed = None;
    }
    if st.dismissed == Some(k) {
        return Show::Nothing;
    }
    let since = match st.cand {
        Some((ck, t)) if ck == k => t,
        _ => {
            st.cand = Some((k, now));
            now
        }
    };
    if !wants {
        return Show::Nothing;
    }
    let el = now.saturating_duration_since(since);
    let dwell = Duration::from_millis(DWELL_MS);
    if el >= dwell {
        if k.col == Col::Name {
            Show::Card(k.id)
        } else {
            Show::Tip(k)
        }
    } else {
        Show::Wait(dwell - el)
    }
}

/// Does an open card stay? A pointer card needs the pointer on its Name cell or on the card,
/// an unmoved list and an unobstructed screen; a keyboard card only its row.
pub fn keep_card(c: &OpenCard, key: Option<CellKey>, over_card: bool, blocked: bool, row_shown: bool, scroll: (f32, f32)) -> bool {
    if blocked || !row_shown {
        return false;
    }
    if c.via_key {
        return true;
    }
    c.scroll == scroll && (over_card || key == Some(CellKey { id: c.id, col: Col::Name }))
}

// ---------- clipping and wrapping ----------

/// Is the text of a cell cut off: by the column (DirectWrite ellipsises it), by the list's
/// visible area (a column partly scrolled away), or by the list's top / bottom edge.
pub fn is_clipped(text_w: f32, cell: Rect, right: bool, view: Rect) -> bool {
    let pad = cell.inset(8.0, 0.0);
    if text_w > pad.w + 0.5 {
        return true;
    }
    let (t0, t1) = if right { (pad.r() - text_w, pad.r()) } else { (pad.x, pad.x + text_w) };
    t0 < view.x - 0.5 || t1 > view.r() + 0.5 || cell.y < view.y - 0.5 || cell.b() > view.b() + 0.5
}

fn is_break(c: char) -> bool {
    matches!(c, ' ' | '\\' | '/' | ',' | ';' | '=' | '-' | ')' | ':')
}

/// Wrap `s` into lines that `fits`, breaking at spaces and path separators when one is near
/// and anywhere otherwise (a path or command line may have no space at all). Nothing is
/// dropped or altered: the lines joined are `s` (a `\n` starts a new line), except past
/// `max_lines`, where the rest is only counted. Returns the lines and the characters left out.
pub fn wrap_hard(s: &str, max_lines: usize, mut fits: impl FnMut(&str) -> bool) -> (Vec<String>, usize) {
    let chars: Vec<char> = s.chars().collect();
    let total = chars.len();
    let mut lines: Vec<String> = Vec::new();
    let mut pos = 0usize;
    while pos <= total {
        let pe = chars[pos..].iter().position(|&c| c == '\n').map(|i| pos + i).unwrap_or(total);
        if pe == pos {
            if pos < total || lines.is_empty() {
                if lines.len() >= max_lines {
                    return (lines, total - pos);
                }
                lines.push(String::new());
            }
        } else {
            let mut a = pos;
            while a < pe {
                if lines.len() >= max_lines {
                    return (lines, total - a);
                }
                let whole: String = chars[a..pe].iter().collect();
                if fits(&whole) {
                    lines.push(whole);
                    break;
                }
                let (mut lo, mut hi) = (1usize, (pe - a).saturating_sub(1).max(1));
                while lo < hi {
                    let mid = (lo + hi).div_ceil(2);
                    let t: String = chars[a..a + mid].iter().collect();
                    if fits(&t) {
                        lo = mid;
                    } else {
                        hi = mid - 1;
                    }
                }
                let lim = a + lo;
                let cut = (a + lo / 2 + 1..=lim).rev().find(|&j| is_break(chars[j - 1])).unwrap_or(lim);
                lines.push(chars[a..cut].iter().collect());
                a = cut;
            }
        }
        if pe >= total {
            break;
        }
        pos = pe + 1;
    }
    (lines, 0)
}

// ---------- parent identity ----------

/// Who started a process, as far as it can be shown without guessing.
#[derive(Clone, Debug, PartialEq)]
pub enum Launcher {
    /// the process records no parent
    None,
    /// a process with that PID exists, was created before this one and is still running
    Verified { pid: u32, name: String },
    /// not shown as a launcher; why
    Unknown(String),
}

/// A parent PID names a process only while that process lives. The parent must exist in the
/// snapshot, with a create time before the child's: a PID that was reused (a newer process
/// owns it now) or whose process left is NOT the launcher, whatever the child recorded.
pub fn launcher_of(procs: &[Proc], p: &Proc) -> Launcher {
    if p.ppid == 0 || p.pid <= 4 || p.ppid == p.pid {
        return Launcher::None;
    }
    match procs.iter().find(|q| q.pid == p.ppid) {
        None => Launcher::Unknown(format!("PID {} is no longer running, or is not visible to Hayate", p.ppid)),
        Some(q) if q.create == 0 || p.create == 0 => Launcher::Unknown(format!("PID {} has no usable start time to compare", p.ppid)),
        Some(q) if q.create > p.create => Launcher::Unknown(format!("PID {} now belongs to a newer process, so the original launcher has exited", p.ppid)),
        Some(q) => Launcher::Verified { pid: q.pid, name: q.name.to_string() },
    }
}

// ---------- card content ----------

pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub note: Option<String>,
    pub mono: bool,
    /// most lines shown at full size
    pub cap: usize,
}

pub struct CardData {
    pub title: String,
    pub chip: Option<String>,
    pub sub: Option<String>,
    pub fields: Vec<Field>,
    pub copy_cmd: Option<(&'static str, String)>,
    pub copy_path: Option<(&'static str, String)>,
}

fn fld(label: &'static str, value: impl Into<String>) -> Field {
    Field { label, value: value.into(), note: None, mono: false, cap: 3 }
}
fn note(mut f: Field, n: impl Into<String>) -> Field {
    f.note = Some(n.into());
    f
}
fn mono(mut f: Field, cap: usize) -> Field {
    f.mono = true;
    f.cap = cap;
    f
}

fn launcher_field(label: &'static str, procs: &[Proc], p: &Proc) -> Field {
    match launcher_of(procs, p) {
        Launcher::None => fld(label, "None recorded"),
        Launcher::Verified { pid, name } => note(fld(label, format!("{name} · PID {pid}")), "Verified: started before this process and still running. A launcher can name another parent on purpose, so this shows where it started, not who is responsible."),
        Launcher::Unknown(why) => note(fld(label, "Unknown"), why),
    }
}

fn user_field(label: &'static str, u: User) -> Field {
    match u {
        User::Pending => fld(label, "Looking up…"),
        User::Known { name, elevated } => {
            let f = fld(label, name.to_string());
            match elevated {
                Some(true) => note(f, "Elevated token (administrator)"),
                Some(false) => note(f, "Standard token"),
                None => f,
            }
        }
        User::Unavailable(why) => note(fld(label, "Unavailable"), why),
    }
}

/// The rule block for one process image: configured and last result apart, and what is not known.
fn rule_fields(out: &mut Vec<Field>, view: &RuleView, path: Option<&str>) {
    match view.mark(path) {
        Mark::Unknown => out.push(note(fld("Rule", "Not matched"), "The image path is not readable (protected or system process), so no rule can match it.")),
        Mark::NoRule => out.push(note(fld("Rule", "No matching rule"), "Nothing is applied to it by Hayate.")),
        Mark::Rule(id) | Mark::Off(id) => {
            let Some(r) = view.get(id) else { return };
            out.push(fld("Rule", format!("{} · {}", r.pattern, if r.enabled { "enabled" } else { "switched off" })));
            out.push(note(fld("Configured", configured_text(r)), "Saved by Hayate; not a reading of the settings Windows has for this process now."));
            out.push(note(fld("Last result", last_text(r)), "From the last time this rule was applied to any process of it."));
        }
    }
}

fn exe_name(p: &Proc) -> String {
    p.name.to_string()
}

/// What the card for row `id` says. `built` is the snapshot the list's rows were built from
/// (`pv.groups` index into it); `user` answers the lazy account lookup. None: the row is gone.
pub fn build(pv: &ProcView, built: &Snapshot, id: RowId, rules: Option<&RuleView>, user: &dyn Fn(u32, i64) -> User) -> Option<CardData> {
    let (pid, ct, is_group) = id;
    let procs = &built.procs;
    if is_group {
        let g = pv.groups.iter().find(|g| g.key == (pid, ct))?;
        let members: Vec<&Proc> = g.members.iter().filter_map(|&i| procs.get(i)).collect();
        let root = members.iter().find(|p| (p.pid, p.create) == g.key).or(members.first()).copied()?;
        if members.len() == 1 {
            return Some(single(root, procs, rules, user, Some(g.name.clone())));
        }
        return Some(group(g.name.clone(), root, &members, procs, built, rules, user));
    }
    let p = procs.iter().find(|p| p.pid == pid && p.create == ct)?;
    Some(single(p, procs, rules, user, None))
}

fn single(p: &Proc, procs: &[Proc], rules: Option<&RuleView>, user: &dyn Fn(u32, i64) -> User, plain_name: Option<String>) -> CardData {
    let wl = p.meta.workload.as_deref();
    let title = wl.map(|w| w.label.clone()).or(plain_name).unwrap_or_else(|| p.name.to_string());
    let runtime = crate::sample::workload::runtime_of(&p.name);
    let mut fields = Vec::new();
    match (wl, runtime) {
        (Some(w), _) => fields.push(note(fld("Running", format!("{}: {}", w.what, w.detail)).tap_mono(), "Inferred from the command line text only; Hayate does not run or open anything to find this.")),
        (None, Some(rt)) => fields.push(note(fld("Running", format!("{rt} workload not identified")), "The command line does not name one clearly, so Hayate does not guess.")),
        _ => {}
    }
    fields.push(fld("Process", format!("{} · PID {}", exe_name(p), p.pid)));
    fields.push(launcher_field("Launched by", procs, p));
    fields.push(user_field("User", user(p.pid, p.create)));
    fields.push(match p.meta.path.as_deref() {
        Some(path) => mono(fld("Executable", path), 6),
        None => note(fld("Executable", "Not readable"), "Protected or system process, or access denied."),
    });
    fields.push(match p.meta.cmd.as_deref() {
        Some(c) => mono(fld("Command", c), 14),
        None => note(fld("Command", "Not readable"), "Protected or system process, or access denied."),
    });
    if let Some(v) = rules {
        rule_fields(&mut fields, v, p.meta.path.as_deref());
    }
    CardData {
        title,
        chip: wl.map(|_| exe_name(p)),
        sub: wl.map(|w| format!("{} workload, inferred from the command line", w.runtime)),
        fields,
        copy_cmd: p.meta.cmd.as_deref().map(|c| ("Copy command", c.to_string())),
        copy_path: p.meta.path.as_deref().map(|c| ("Copy path", c.to_string())),
    }
}

impl Field {
    fn tap_mono(self) -> Self {
        let mut f = self;
        f.mono = true;
        f.cap = 4;
        f
    }
}

fn distinct<'a>(it: impl Iterator<Item = Option<&'a str>>) -> usize {
    let mut v: Vec<Option<&str>> = it.collect();
    v.sort();
    v.dedup();
    v.len()
}

fn group(name: String, root: &Proc, members: &[&Proc], procs: &[Proc], built: &Snapshot, rules: Option<&RuleView>, user: &dyn Fn(u32, i64) -> User) -> CardData {
    let n = members.len();
    let mut fields = Vec::new();
    // who is in it
    let mut list: Vec<String> = Vec::new();
    let mut ordered: Vec<&Proc> = vec![root];
    ordered.extend(members.iter().copied().filter(|p| (p.pid, p.create) != (root.pid, root.create)));
    for p in ordered.iter().take(6) {
        let w = p.meta.workload.as_ref().map(|w| format!(" · {}", w.label)).unwrap_or_default();
        list.push(format!("{} · PID {}{}", exe_name(p), p.pid, w));
    }
    if n > 6 {
        list.push(format!("and {} more", n - 6));
    }
    fields.push(Field { cap: 8, ..note(fld("Processes", list.join("\n")), "Each process has its own path, command line, account and rules; this card does not stand for all of them.") });
    // workloads
    let with_wl = members.iter().filter(|p| p.meta.workload.is_some()).count();
    let wl_kinds = {
        let mut v: Vec<String> = members.iter().filter_map(|p| p.meta.workload.as_ref().map(|w| format!("{} ({})", w.label, w.runtime))).collect();
        v.sort();
        v.dedup();
        v
    };
    let runtimes = members.iter().filter(|p| crate::sample::workload::runtime_of(&p.name).is_some()).count();
    if with_wl > 0 {
        let v = if wl_kinds.len() == 1 && with_wl == n { wl_kinds[0].clone() } else { format!("Mixed: {}{}", wl_kinds.join(", "), if with_wl < n { format!(" (+{} not identified)", n - with_wl) } else { String::new() }) };
        fields.push(note(Field { cap: 4, ..fld("Workload", v) }, "Inferred from each command line."));
    } else if runtimes > 0 {
        fields.push(fld("Workload", "Not identified for any runtime process here"));
    }
    // executables / commands: the same for all, or the root's with a count
    let paths = distinct(members.iter().map(|p| p.meta.path.as_deref()));
    fields.push(match root.meta.path.as_deref() {
        Some(p) if paths == 1 => mono(fld("Executable", p), 6),
        Some(p) => note(mono(fld("Root executable", p), 6), format!("{paths} different executables in this group.")),
        None => note(fld("Executable", "Not readable"), "Protected or system process, or access denied."),
    });
    let cmds = distinct(members.iter().map(|p| p.meta.cmd.as_deref()));
    fields.push(match root.meta.cmd.as_deref() {
        Some(c) if cmds == 1 => mono(fld("Command", c), 12),
        Some(c) => note(mono(fld("Root command", c), 12), format!("{cmds} different command lines in this group.")),
        None => note(fld("Command", "Not readable"), "Protected or system process, or access denied."),
    });
    fields.push(launcher_field("Root launched by", procs, root));
    fields.push(note(user_field("Root user", user(root.pid, root.create)), "Other processes in the group may run as another account."));
    if let Some(v) = rules {
        let m: Vec<(u32, i64, String)> = members.iter().map(|p| (p.pid, p.create, p.name.to_string())).collect();
        // rule state of each member, summarised: members of one app can differ
        let st = member_states(v, built, &m);
        fields.push(note(fld("Rules", summary_line(v, &st)), "Right-click the row → View rule(s) to inspect each process."));
    }
    CardData {
        title: name,
        chip: None,
        sub: Some(format!("Group of {n} processes")),
        fields,
        copy_cmd: root.meta.cmd.as_deref().map(|c| ("Copy root command", c.to_string())),
        copy_path: root.meta.path.as_deref().map(|c| ("Copy root path", c.to_string())),
    }
}

// ---------- the overlay ----------

const CARD_BG: usize = 0;
const COPY_CMD: usize = 1;
const COPY_PATH: usize = 2;

fn blocked(app: &App) -> bool {
    app.peek.is_some() || app.cpu.is_some() || app.setup.panel.is_some() || app.menu_open
}

fn row_index(pv: &ProcView, id: RowId) -> Option<usize> {
    pv.row_ids.iter().position(|r| *r == Some(id))
}

/// Whether the text of cell (`ri`, `ci`) is cut off now, with the text it draws.
fn clipped_text(app: &App, g: &mut Gfx, ri: usize, ci: usize) -> Option<(bool, String, Font)> {
    let pv = &app.procs;
    let built = pv.built.as_ref()?;
    let (grp, pi) = match *pv.rows.get(ri)? {
        Row::Group(gi) => (Some(pv.groups.get(gi)?), pv.groups.get(gi)?.members[0]),
        Row::Child(_, pi) => (None, pi),
        _ => return None,
    };
    let p = built.procs.get(pi)?;
    let single = grp.map(|g| g.members.len() == 1).unwrap_or(true);
    let t = cell_text(pv.cols[ci].col, grp, p, single)?;
    if t.s.is_empty() {
        return None;
    }
    let cell = pv.cell_rect(ri, ci)?;
    let w = g.measure(&t.s, t.font);
    let clipped = is_clipped(w, cell, t.font.align == Align::Right, pv.list);
    Some((clipped, t.s, t.font))
}

/// Run after every tab has painted: decides what help is up, draws it, and registers the card's
/// hit regions last so they win over the rows beneath it.
pub fn overlay(app: &mut App, g: &mut Gfx) {
    if app.tab != Tab::Processes {
        app.hov = HoverState::default();
        return;
    }
    let now = Instant::now();
    let blk = blocked(app);
    let (mx, my) = (app.mx, app.my);
    let raw = if mx >= 0.0 { app.procs.cell_at(mx, my) } else { None };
    app.hov.last_cell = raw;
    // a cell counts only where the row is what the pointer would hit: nothing above it
    let hit = if mx >= 0.0 { app.hit(mx, my) } else { None };
    let cell = raw.filter(|&(ri, _)| hit == Some(Hit::Row(ri)));
    let key = cell.and_then(|(ri, ci)| app.procs.id_at(ri).map(|id| CellKey { id, col: app.procs.cols[ci].col }));

    // a keyboard card follows the selection
    if let Some(c) = app.hov.card.as_mut() {
        if c.via_key {
            match app.procs.sel {
                Some(s) if app.procs.row_ids.contains(&Some(s)) => c.id = s,
                _ => app.hov.card = None,
            }
        }
    }
    if let Some(c) = &app.hov.card {
        let over = c.rect.contains(mx, my);
        let shown = row_index(&app.procs, c.id).is_some();
        if !keep_card(c, key, over, blk, shown, (app.procs.scroll, app.procs.hs)) {
            app.hov.card = None;
        }
    }

    let mut tip: Option<(Rect, String, Font)> = None;
    if app.hov.card.is_none() {
        // clipping is measured only for the one cell under the pointer
        let probe = cell.filter(|&(_, ci)| !matches!(app.procs.cols[ci].col, Col::Name | Col::Status)).and_then(|(ri, ci)| clipped_text(app, g, ri, ci).map(|t| (ri, ci, t)));
        let wants = match key {
            Some(k) if k.col == Col::Name => true,
            Some(_) => probe.as_ref().is_some_and(|(_, _, t)| t.0),
            None => false,
        };
        match step(&mut app.hov, key, wants, blk || app.down.is_some(), now) {
            Show::Wait(d) => app.timer_ms = Some(d.as_millis() as u32 + 8),
            Show::Card(id) => {
                app.hov.card = Some(OpenCard { id, via_key: false, rect: Rect::default(), scroll: (app.procs.scroll, app.procs.hs), copied: None, body_scroll: 0.0, body_max: 0.0 });
            }
            Show::Tip(_) => {
                if let Some((ri, ci, (_, s, font))) = probe {
                    if let Some(cr) = app.procs.cell_rect(ri, ci).and_then(|r| super::cpu::visible_rect(r, app.procs.list)) {
                        tip = Some((cr, s, font));
                    }
                }
            }
            Show::Nothing => {}
        }
    }
    if app.hov.card.is_some() {
        draw_card(app, g);
    } else if let Some((anchor, text, font)) = tip {
        draw_full_tip(app, g, anchor, &text, font);
    }
}

/// Ctrl+I: the card for the selected row from the keyboard (again: close it).
pub fn toggle_key(app: &mut App) -> Action {
    if app.hov.card.take().is_some() {
        return Action::Repaint;
    }
    let Some(id) = app.procs.sel.filter(|s| app.procs.row_ids.contains(&Some(*s))) else { return Action::None };
    app.hov.card = Some(OpenCard { id, via_key: true, rect: Rect::default(), scroll: (app.procs.scroll, app.procs.hs), copied: None, body_scroll: 0.0, body_max: 0.0 });
    app.hov.dismissed = None;
    Action::Repaint
}

pub fn click(app: &mut App, h: Hit) -> Action {
    let Hit::Card(i) = h else { return Action::None };
    let which = match i {
        COPY_CMD | COPY_PATH => i,
        _ => return Action::None,
    };
    let Some(id) = app.hov.card.as_ref().map(|c| c.id) else { return Action::None };
    let Some(built) = app.procs.built.clone() else { return Action::None };
    // the text of the process the card shows, resolved against its identity now
    let Some(data) = build(&app.procs, &built, id, None, &|_, _| User::Pending) else { return Action::None };
    let text = if which == COPY_CMD { data.copy_cmd } else { data.copy_path };
    match text {
        Some((_, t)) => {
            if let Some(c) = app.hov.card.as_mut() {
                c.copied = Some(which);
            }
            Action::Clip(t)
        }
        None => Action::None,
    }
}

struct Wrapped {
    label: &'static str,
    lines: Vec<String>,
    omitted: usize,
    notes: Vec<String>,
    mono: bool,
}

fn layout(g: &mut Gfx, data: &CardData, iw: f32, scale: f32) -> (Vec<Wrapped>, f32) {
    let mut out = Vec::new();
    let mut h = 0.0;
    for fl in &data.fields {
        let font = if fl.mono { f(11.0).m() } else { f(11.5) };
        let cap = ((fl.cap as f32 * scale).round() as usize).max(2);
        let (lines, omitted) = wrap_hard(&fl.value, cap, |s| g.measure(s, font) <= iw);
        let notes = match &fl.note {
            Some(n) => wrap_hard(n, 5, |s| g.measure(s, f(10.5)) <= iw).0,
            None => Vec::new(),
        };
        h += (lines.len() + usize::from(omitted > 0)) as f32 * LINE_H + notes.len() as f32 * 14.0 + 7.0;
        out.push(Wrapped { label: fl.label, lines, omitted, notes, mono: fl.mono });
    }
    (out, h)
}

fn draw_card(app: &mut App, g: &mut Gfx) {
    let (w, h) = (app.w, app.h);
    let Some(id) = app.hov.card.as_ref().map(|c| c.id) else { return };
    let Some(ri) = row_index(&app.procs, id) else {
        app.hov.card = None;
        return;
    };
    let Some(name_ci) = app.procs.cols.iter().position(|c| c.col == Col::Name) else { return };
    let anchor = app.procs.cell_rect(ri, name_ci).and_then(|r| super::cpu::visible_rect(r, app.procs.list));
    let Some(anchor) = anchor else {
        app.hov.card = None;
        return;
    };
    let Some(built) = app.procs.built.clone() else { return };
    let rules = crate::features::get().map(|ft| ft.rules.view());
    let Some(data) = build(&app.procs, &built, id, rules.as_ref(), &who::lookup) else {
        app.hov.card = None;
        return;
    };

    let cw = CARD_W.min(w - 16.0);
    let iw = cw - 28.0 - LABEL_W;
    let top = TITLE_H + 6.0;
    let bottom = h - STATUS_H - 4.0;
    let head_h = if data.sub.is_some() { 57.0 } else { 40.0 };
    let foot_h = 42.0;
    let room = (bottom - top).max(120.0);
    let mut chosen = layout(g, &data, iw, 1.0);
    for scale in [0.6, 0.35] {
        if head_h + chosen.1 + foot_h <= room {
            break;
        }
        chosen = layout(g, &data, iw, scale);
    }
    let (fields, fh) = chosen;
    let ch = (head_h + fh + foot_h).min(room);
    let x = anchor.x.min(w - cw - 8.0).max(8.0);
    // directly under the cell (no gap to cross with the pointer), else directly above
    let mut y = anchor.b();
    if y + ch > bottom {
        y = (anchor.y - ch).max(top);
        if y + ch > bottom {
            y = (bottom - ch).max(top);
        }
    }
    let r = Rect::new(x, y, cw, ch);
    g.round(r, 8.0, PAN2);
    g.round_stroke(r, 8.0, LN, 1.0);
    app.hits.push((r, Hit::Card(CARD_BG)));
    g.clip(r);

    let lx = r.x + 14.0;
    let vx = lx + LABEL_W;
    let mut ty = r.y + 8.0;
    let chip_w = data.chip.as_ref().map(|c| g.measure(c, f(10.5)) + 14.0).unwrap_or(0.0);
    let avail = r.w - 28.0;
    let tw = g.measure(&data.title, f(13.0).b()).min((avail - chip_w - 16.0).max(60.0));
    g.text(&data.title, Rect::new(lx, ty, tw + 8.0, 20.0), f(13.0).b(), TX1);
    if let Some(chip) = &data.chip {
        let cr = Rect::new(lx + tw + 14.0, ty + 3.0, chip_w, 15.0);
        g.round_stroke(cr, 7.5, LN, 1.0);
        g.text(chip, cr, f(10.5).c(), C_NET);
    }
    ty += 21.0;
    if let Some(sub) = &data.sub {
        g.text(sub, Rect::new(lx, ty, avail, 15.0), f(11.0), TX3);
        ty += 17.0;
    }
    g.fill(Rect::new(r.x + 1.0, ty + 4.0, r.w - 2.0, 1.0), SEP);
    ty += 11.0;
    let body = Rect::new(r.x + 1.0, ty, r.w - 2.0, (r.b() - foot_h - ty).max(0.0));
    let max_scroll = (fh - body.h).max(0.0);
    if let Some(c) = app.hov.card.as_mut() {
        c.body_max = max_scroll;
        c.body_scroll = c.body_scroll.clamp(0.0, max_scroll);
        ty -= c.body_scroll;
    }
    g.clip(body);
    for fl in &fields {
        g.text(fl.label, Rect::new(lx, ty, LABEL_W - 6.0, LINE_H), f(11.0), TX3);
        let font = if fl.mono { f(11.0).m() } else { f(11.5) };
        for l in &fl.lines {
            g.text(l, Rect::new(vx, ty, iw, LINE_H), font, TX1);
            ty += LINE_H;
        }
        if fl.omitted > 0 {
            g.text(&format!("… {} more characters not shown (use Copy)", fl.omitted), Rect::new(vx, ty, iw, LINE_H), f(10.5), TX3);
            ty += LINE_H;
        }
        for l in &fl.notes {
            g.text(l, Rect::new(vx, ty, iw, 14.0), f(10.5), TX4);
            ty += 14.0;
        }
        ty += 7.0;
    }
    g.unclip();
    if max_scroll > 0.0 {
        g.text("Scroll for more", Rect::new(r.r() - 105.0, r.b() - foot_h, 95.0, 12.0), f(9.0), TX3);
    }
    g.unclip();

    // copy buttons (a group's copy the ROOT's text, and say so)
    let by = r.b() - 34.0;
    let mut bx = lx;
    let copied = app.hov.card.as_ref().and_then(|c| c.copied);
    for (idx, item) in [(COPY_CMD, &data.copy_cmd), (COPY_PATH, &data.copy_path)] {
        let (label, enabled) = match item {
            Some((l, _)) => (if copied == Some(idx) { "Copied".to_string() } else { l.to_string() }, true),
            None => (if idx == COPY_CMD { "Copy command" } else { "Copy path" }.to_string(), false),
        };
        let bw = g.measure(&label, f(12.0)) + 26.0;
        let br = Rect::new(bx, by, bw, 26.0);
        if br.r() <= r.r() - 8.0 {
            let hov = enabled && app.hover == Some(Hit::Card(idx));
            button(g, br, &label, hov, enabled);
            if enabled {
                app.hits.push((br, Hit::Card(idx)));
            }
        }
        bx += bw + 8.0;
    }
    if let Some(c) = app.hov.card.as_mut() {
        c.rect = r;
    }
}

fn tip_lines(text: &str, room: f32, mut fits: impl FnMut(&str) -> bool) -> (Vec<String>, usize) {
    let slots = ((room - 14.0).max(0.0) / LINE_H) as usize;
    let (lines, omitted) = wrap_hard(text, slots, &mut fits);
    if omitted > 0 {
        // The omission notice occupies a line too.
        wrap_hard(text, slots.saturating_sub(1), fits)
    } else {
        (lines, omitted)
    }
}

/// Plain tooltip: the exact text of one clipped cell, wrapped to the window, never past its
/// edge. Only text that does not fit the space at all is cut, with a count of what is left out.
fn draw_full_tip(app: &mut App, g: &mut Gfx, a: Rect, text: &str, font: Font) {
    let (w, h) = (app.w, app.h);
    let font = Font { align: Align::Left, ..font };
    let tw = (w - 16.0).clamp(160.0, 640.0);
    let iw = tw - 20.0;
    let top = TITLE_H + 4.0;
    let bottom = h - STATUS_H - 4.0;
    let below = bottom - a.b() - 4.0;
    let above = a.y - top - 4.0;
    let want_h = |lines: usize| lines as f32 * LINE_H + 14.0;
    // first measure how many lines the text needs, up to what the roomier side holds
    let side_below = {
        let (l, o) = tip_lines(text, below, |s| g.measure(s, font) <= iw);
        (l, o)
    };
    let (lines, omitted, under) = if side_below.1 == 0 || below >= above {
        (side_below.0, side_below.1, true)
    } else {
        let (l, o) = tip_lines(text, above, |s| g.measure(s, font) <= iw);
        (l, o, false)
    };
    let n = lines.len() + usize::from(omitted > 0);
    let th = want_h(n);
    let x = a.x.min(w - tw - 8.0).max(8.0);
    let y = if under { a.b() + 4.0 } else { (a.y - th - 4.0).max(top) };
    let r = Rect::new(x, y, tw, th);
    g.round(r, 6.0, PAN3);
    g.round_stroke(r, 6.0, LN, 1.0);
    let mut ly = r.y + 7.0;
    for l in &lines {
        g.text(l, Rect::new(r.x + 10.0, ly, iw, LINE_H), font, TX1);
        ly += LINE_H;
    }
    if omitted > 0 {
        g.text(&format!("… {omitted} more characters (Ctrl+I shows the full card)"), Rect::new(r.x + 10.0, ly, iw, LINE_H), f(10.5), TX3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::workload::Workload;
    use std::sync::Arc;

    fn proc(pid: u32, ppid: u32, create: i64, name: &str) -> Proc {
        Proc { pid, ppid, name: name.into(), create, cpu: 0.0, mem: 1, io_bps: 0.0, threads: 1, handles: 1, session: 1, windowed: true, meta: Arc::new(Default::default()), gpu: 0.0, gpu_engine: None }
    }
    fn with_meta(mut p: Proc, path: &str, cmd: &str) -> Proc {
        let wl = crate::sample::workload::infer(path, cmd).map(Arc::new);
        p.meta = Arc::new(crate::sample::meta::Meta { path: Some(path.into()), cmd: Some(cmd.into()), workload: wl, ..Default::default() });
        p
    }
    fn nobody(_: u32, _: i64) -> User {
        User::Unavailable("test")
    }

    // ---- wrapping ----

    fn fit10(s: &str) -> bool {
        s.chars().count() <= 10
    }

    #[test]
    fn tooltip_reserves_space_for_omission_notice() {
        let (lines, omitted) = tip_lines(&"x".repeat(100), 74.0, fit10);
        assert_eq!(lines.len(), 3);
        assert_eq!(omitted, 70);
        assert!((lines.len() + 1) as f32 * LINE_H + 14.0 <= 74.0);
    }

    #[test]
    fn card_wheel_keeps_body_scroll_bounded() {
        let mut app = App::new();
        app.hov.card = Some(OpenCard { id: (100, 10, false), via_key: true, rect: Rect::default(), scroll: (0.0, 0.0), copied: None, body_scroll: 0.0, body_max: 90.0 });
        assert!(wheel(&mut app, -1000.0));
        assert_eq!(app.hov.card.as_ref().unwrap().body_scroll, 90.0);
        assert!(wheel(&mut app, 1000.0));
        assert_eq!(app.hov.card.as_ref().unwrap().body_scroll, 0.0);
    }

    #[test]
    fn wrapped_text_is_the_exact_string_and_never_overflows() {
        for s in [r"C:\Users\you\AppData\Local\Discord\app-1.0.9220\Discord.exe --processStart Discord.exe", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "ünïcödé 日本語 text that is long enough to wrap around", "short"] {
            let (lines, omitted) = wrap_hard(s, 100, fit10);
            assert_eq!(omitted, 0);
            assert!(lines.iter().all(|l| l.chars().count() <= 10), "{lines:?}");
            assert_eq!(lines.concat(), s, "lines joined are the string, nothing added or dropped");
        }
        // an unbreakable run is cut anywhere, a path breaks after a separator
        let (lines, _) = wrap_hard(r"C:\Program Files\Tool\x.exe", 10, fit10);
        assert_eq!(lines.concat(), r"C:\Program Files\Tool\x.exe");
        assert!(lines.iter().all(|l| l.chars().count() <= 10), "{lines:?}");
        let (lines, _) = wrap_hard(r"abcdefg\hijklmnop", 10, fit10);
        assert_eq!(lines, vec![r"abcdefg\".to_string(), "hijklmnop".into()], "a path breaks after a separator when one is near");
    }

    #[test]
    fn wrapping_counts_what_a_line_cap_leaves_out() {
        let s = "x".repeat(95);
        let (lines, omitted) = wrap_hard(&s, 3, fit10);
        assert_eq!((lines.len(), omitted), (3, 65));
        let (lines, omitted) = wrap_hard("a\nb\n\nc", 10, fit10);
        assert_eq!((lines, omitted), (vec!["a".to_string(), "b".into(), String::new(), "c".into()], 0));
        assert_eq!(wrap_hard("", 5, fit10).0, vec![String::new()]);
        let (lines, omitted) = wrap_hard("one\ntwo\nthree", 2, fit10);
        assert_eq!((lines.len(), omitted), (2, 5));
    }

    // ---- clipping ----

    #[test]
    fn a_cell_is_clipped_only_when_its_text_is_cut() {
        let view = Rect::new(0.0, 40.0, 800.0, 400.0);
        let cell = Rect::new(100.0, 100.0, 120.0, 22.0); // 104 px of text room
        assert!(!is_clipped(90.0, cell, false, view), "fits");
        assert!(!is_clipped(104.0, cell, false, view), "exactly fits");
        assert!(is_clipped(140.0, cell, false, view), "ellipsised by the column");
        assert!(is_clipped(140.0, cell, true, view), "right aligned numbers too");
        // a column partly scrolled out of the list: the text is cut by the view even though it fits its cell
        let edge = Rect::new(750.0, 100.0, 120.0, 22.0);
        assert!(is_clipped(90.0, edge, false, view), "cut by the right edge");
        assert!(!is_clipped(30.0, edge, false, view), "text still fully inside");
        let left = Rect::new(-60.0, 100.0, 120.0, 22.0);
        assert!(is_clipped(80.0, left, false, view), "cut by the left edge");
        assert!(!is_clipped(30.0, left, true, view), "right aligned text that ends inside the view is whole");
        // a row cut by the list's top or bottom edge
        assert!(is_clipped(10.0, Rect::new(100.0, 30.0, 120.0, 22.0), false, view));
        assert!(is_clipped(10.0, Rect::new(100.0, 430.0, 120.0, 22.0), false, view));
    }

    // ---- dwell ----

    fn key(pid: u32, col: Col) -> CellKey {
        CellKey { id: (pid, 1, true), col }
    }

    #[test]
    fn help_appears_after_the_dwell_and_restarts_on_a_new_cell() {
        let mut st = HoverState::default();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let k = key(1, Col::Name);
        assert_eq!(step(&mut st, Some(k), true, false, ms(0)), Show::Wait(Duration::from_millis(DWELL_MS)));
        assert_eq!(step(&mut st, Some(k), true, false, ms(100)), Show::Wait(Duration::from_millis(DWELL_MS - 100)));
        assert_eq!(step(&mut st, Some(k), true, false, ms(DWELL_MS)), Show::Card((1, 1, true)));
        // another cell restarts the wait, the first one's time does not carry over
        let k2 = key(1, Col::Path);
        assert_eq!(step(&mut st, Some(k2), true, false, ms(DWELL_MS + 10)), Show::Wait(Duration::from_millis(DWELL_MS)));
        assert_eq!(step(&mut st, Some(k2), true, false, ms(DWELL_MS + 10 + DWELL_MS)), Show::Tip(k2));
        // an unclipped cell has nothing to say, however long the pointer rests
        let k3 = key(2, Col::Cpu);
        assert_eq!(step(&mut st, Some(k3), false, false, ms(5000)), Show::Nothing);
        assert_eq!(step(&mut st, Some(k3), false, false, ms(9000)), Show::Nothing);
        // leaving the list forgets everything
        assert_eq!(step(&mut st, None, false, false, ms(9100)), Show::Nothing);
        assert_eq!(step(&mut st, Some(k), true, false, ms(9200)), Show::Wait(Duration::from_millis(DWELL_MS)));
    }

    #[test]
    fn a_menu_a_panel_or_a_drag_blocks_help_and_restarts_the_wait() {
        let mut st = HoverState::default();
        let t0 = Instant::now();
        let k = key(1, Col::Name);
        assert!(matches!(step(&mut st, Some(k), true, false, t0), Show::Wait(_)));
        assert_eq!(step(&mut st, Some(k), true, true, t0 + Duration::from_secs(5)), Show::Nothing);
        assert!(matches!(step(&mut st, Some(k), true, false, t0 + Duration::from_secs(6)), Show::Wait(_)), "the dwell starts over once unblocked");
    }

    #[test]
    fn a_dismissed_cell_stays_quiet_until_the_pointer_leaves_it() {
        let mut st = HoverState::default();
        let t0 = Instant::now();
        let k = key(1, Col::Name);
        st.card = Some(OpenCard { id: k.id, via_key: false, rect: Rect::default(), scroll: (0.0, 0.0), copied: None, body_scroll: 0.0, body_max: 0.0 });
        assert!(st.press());
        assert!(st.card.is_none());
        let later = t0 + Duration::from_secs(10);
        assert_eq!(step(&mut st, Some(k), true, false, later), Show::Nothing, "a click does not bring the card straight back");
        assert_eq!(step(&mut st, Some(key(2, Col::Name)), true, false, later), Show::Wait(Duration::from_millis(DWELL_MS)));
        assert!(matches!(step(&mut st, Some(k), true, false, later + Duration::from_secs(1)), Show::Wait(_)), "leaving and returning is a new hover");
    }

    #[test]
    fn a_pointer_card_needs_the_pointer_the_list_and_a_clear_screen() {
        let id = (1, 1, true);
        let mut c = OpenCard { id, via_key: false, rect: Rect::new(0.0, 0.0, 10.0, 10.0), scroll: (5.0, 0.0), copied: None, body_scroll: 0.0, body_max: 0.0 };
        let name = Some(CellKey { id, col: Col::Name });
        assert!(keep_card(&c, name, false, false, true, (5.0, 0.0)), "pointer on the Name cell");
        assert!(keep_card(&c, None, true, false, true, (5.0, 0.0)), "pointer on the card");
        assert!(!keep_card(&c, None, false, false, true, (5.0, 0.0)), "pointer elsewhere");
        assert!(!keep_card(&c, Some(CellKey { id, col: Col::Path }), false, false, true, (5.0, 0.0)), "another cell of the row");
        assert!(!keep_card(&c, name, false, false, true, (6.0, 0.0)), "the list scrolled under it");
        assert!(!keep_card(&c, name, false, true, true, (5.0, 0.0)), "a menu or panel opened");
        assert!(!keep_card(&c, name, false, false, false, (5.0, 0.0)), "the row is gone");
        c.via_key = true;
        assert!(keep_card(&c, None, false, false, true, (9.0, 9.0)), "a keyboard card stays with its row");
        assert!(!keep_card(&c, None, false, false, false, (9.0, 9.0)));
    }

    // ---- launcher identity ----

    #[test]
    fn a_launcher_is_shown_only_when_it_is_the_process_that_started_this_one() {
        let child = || proc(50, 10, 1_000, "app.exe");
        // verified: exists, older, still running
        assert_eq!(launcher_of(&[proc(10, 0, 500, "launcher.exe"), child()], &child()), Launcher::Verified { pid: 10, name: "launcher.exe".into() });
        // the launcher exited: its pid is not running
        assert!(matches!(launcher_of(std::slice::from_ref(&child()), &child()), Launcher::Unknown(w) if w.contains("no longer running")));
        // the pid was reused by a process younger than the child: never the launcher
        let reused = proc(10, 0, 9_000, "other.exe");
        let l = launcher_of(&[reused, child()], &child());
        assert!(matches!(&l, Launcher::Unknown(w) if w.contains("newer process")), "{l:?}");
        // no usable times: nothing is claimed
        assert!(matches!(launcher_of(&[proc(10, 0, 0, "x.exe"), child()], &child()), Launcher::Unknown(_)));
        // no parent recorded, or itself
        assert_eq!(launcher_of(&[], &proc(7, 0, 5, "a.exe")), Launcher::None);
        assert_eq!(launcher_of(&[], &proc(7, 7, 5, "a.exe")), Launcher::None);
    }

    // ---- card content ----

    fn view(procs: Vec<Proc>) -> (ProcView, Arc<Snapshot>) {
        let mut pv = ProcView::new();
        let s = Arc::new(Snapshot { procs, ..Default::default() });
        pv.rebuild(&s, "", super::super::procs::Interact { now: 0, inside: false, active: false });
        (pv, s)
    }
    fn field<'a>(d: &'a CardData, label: &str) -> &'a Field {
        d.fields.iter().find(|f| f.label == label).unwrap_or_else(|| panic!("no {label} in {:?}", d.fields.iter().map(|f| f.label).collect::<Vec<_>>()))
    }

    #[test]
    fn a_runtime_card_names_the_workload_and_keeps_the_executable() {
        let launcher = with_meta(proc(10, 0, 100, "prism.exe"), r"C:\p\prism.exe", "prism.exe");
        let java = with_meta(proc(50, 10, 200, "javaw.exe"), r"C:\Program Files\Java\bin\javaw.exe", r#""C:\Program Files\Java\bin\javaw.exe" -Xmx16G -cp a;b net.minecraft.client.main.Main --version 1.21"#);
        let (pv, s) = view(vec![launcher, java]);
        let d = build(&pv, &s, (50, 200, true), None, &nobody).unwrap();
        assert_eq!(d.title, "net.minecraft.client.main.Main");
        assert_eq!(d.chip.as_deref(), Some("javaw.exe"), "the runtime image stays visible");
        assert!(d.sub.as_deref().unwrap().contains("inferred"), "inferred labels say so");
        assert!(field(&d, "Running").value.starts_with("Java main class: net.minecraft"));
        assert_eq!(field(&d, "Process").value, "javaw.exe · PID 50");
        assert_eq!(field(&d, "Executable").value, r"C:\Program Files\Java\bin\javaw.exe");
        assert!(field(&d, "Command").value.contains("--version 1.21") && field(&d, "Command").value.starts_with("\"C:\\Program Files"), "the full command line, quotes and all");
        assert_eq!(field(&d, "Launched by").value, "prism.exe · PID 10");
        assert!(field(&d, "Launched by").note.as_deref().unwrap().starts_with("Verified"));
        // the account is its own field, separate from the launcher
        assert_eq!(field(&d, "User").value, "Unavailable");
        assert_eq!(d.copy_cmd.as_ref().unwrap().1, field(&d, "Command").value);
        assert_eq!(d.copy_path.as_ref().unwrap().1, field(&d, "Executable").value);
    }

    #[test]
    fn an_unidentified_runtime_stays_the_runtime() {
        let j = with_meta(proc(50, 0, 200, "java.exe"), r"C:\j\java.exe", "java -version");
        let (pv, s) = view(vec![j]);
        let d = build(&pv, &s, (50, 200, true), None, &nobody).unwrap();
        assert_eq!(d.title, "java.exe", "no application identity is invented");
        assert!(d.chip.is_none() && d.sub.is_none());
        assert!(field(&d, "Running").value.contains("not identified"));
        assert_eq!(field(&d, "Launched by").value, "None recorded");
    }

    #[test]
    fn a_reused_or_exited_parent_is_never_named() {
        let child = with_meta(proc(50, 10, 200, "app.exe"), r"C:\a\app.exe", "app.exe");
        let reuser = with_meta(proc(10, 0, 900, "unrelated.exe"), r"C:\u\unrelated.exe", "unrelated.exe");
        let (pv, s) = view(vec![reuser, child]);
        let d = build(&pv, &s, (50, 200, false), None, &nobody).unwrap();
        let l = field(&d, "Launched by");
        assert_eq!(l.value, "Unknown");
        assert!(!l.value.contains("unrelated") && l.note.as_deref().unwrap().contains("newer process"));
    }

    #[test]
    fn the_account_is_whatever_the_lookup_says_and_never_the_launcher() {
        let a = with_meta(proc(10, 0, 100, "a.exe"), r"C:\a.exe", "a.exe");
        let (pv, s) = view(vec![a]);
        let known = |_: u32, _: i64| User::Known { name: r"PC\you".into(), elevated: Some(false) };
        let d = build(&pv, &s, (10, 100, true), None, &known).unwrap();
        assert_eq!(field(&d, "User").value, r"PC\you");
        assert_eq!(field(&d, "User").note.as_deref(), Some("Standard token"));
        let d = build(&pv, &s, (10, 100, true), None, &|_, _| User::Pending).unwrap();
        assert_eq!(field(&d, "User").value, "Looking up…");
    }

    #[test]
    fn a_mixed_group_is_described_member_by_member_not_by_its_root() {
        let root = with_meta(proc(10, 0, 100, "app.exe"), r"C:\a\app.exe", r#"app.exe --main"#);
        let w1 = with_meta(Proc { windowed: false, ..proc(11, 10, 110, "app.exe") }, r"C:\a\app.exe", "app.exe --type=gpu");
        let helper = with_meta(Proc { windowed: false, ..proc(12, 10, 120, "helper.exe") }, r"C:\a\helper.exe", "helper.exe");
        let (pv, s) = view(vec![root, w1, helper]);
        let d = build(&pv, &s, (10, 100, true), None, &nobody).unwrap();
        assert_eq!(d.sub.as_deref(), Some("Group of 3 processes"));
        let procs = &field(&d, "Processes").value;
        assert!(procs.starts_with("app.exe · PID 10") && procs.contains("helper.exe · PID 12"), "{procs}");
        // path and command differ across the group: the card says so and labels the root's
        assert_eq!(field(&d, "Root executable").value, r"C:\a\app.exe");
        assert!(field(&d, "Root executable").note.as_deref().unwrap().contains("2 different executables"));
        assert_eq!(field(&d, "Root command").value, "app.exe --main");
        assert!(field(&d, "Root command").note.as_deref().unwrap().contains("3 different command lines"));
        assert!(d.fields.iter().any(|f| f.label == "Root user") && !d.fields.iter().any(|f| f.label == "User"), "the account is the root's, said so");
        assert_eq!(d.copy_cmd.as_ref().unwrap().0, "Copy root command");
    }

    #[test]
    fn a_uniform_group_shares_its_values_and_a_mixed_workload_is_called_mixed() {
        let mk = |pid: u32, create: i64, cmd: &str| with_meta(Proc { windowed: pid == 10, ..proc(pid, if pid == 10 { 0 } else { 10 }, create, "node.exe") }, r"C:\n\node.exe", cmd);
        let (pv, s) = view(vec![mk(10, 100, "node a.js"), mk(11, 110, "node a.js"), mk(12, 120, "node b.js")]);
        let d = build(&pv, &s, (10, 100, true), None, &nobody).unwrap();
        assert_eq!(field(&d, "Executable").value, r"C:\n\node.exe", "one executable: stated plainly");
        let wl = &field(&d, "Workload").value;
        assert!(wl.starts_with("Mixed:") && wl.contains("a.js (Node.js)") && wl.contains("b.js (Node.js)"), "{wl}");
        // every member the same workload: said as one
        let (pv, s) = view(vec![mk(10, 100, "node a.js"), mk(11, 110, "node a.js")]);
        let d = build(&pv, &s, (10, 100, true), None, &nobody).unwrap();
        assert_eq!(field(&d, "Workload").value, "a.js (Node.js)");
    }

    #[test]
    fn rules_keep_configured_apart_from_the_last_result() {
        use crate::rules::{Actions, Priority, Rule};
        let mut r = Rule::new("app.exe", Actions { priority: Some(Priority::High), ..Default::default() });
        r.id = 3;
        r.last = Some(crate::rules::LastResult { time: 0, pid: 77, ok: false, protected: false, text: "affinity refused".into() });
        let v = RuleView::new(vec![r]);
        let a = with_meta(proc(10, 0, 100, "app.exe"), r"C:\a\app.exe", "app.exe");
        let b = with_meta(proc(11, 0, 100, "other.exe"), r"C:\a\other.exe", "other.exe");
        let (pv, s) = view(vec![a, b]);
        let d = build(&pv, &s, (10, 100, true), Some(&v), &nobody).unwrap();
        assert_eq!(field(&d, "Rule").value, "app.exe · enabled");
        assert!(field(&d, "Configured").value.starts_with("High") || field(&d, "Configured").value.contains("High"));
        assert!(field(&d, "Configured").note.as_deref().unwrap().contains("not a reading of the settings Windows has"));
        assert!(field(&d, "Last result").value.contains("PID 77") && field(&d, "Last result").value.contains("not fully applied"));
        assert!(field(&d, "Last result").note.as_deref().unwrap().contains("any process"));
        let d = build(&pv, &s, (11, 100, true), Some(&v), &nobody).unwrap();
        assert_eq!(field(&d, "Rule").value, "No matching rule");
        assert!(d.fields.iter().all(|f| f.label != "Configured"), "nothing configured is claimed for an unmatched process");
    }

    #[test]
    fn a_row_that_is_gone_has_no_card() {
        let (pv, s) = view(vec![proc(10, 0, 100, "a.exe")]);
        assert!(build(&pv, &s, (99, 1, true), None, &nobody).is_none());
        assert!(build(&pv, &s, (10, 555, false), None, &nobody).is_none(), "same pid, other create time");
        let _ = Workload { label: String::new(), runtime: "", what: "", detail: String::new() };
    }
}
