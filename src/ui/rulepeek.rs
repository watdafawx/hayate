//! Rules on a process, at a glance: the Status-column badge, its hover tip and the
//! quick-view panel (right-click → "View rule(s)").
//!
//! Matching is the engine's own (`rules::view`); nothing here enables, changes or
//! applies a rule. Two different things are kept apart on screen:
//! - what a rule is CONFIGURED to do (saved settings), and
//! - the LAST RESULT of applying it (to some process of that rule, at some time).
//!
//! Neither says what Windows has for the process right now.
//!
//! Targets are identities (pid + create time, or a group's root), never row
//! indexes: the badge click resolves its row through `ProcView::id_at` at press
//! and release (`same_press`), and the panel re-resolves its members against the
//! newest snapshot at every paint.

use super::procs::RowId;
use super::*;
use crate::features;
use crate::rules::view::{group_mark, GroupMark, Mark, RuleView, Standing};
use crate::rules::Rule;

const CLOSE: usize = 0;
const BACKDROP: usize = 1;
const PANEL_BG: usize = 2;
const BACK: usize = 3;
const CREATE: usize = 4;
/// "Open in Rules" of matching rule i = OPEN + i (index into `Peek::open_ids`)
const OPEN: usize = 100;
/// "Inspect" of group member i = MEMBER + i (index into `Peek::members`)
const MEMBER: usize = 1000;

/// One process as the panel lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberState {
    pub pid: u32,
    pub create: i64,
    pub name: String,
    pub alive: bool,
    pub path: Option<String>,
    pub mark: Mark,
}

pub struct Peek {
    name: String,
    group: bool,
    /// frozen when the panel opens: the processes the user was looking at
    members: Vec<(u32, i64, String)>,
    /// drilled into one member of a group
    member: Option<(u32, i64)>,
    scroll: f32,
    /// rule ids behind the "Open in Rules" buttons of the last paint
    open_ids: Vec<u32>,
    /// opened from a Top list menu: that menu's target, so "Create rule…" can reopen it
    /// (Top lists have no selection to point at)
    origin: Option<Box<super::procmenu::Target>>,
}

impl Peek {
    /// The one process the panel is about right now, if it is a single process.
    fn subject(&self) -> Option<(u32, i64)> {
        match (self.member, self.group) {
            (Some(m), _) => Some(m),
            (None, false) => self.members.first().map(|m| (m.0, m.1)),
            (None, true) if self.members.len() == 1 => self.members.first().map(|m| (m.0, m.1)),
            _ => None,
        }
    }
}

// ---------- state of processes ----------

/// Processes the row `id` stands for, from the snapshot its rows were built from (what the user saw).
fn members_of(app: &App, id: RowId) -> Option<(String, Vec<(u32, i64, String)>)> {
    let (pid, ct, grp) = id;
    let built = app.procs.built.as_ref()?;
    if grp {
        let g = app.procs.groups.iter().find(|g| g.key == (pid, ct))?;
        let m: Vec<_> = g.members.iter().filter_map(|&i| built.procs.get(i)).map(|p| (p.pid, p.create, p.name.to_string())).collect();
        (!m.is_empty()).then(|| (g.name.clone(), m))
    } else {
        let p = built.procs.iter().find(|p| p.pid == pid && p.create == ct)?;
        Some((p.name.to_string(), vec![(p.pid, p.create, p.name.to_string())]))
    }
}

/// Each member against the newest snapshot: still the same process (pid AND create time), its image, its rule state.
pub fn member_states(view: &RuleView, snap: &Snapshot, members: &[(u32, i64, String)]) -> Vec<MemberState> {
    members
        .iter()
        .map(|(pid, create, name)| match snap.procs.iter().find(|p| p.pid == *pid && p.create == *create) {
            Some(p) => {
                let path = p.meta.path.as_deref().map(String::from);
                MemberState { pid: *pid, create: *create, name: name.clone(), alive: true, mark: view.mark(path.as_deref()), path }
            }
            None => MemberState { pid: *pid, create: *create, name: name.clone(), alive: false, path: None, mark: Mark::Unknown },
        })
        .collect()
}

/// What a Processes row shows in its Status cell: the combined rule state of the row's processes.
pub fn row_mark(view: &RuleView, snap: &Snapshot, groups: &[super::procs::Group], row: super::procs::Row) -> GroupMark {
    use super::procs::Row;
    let mark = |i: usize| snap.procs.get(i).map(|p| view.mark(p.meta.path.as_deref()));
    match row {
        Row::Group(gi) => groups.get(gi).map(|g| group_mark(g.members.iter().filter_map(|&i| mark(i)))).unwrap_or(GroupMark::Plain),
        Row::Child(_, pi) => mark(pi).map(GroupMark::Same).unwrap_or(GroupMark::Plain),
        Row::Header(..) | Row::Gone(_) => GroupMark::Plain,
    }
}

// ---------- badge ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Look {
    Rule,
    Off,
    Mixed,
}

/// Width of the badge for `gm` in a cell with `avail` px, and whether it is the short form.
/// None: nothing to show, or no room. The badge always stays inside the cell.
fn badge_fit(gm: GroupMark, avail: f32) -> Option<(Look, f32, bool)> {
    let (look, full, short) = match gm {
        GroupMark::Same(Mark::Rule(_)) => (Look::Rule, 36.0, 18.0),
        GroupMark::Same(Mark::Off(_)) => (Look::Off, 52.0, 24.0),
        GroupMark::Mixed => (Look::Mixed, 44.0, 18.0),
        GroupMark::Same(Mark::Unknown) | GroupMark::Same(Mark::NoRule) | GroupMark::Plain => return None,
    };
    if avail >= full {
        Some((look, full, false))
    } else if avail >= short {
        Some((look, short, true))
    } else {
        None
    }
}

fn pill(g: &mut Gfx, r: Rect, look: Look, short: bool, hov: bool) {
    let (bg, ln, tx, label) = match (look, short) {
        (Look::Rule, false) => (Some(ACC_BG), ACC_LN, ACC_TX, "Rule"),
        (Look::Rule, true) => (Some(ACC_BG), ACC_LN, ACC_TX, "R"),
        (Look::Off, false) => (None, LN, TX3, "Rule off"),
        (Look::Off, true) => (None, LN, TX3, "off"),
        (Look::Mixed, false) => (Some(PAN3), LN, TX2, "Mixed"),
        (Look::Mixed, true) => (Some(PAN3), LN, TX2, "M"),
    };
    if let Some(bg) = bg {
        g.round(r, r.h / 2.0, bg);
    }
    g.round_stroke(r, r.h / 2.0, if hov { ACC } else { ln }, 1.0);
    g.text(label, r, f(10.5).c(), tx);
}

/// The badge of one Processes row, inside the Status cell `cell` (never wider than the cell, so no column moves).
pub fn badge(hits: &mut Vec<(Rect, Hit)>, g: &mut Gfx, cell: Rect, ri: usize, gm: GroupMark, hov: bool) {
    let pad = cell.inset(8.0, 0.0);
    let Some((look, w, short)) = badge_fit(gm, pad.w) else { return };
    let r = Rect::new(pad.x, cell.y + 3.0, w, 16.0);
    pill(g, r, look, short, hov);
    hits.push((r, Hit::RuleBadge(ri)));
}

// ---------- text ----------

/// "none yet", or when / which process / what happened. The last time the rule was applied to
/// ANY process of that rule, not necessarily the one being looked at.
pub(super) fn last_text(r: &Rule) -> String {
    match &r.last {
        None => "none yet".into(),
        Some(l) => {
            let lock = if l.protected { "protected · " } else { "" };
            let state = if l.ok { "" } else { " (not fully applied)" };
            format!("{} · PID {} · {lock}{}{state}", super::rules::ago(l.time), l.pid, l.text)
        }
    }
}

/// What the rule is saved to apply (settings, not a reading of Windows).
pub(super) fn configured_text(r: &Rule) -> String {
    let s = r.actions.summary();
    if s.is_empty() {
        "no actions".to_string()
    } else {
        s
    }
}

fn last_line(r: &Rule) -> String {
    match &r.last {
        None => "Last result: none yet".into(),
        Some(_) => format!("Last result (any process of this rule): {}", last_text(r)),
    }
}

fn configured_line(r: &Rule) -> String {
    format!("Configured: {}", configured_text(r))
}

const HONEST: &str = "Configured is what the rule is set to apply. It is saved by Hayate, not a reading of Windows: the process, or another program, may have changed those settings since.";

/// One line about a set of processes: all the same rule, or a count of each state.
pub fn summary_line(view: &RuleView, states: &[MemberState]) -> String {
    let alive: Vec<&MemberState> = states.iter().filter(|m| m.alive).collect();
    let gone = states.len() - alive.len();
    let (mut on, mut off, mut none, mut unk) = (0, 0, 0, 0);
    for m in &alive {
        match m.mark {
            Mark::Rule(_) => on += 1,
            Mark::Off(_) => off += 1,
            Mark::NoRule => none += 1,
            Mark::Unknown => unk += 1,
        }
    }
    let mut s = match group_mark(alive.iter().map(|m| m.mark)) {
        GroupMark::Same(Mark::Rule(id)) => format!("All {} running processes match {}.", alive.len(), view.get(id).map(|r| r.pattern.as_str()).unwrap_or("a rule")),
        GroupMark::Same(Mark::Off(id)) => format!("All {} running processes match {}, which is switched off.", alive.len(), view.get(id).map(|r| r.pattern.as_str()).unwrap_or("a rule")),
        GroupMark::Same(Mark::NoRule) => format!("No rule matches any of the {} running processes.", alive.len()),
        GroupMark::Same(Mark::Unknown) => "Image paths are not readable, so rules cannot be matched.".to_string(),
        GroupMark::Mixed | GroupMark::Plain => {
            let mut parts = vec![format!("{on} with a rule"), format!("{off} with a switched-off rule"), format!("{none} without")];
            if unk > 0 {
                parts.push(format!("{unk} unreadable"));
            }
            format!("Mixed: {} (of {} running).", parts.join(", "), alive.len())
        }
    };
    if gone > 0 {
        s.push_str(&format!(" {gone} exited since this opened."));
    }
    s
}

/// Hover tip of a badge for the running processes of its row (`marks`). None: nothing to say.
pub fn tip_paras(view: &RuleView, marks: &[Mark]) -> Option<Vec<String>> {
    let hint = "Click: open it in Rules. Right-click → View rule(s) for every match.".to_string();
    match group_mark(marks.iter().copied()) {
        GroupMark::Same(Mark::Rule(id)) => {
            let r = view.get(id)?;
            let mut v = vec![format!("Rule: {}", r.pattern), format!("Enabled · {}", configured_line(r)), last_line(r)];
            if marks.len() > 1 {
                v.insert(1, format!("Applies to all {} processes of this row.", marks.len()));
            }
            v.push(hint);
            Some(v)
        }
        GroupMark::Same(Mark::Off(id)) => {
            let r = view.get(id)?;
            Some(vec![
                format!("Rule off: {}", r.pattern),
                "Switched off: nothing is applied to this process until it is enabled in Rules.".into(),
                configured_line(r),
                last_line(r),
                hint,
            ])
        }
        GroupMark::Mixed => {
            let n = |f: fn(&Mark) -> bool| marks.iter().filter(|m| f(m)).count();
            Some(vec![
                "Mixed rules".into(),
                format!(
                    "{} of {} processes have a rule, {} a switched-off one, {} none. Rules match each process's own image.",
                    n(|m| matches!(m, Mark::Rule(_))),
                    marks.len(),
                    n(|m| matches!(m, Mark::Off(_))),
                    n(|m| matches!(m, Mark::NoRule | Mark::Unknown)),
                ),
                "Click to inspect each process.".into(),
            ])
        }
        _ => None,
    }
}

/// The tip for the badge of visible row `ri`, from what the row stands for right now.
pub fn badge_tip(app: &App, ri: usize) -> Option<Vec<String>> {
    let id = app.procs.id_at(ri)?;
    let (_, members) = members_of(app, id)?;
    let view = features::get()?.rules.view();
    let marks: Vec<Mark> = member_states(&view, &app.snap, &members).into_iter().filter(|m| m.alive).map(|m| m.mark).collect();
    tip_paras(&view, &marks)
}

// ---------- opening ----------

/// What a click on the badge does, from the row's combined state. A rule that no longer
/// exists, or mixed members, open the panel instead of guessing.
#[derive(Debug, PartialEq)]
pub enum BadgeClick {
    OpenRule(u32),
    Panel,
}

pub fn badge_click_for(gm: GroupMark, exists: impl Fn(u32) -> bool) -> BadgeClick {
    match gm {
        GroupMark::Same(Mark::Rule(id)) | GroupMark::Same(Mark::Off(id)) if exists(id) => BadgeClick::OpenRule(id),
        _ => BadgeClick::Panel,
    }
}

/// One click on the badge of visible row `i`. Resolves the row's identity now; selects nothing else.
pub fn badge_click(app: &mut App, i: usize) -> Action {
    let Some(id) = app.procs.id_at(i) else { return Action::None };
    let Some((_, members)) = members_of(app, id) else { return Action::None };
    let Some(ft) = features::get() else { return Action::None };
    let view = ft.rules.view();
    let marks: Vec<Mark> = member_states(&view, &app.snap, &members).into_iter().filter(|m| m.alive).map(|m| m.mark).collect();
    if marks.is_empty() {
        return Action::Repaint; // exited between the paint and the click: nothing to open
    }
    match badge_click_for(group_mark(marks), |rid| view.get(rid).is_some()) {
        BadgeClick::OpenRule(rid) => open_rule(app, rid),
        BadgeClick::Panel => {
            open(app, id);
            Action::Repaint
        }
    }
}

/// Open the quick view for a row identity (context menu "View rule(s)", badge of a mixed row).
pub fn open(app: &mut App, id: RowId) {
    let Some((name, members)) = members_of(app, id) else { return };
    super::setup::close(app);
    app.cpu = None;
    app.disallow = None;
    let single = members.len() == 1;
    let member = if single { members.first().map(|m| (m.0, m.1)) } else { None };
    app.peek = Some(Peek { name, group: id.2, members, member, scroll: 0.0, open_ids: Vec::new(), origin: None });
}

/// Open the quick view for the processes a context menu captured when it opened (the menu's
/// "View rule(s)"): exactly those members, not whatever a list shows by the time this runs.
pub fn open_target(app: &mut App, t: &super::procmenu::Target) {
    if t.members.is_empty() {
        return;
    }
    super::setup::close(app);
    app.cpu = None;
    app.disallow = None;
    let member = if t.members.len() == 1 { t.members.first().map(|m| (m.0, m.1)) } else { None };
    let origin = (t.origin == super::procmenu::Origin::Top).then(|| Box::new(t.clone()));
    let name = if t.group { t.label.clone() } else { t.rep.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| t.label.clone()) };
    app.peek = Some(Peek { name, group: t.group, members: t.members.clone(), member, scroll: 0.0, open_ids: Vec::new(), origin });
}

/// Select one rule in the Rules tab and scroll it into view. Changes nothing about the rule.
pub fn open_rule(app: &mut App, id: u32) -> Action {
    let Some(ft) = features::get() else { return Action::None };
    if ft.rules.get(id).is_none() {
        return Action::Repaint; // deleted meanwhile; the panel (if open) shows the current state
    }
    app.peek = None;
    app.switch_tab(Tab::Rules);
    app.rules.sel = Some(id);
    app.rules.reveal = Some(id);
    Action::Repaint
}

pub fn close(app: &mut App) {
    app.peek = None;
}

pub fn wheel(app: &mut App, delta: f32) -> bool {
    match app.peek.as_mut() {
        Some(p) => {
            p.scroll = (p.scroll - delta * 0.5).max(0.0);
            true
        }
        None => false,
    }
}

pub fn click(app: &mut App, h: Hit) -> Action {
    let Hit::Peek(id) = h else { return Action::None };
    match id {
        CLOSE | BACKDROP => {
            app.peek = None;
            Action::Repaint
        }
        PANEL_BG => Action::None,
        BACK => {
            if let Some(p) = app.peek.as_mut() {
                p.member = None;
                p.scroll = 0.0;
            }
            Action::Repaint
        }
        CREATE => {
            // the existing create-rule path is the row's context menu: only for the selected process itself
            let Some(subject) = app.peek.as_ref().and_then(|p| p.subject()) else { return Action::None };
            // from a Top list: the menu that opened this, when the subject is its process
            let origin = app.peek.as_ref().and_then(|p| p.origin.as_ref()).filter(|t| t.rep.as_ref().map(|r| (r.pid, r.create)) == Some(subject)).map(|t| (**t).clone());
            if let Some(t) = origin {
                app.peek = None;
                return Action::RowMenu(app.mx, app.my, Box::new(t));
            }
            if app.procs.sel.map(|s| (s.0, s.1)) != Some(subject) {
                return Action::None;
            }
            let snap = app.snap.clone();
            let Some(t) = app.procs.menu_target(&snap) else { return Action::None };
            app.peek = None;
            Action::RowMenu(app.mx, app.my, Box::new(t))
        }
        i if (OPEN..MEMBER).contains(&i) => {
            let rid = app.peek.as_ref().and_then(|p| p.open_ids.get(i - OPEN).copied());
            match rid {
                Some(rid) => open_rule(app, rid),
                None => Action::None,
            }
        }
        i if i >= MEMBER => {
            let snap = app.snap.clone();
            if let Some(p) = app.peek.as_mut() {
                // only a member that is still the same running process
                if let Some(&(pid, create, _)) = p.members.get(i - MEMBER) {
                    if snap.procs.iter().any(|q| q.pid == pid && q.create == create) {
                        p.member = Some((pid, create));
                        p.scroll = 0.0;
                    }
                }
            }
            Action::Repaint
        }
        _ => Action::None,
    }
}

// ---------- panel ----------

pub fn overlay(app: &mut App, g: &mut Gfx, body: Rect) {
    let Some(mut p) = app.peek.take() else { return };
    paint(app, g, body, &mut p);
    app.peek = Some(p);
}

fn text_btn(app: &mut App, g: &mut Gfx, vp: Rect, x: f32, y: f32, label: &str, id: usize, enabled: bool) -> f32 {
    let w = g.measure(label, f(12.0)) + 26.0;
    let r = Rect::new(x, y, w, 26.0);
    let hov = app.hover == Some(Hit::Peek(id)) && enabled;
    button(g, r, label, hov, enabled);
    if enabled {
        if let Some(v) = super::cpu::visible_rect(r, vp) {
            app.hits.push((v, Hit::Peek(id)));
        }
    }
    w
}

fn paint(app: &mut App, g: &mut Gfx, body: Rect, p: &mut Peek) {
    p.open_ids.clear();
    g.fill_a(body, BG, 0.72);
    app.hits.push((body, Hit::Peek(BACKDROP)));
    let w = (body.w - 40.0).clamp(300.0, 560.0);
    let h = (body.h - 40.0).clamp(180.0, 540.0);
    let r = Rect::new(body.x + ((body.w - w) / 2.0).max(0.0), body.y + ((body.h - h) / 2.0).max(0.0), w, h);
    panel(g, r);
    app.hits.push((r, Hit::Peek(PANEL_BG)));

    // title row
    let drilled = p.group && p.members.len() > 1 && p.member.is_some();
    let mut tx = r.x + 16.0;
    if drilled {
        let br = Rect::new(r.x + 10.0, r.y + 5.0, 28.0, 26.0);
        let hov = app.hover == Some(Hit::Peek(BACK));
        g.round(br, 6.0, if hov { PAN3 } else { PAN2 });
        g.text("\u{E72B}", br, Font { mono: false, ..f(11.0) }.icon().c(), TX1);
        app.hits.push((br, Hit::Peek(BACK)));
        tx = br.r() + 8.0;
    }
    let cr = Rect::new(r.r() - 36.0, r.y + 5.0, 28.0, 26.0);
    let chov = app.hover == Some(Hit::Peek(CLOSE));
    g.round(cr, 6.0, if chov { PAN3 } else { PAN });
    g.text("\u{E711}", cr, Font { mono: false, ..f(11.0) }.icon().c(), TX2);
    app.hits.push((cr, Hit::Peek(CLOSE)));
    g.text(&format!("Rules · {}", p.name), Rect::new(tx, r.y, (cr.x - tx - 8.0).max(0.0), 36.0), f(13.0).b(), TX1);
    g.fill(Rect::new(r.x + 1.0, r.y + 36.0, r.w - 2.0, 1.0), SEP);

    let vp = Rect::new(r.x + 1.0, r.y + 37.0, r.w - 2.0, r.h - 38.0);
    let x = r.x + 18.0;
    let iw = r.w - 36.0;
    let top = vp.y + 14.0 - p.scroll;
    let mut y = top;
    g.clip(vp);

    let Some(ft) = features::get() else {
        g.text("Rules engine not started.", Rect::new(x, y, iw, 20.0), f(12.0), TX2);
        g.unclip();
        return;
    };
    let view = ft.rules.view();
    let snap = app.snap.clone();
    let states = member_states(&view, &snap, &p.members);

    match p.subject() {
        None => {
            // a group: summary, then one row per member to inspect
            for l in super::cpu::wrap(g, &summary_line(&view, &states), iw, f(12.0), 3) {
                g.text(&l, Rect::new(x, y, iw, 18.0), f(12.0), TX1);
                y += 18.0;
            }
            y += 4.0;
            for l in super::cpu::wrap(g, "Rules match each process's own image, so members of one app can differ. Pick one to see its rules.", iw, f(11.5), 3) {
                g.text(&l, Rect::new(x, y, iw, 16.0), f(11.5), TX3);
                y += 16.0;
            }
            y += 8.0;
            for (i, m) in states.iter().enumerate() {
                let rr = Rect::new(r.x + 10.0, y, r.w - 20.0, 26.0);
                if rr.b() >= vp.y && rr.y <= vp.b() {
                    let hov = m.alive && app.hover == Some(Hit::Peek(MEMBER + i));
                    if hov {
                        g.round(rr, 5.0, PAN2);
                    }
                    let name_w = (rr.w - 190.0).max(60.0);
                    g.text(&m.name, Rect::new(rr.x + 8.0, rr.y, name_w * 0.55, rr.h), f(12.0), if m.alive { TX1 } else { TX4 });
                    g.text(&m.pid.to_string(), Rect::new(rr.x + 8.0 + name_w * 0.55, rr.y, 56.0, rr.h), f(11.0).m(), TX3);
                    let sx = rr.x + 8.0 + name_w * 0.55 + 60.0;
                    let sw = (rr.r() - sx - 64.0).max(0.0);
                    if !m.alive {
                        g.text("exited", Rect::new(sx, rr.y, sw, rr.h), f(11.5), TX4);
                    } else {
                        match m.mark {
                            Mark::Rule(id) | Mark::Off(id) => {
                                let off = matches!(m.mark, Mark::Off(_));
                                let pr = Rect::new(sx, rr.y + 5.0, if off { 52.0 } else { 36.0 }, 16.0);
                                pill(g, pr, if off { Look::Off } else { Look::Rule }, false, false);
                                g.text(view.get(id).map(|r| r.pattern.as_str()).unwrap_or(""), Rect::new(pr.r() + 8.0, rr.y, (sw - pr.w - 8.0).max(0.0), rr.h), f(11.5), TX3);
                            }
                            Mark::NoRule => g.text("no matching rule", Rect::new(sx, rr.y, sw, rr.h), f(11.5), TX4),
                            Mark::Unknown => g.text("image path not readable", Rect::new(sx, rr.y, sw, rr.h), f(11.5), TX4),
                        }
                        g.text("Inspect ›", Rect::new(rr.r() - 64.0, rr.y, 56.0, rr.h), f(11.5).r(), if hov { TX1 } else { TX3 });
                        if let Some(v) = super::cpu::visible_rect(rr, vp) {
                            app.hits.push((v, Hit::Peek(MEMBER + i)));
                        }
                    }
                }
                y += 28.0;
            }
        }
        Some((pid, create)) => {
            let m = states.iter().find(|m| m.pid == pid && m.create == create);
            let name = m.map(|m| m.name.as_str()).unwrap_or("");
            g.text(name, Rect::new(x, y, iw * 0.6, 20.0), f(13.0).b(), TX1);
            g.text(&format!("PID {pid}"), Rect::new(x, y, iw, 20.0), f(11.5).m().r(), TX3);
            y += 22.0;
            match m {
                Some(m) if m.alive => {
                    if let Some(path) = &m.path {
                        g.text(path, Rect::new(x, y, iw, 16.0), f(11.0).m(), TX3);
                        y += 22.0;
                    }
                    let matching = m.path.as_deref().map(|path| view.matching(path)).unwrap_or_default();
                    if m.path.is_none() {
                        for l in super::cpu::wrap(g, "This process's image path is not readable (protected or system process), so rules cannot be matched to it.", iw, f(12.0), 3) {
                            g.text(&l, Rect::new(x, y, iw, 18.0), f(12.0), TX2);
                            y += 18.0;
                        }
                    } else if matching.is_empty() {
                        g.text("No matching rule", Rect::new(x, y, iw, 20.0), f(13.0), TX1);
                        y += 22.0;
                        for l in super::cpu::wrap(g, "No saved rule, enabled or switched off, matches this image. Nothing is applied to it by Hayate.", iw, f(11.5), 3) {
                            g.text(&l, Rect::new(x, y, iw, 16.0), f(11.5), TX3);
                            y += 16.0;
                        }
                        y += 8.0;
                        let is_sel = app.procs.sel.map(|s| (s.0, s.1)) == Some((pid, create)) || p.origin.as_ref().and_then(|t| t.rep.as_ref()).is_some_and(|r| (r.pid, r.create) == (pid, create));
                        if is_sel {
                            text_btn(app, g, vp, x, y, "Create rule…", CREATE, true);
                            y += 34.0;
                        } else {
                            g.text(if p.origin.is_some() { "Right-click this process in the list to create a rule for it." } else { "Select this process in the list to create a rule for it." }, Rect::new(x, y, iw, 16.0), f(11.5), TX4);
                            y += 24.0;
                        }
                    } else {
                        for (i, mt) in matching.iter().enumerate() {
                            p.open_ids.push(mt.rule.id);
                            y = rule_card(app, g, vp, &view, mt, i, x, y, iw);
                        }
                    }
                }
                _ => {
                    g.text("This process has exited.", Rect::new(x, y, iw, 20.0), f(12.0), TX2);
                    y += 24.0;
                }
            }
            y += 6.0;
            for l in super::cpu::wrap(g, HONEST, iw, f(11.0), 5) {
                g.text(&l, Rect::new(x, y, iw, 15.0), f(11.0), TX4);
                y += 15.0;
            }
        }
    }
    g.unclip();
    let content = y - top + 28.0;
    let max = (content - vp.h).max(0.0);
    p.scroll = p.scroll.clamp(0.0, max);
    if max > 0.0 {
        let th = (vp.h * vp.h / content).max(12.0).min(vp.h);
        let ty = vp.y + (vp.h - th) * p.scroll / max;
        g.round(Rect::new(r.r() - 8.0, ty, 3.0, th), 1.5, LN);
    }
}

/// One matching rule: standing, pattern, configured vs last result, open button. Returns the next y.
#[allow(clippy::too_many_arguments)]
fn rule_card(app: &mut App, g: &mut Gfx, vp: Rect, view: &RuleView, mt: &crate::rules::view::Matching, i: usize, x: f32, mut y: f32, iw: f32) -> f32 {
    let (look, label, why) = match mt.standing {
        Standing::Effective => (Look::Rule, "In effect", String::from("The rule Hayate applies to this process.")),
        Standing::Shadowed { by } => (Look::Mixed, "Shadowed", format!("Enabled, but {} wins for this process (exact path, then exe name, then pattern; earlier first).", view.get(by).map(|r| r.pattern.as_str()).unwrap_or("another rule"))),
        Standing::Disabled => (Look::Off, "Disabled", String::from("Switched off: applies to nobody until it is enabled in Rules.")),
    };
    let top = y;
    let pw = g.measure(label, f(10.5)) + 18.0;
    let pr = Rect::new(x, y + 1.0, pw, 16.0);
    let (bg, ln, tx) = match look {
        Look::Rule => (Some(ACC_BG), ACC_LN, ACC_TX),
        Look::Mixed => (Some(PAN3), LN, TX2),
        Look::Off => (None, LN, TX3),
    };
    if let Some(bg) = bg {
        g.round(pr, 8.0, bg);
    }
    g.round_stroke(pr, 8.0, ln, 1.0);
    g.text(label, pr, f(10.5).c(), tx);
    g.text(&mt.rule.pattern, Rect::new(pr.r() + 10.0, y - 1.0, (iw - pw - 10.0).max(0.0), 20.0), f(12.0).m(), if mt.rule.enabled { TX1 } else { TX3 });
    y += 24.0;
    for l in super::cpu::wrap(g, &why, iw, f(11.5), 3) {
        g.text(&l, Rect::new(x, y, iw, 16.0), f(11.5), TX3);
        y += 16.0;
    }
    for (text, col) in [(configured_line(&mt.rule), TX2), (last_line(&mt.rule), TX3)] {
        for l in super::cpu::wrap(g, &text, iw, f(11.5), 3) {
            g.text(&l, Rect::new(x, y, iw, 16.0), f(11.5), col);
            y += 16.0;
        }
    }
    y += 6.0;
    text_btn(app, g, vp, x, y, "Open in Rules", OPEN + i, true);
    y += 26.0;
    g.fill(Rect::new(x, y + 8.0, iw, 1.0), SEP);
    let _ = top;
    y + 18.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Actions, Priority};

    fn rule(id: u32, pattern: &str, enabled: bool) -> Rule {
        let mut r = Rule::new(pattern, Actions { priority: Some(Priority::High), ..Default::default() });
        r.id = id;
        r.enabled = enabled;
        r
    }

    fn st(pid: u32, alive: bool, mark: Mark) -> MemberState {
        MemberState { pid, create: pid as i64, name: format!("p{pid}.exe"), alive, path: None, mark }
    }

    #[test]
    fn badge_never_exceeds_its_cell_and_never_shows_without_a_rule() {
        for gm in [GroupMark::Same(Mark::Rule(1)), GroupMark::Same(Mark::Off(1)), GroupMark::Mixed] {
            // the narrowest column Blink allows is 40 px wide: 24 px of cell after the 8 px padding
            for avail in [24.0f32, 30.0, 40.0, 54.0, 80.0, 400.0] {
                let (_, w, _) = badge_fit(gm, avail).unwrap_or_else(|| panic!("{gm:?} must fit in {avail}"));
                assert!(w <= avail, "{gm:?}: badge {w} in {avail}");
            }
            assert!(badge_fit(gm, 10.0).is_none(), "a cell too small for even the short form draws nothing");
        }
        // the default Status column is 70 px: the full "Rule" fits, "Rule off" fits
        assert_eq!(badge_fit(GroupMark::Same(Mark::Rule(1)), 54.0), Some((Look::Rule, 36.0, false)));
        assert_eq!(badge_fit(GroupMark::Same(Mark::Off(1)), 54.0), Some((Look::Off, 52.0, false)));
        for gm in [GroupMark::Same(Mark::NoRule), GroupMark::Same(Mark::Unknown), GroupMark::Plain] {
            assert_eq!(badge_fit(gm, 400.0), None);
        }
    }

    #[test]
    fn a_click_opens_the_exact_rule_only_when_there_is_one() {
        let exists = |id: u32| id == 7;
        assert_eq!(badge_click_for(GroupMark::Same(Mark::Rule(7)), exists), BadgeClick::OpenRule(7));
        assert_eq!(badge_click_for(GroupMark::Same(Mark::Off(7)), exists), BadgeClick::OpenRule(7), "a disabled rule is opened too, never enabled");
        assert_eq!(badge_click_for(GroupMark::Same(Mark::Rule(8)), exists), BadgeClick::Panel, "deleted meanwhile: the panel shows the truth");
        assert_eq!(badge_click_for(GroupMark::Mixed, exists), BadgeClick::Panel, "mixed members are inspected, not collapsed to the root's rule");
    }

    #[test]
    fn the_summary_says_mixed_and_counts_each_state() {
        let v = RuleView::new(vec![rule(1, "game.exe", true), rule(2, "x.exe", false)]);
        let same = [st(1, true, Mark::Rule(1)), st(2, true, Mark::Rule(1))];
        assert_eq!(summary_line(&v, &same), "All 2 running processes match game.exe.");
        let mixed = [st(1, true, Mark::Rule(1)), st(2, true, Mark::NoRule), st(3, true, Mark::Off(2)), st(4, true, Mark::Unknown), st(5, false, Mark::Unknown)];
        let s = summary_line(&v, &mixed);
        assert!(s.starts_with("Mixed: 1 with a rule, 1 with a switched-off rule, 1 without, 1 unreadable (of 4 running)."), "{s}");
        assert!(s.ends_with("1 exited since this opened."), "{s}");
        assert!(summary_line(&v, &[st(1, true, Mark::Off(2)), st(2, true, Mark::Off(2))]).contains("switched off"));
    }

    #[test]
    fn the_tip_keeps_configured_and_last_result_apart_and_never_claims_a_shared_rule_for_mixed() {
        let mut r = rule(1, "game.exe", true);
        r.last = Some(crate::rules::LastResult { time: 0, pid: 99, ok: false, protected: true, text: "priority refused".into() });
        let v = RuleView::new(vec![r, rule(2, "x.exe", false)]);
        let t = tip_paras(&v, &[Mark::Rule(1)]).unwrap();
        assert_eq!(t[0], "Rule: game.exe");
        assert!(t[1].starts_with("Enabled · Configured: High"), "{t:?}");
        assert!(t[2].starts_with("Last result (any process of this rule):") && t[2].contains("PID 99") && t[2].contains("not fully applied"), "{t:?}");
        let off = tip_paras(&v, &[Mark::Off(2)]).unwrap();
        assert_eq!(off[0], "Rule off: x.exe");
        assert!(off[1].contains("nothing is applied"), "{off:?}");
        assert!(off[3].contains("none yet"), "never applied is said, not implied");
        let mixed = tip_paras(&v, &[Mark::Rule(1), Mark::NoRule, Mark::NoRule]).unwrap();
        assert_eq!(mixed[0], "Mixed rules");
        assert!(mixed[1].starts_with("1 of 3 processes have a rule"), "{mixed:?}");
        assert!(!mixed.iter().any(|l| l.contains("game.exe")), "no shared rule is named for a mixed row");
        assert!(tip_paras(&v, &[Mark::NoRule]).is_none());
        assert!(tip_paras(&v, &[Mark::Rule(9)]).is_none(), "a rule that vanished is not described from memory");
    }

    #[test]
    fn member_states_follow_identity_not_pid() {
        use crate::sample::Proc;
        use crate::sample::Snapshot;
        let v = RuleView::new(vec![rule(1, "game.exe", true)]);
        let mk = |pid: u32, create: i64| Proc {
            pid,
            ppid: 0,
            name: "game.exe".into(),
            create,
            cpu: 0.0,
            mem: 1,
            io_bps: 0.0,
            threads: 1,
            handles: 1,
            session: 1,
            windowed: true,
            meta: std::sync::Arc::new(crate::sample::meta::Meta { path: Some(r"C:\g\game.exe".into()), ..Default::default() }),
            gpu: 0.0,
            gpu_engine: None,
        };
        let snap = Snapshot { procs: vec![mk(10, 100), mk(20, 999)], ..Default::default() };
        // pid 20 was opened as create 200, but a different process runs under pid 20 now: exited, not matched
        let s = member_states(&v, &snap, &[(10, 100, "game.exe".into()), (20, 200, "game.exe".into())]);
        assert_eq!((s[0].alive, s[0].mark), (true, Mark::Rule(1)));
        assert_eq!((s[1].alive, s[1].mark), (false, Mark::Unknown));
    }
}
