//! "Disallow running" panel: the explicit confirmation for an opt-in rule that makes
//! Blink END a program after it starts. Drawn over the tab body like the CPU panel (no
//! dialog, nothing before a click). Opened from the process menu (Processes + Vitals Top)
//! and from the Rules tab when a Disallow rule is switched on.
//!
//! What it says, on purpose: this is not a launch block. Windows still starts the
//! program; Blink ends it after it has started, only while Blink runs, and only a
//! program Blink may end. Opening the panel changes nothing. Success is only reported
//! after the disk write is acknowledged; a failed or unconfirmed (timed out) write offers
//! a real retry of the write, also when the change is already on in memory.
//!
//! The text area scrolls (mouse wheel) and the buttons are pinned below it, so every
//! limit and every button stays reachable in a narrow or short window.

use std::time::{Duration, Instant};

use super::cpu::{visible_rect, wrap};
use super::setup::accent;
use super::*;
use crate::features;
use crate::gfx::Font;
use crate::rules::disallow::{DisallowContext, DisallowMode};
use crate::rules::protect;

const CLOSE: usize = 0;
const BACKDROP: usize = 1;
const PANEL_BG: usize = 2;
const KEEP_OFF: usize = 3;
const CONFIRM: usize = 4;
const UNDO: usize = 5;
const SCOPE_NAME: usize = 6;
const SCOPE_PATH: usize = 7;
const RETRY: usize = 8;

/// A write the disk has not acknowledged by now is reported as unconfirmed (the write may
/// be stuck behind a hung disk); the panel never waits for it forever.
pub const SAVE_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    /// every program with this file name, wherever it sits
    Name,
    /// this one file only
    Path,
}

struct Pending {
    status: crate::rules::SaveStatus,
    /// shown when the disk acknowledges
    done: String,
    since: Instant,
}

pub struct DisallowPanel {
    /// opened on an existing rule (Rules tab): the pattern is fixed
    rule: Option<u32>,
    name: String,
    path: Option<String>,
    scope: Scope,
    /// why the confirm button is off (refused program, exited process, no engine)
    block: Option<String>,
    msg: Option<(bool, String)>,
    pending: Option<Pending>,
    /// the last write failed or was not confirmed: text to show once a retry succeeds
    retry: Option<String>,
    /// the rule this panel saved or found (undo target)
    saved: Option<u32>,
    /// the rule's enabled switch before this panel saved Disallow: undo puts it back
    undo_enabled: Option<bool>,
    /// what already exists for the chosen pattern
    ctx: DisallowContext,
    scroll: f32,
}

impl DisallowPanel {
    fn remember_save(&mut self, saved: crate::rules::disallow::DisallowSaved) {
        self.saved = Some(saved.id);
        self.undo_enabled = if saved.created { None } else { Some(saved.was_enabled) };
    }
    fn new(rule: Option<u32>) -> DisallowPanel {
        DisallowPanel { rule, name: String::new(), path: None, scope: Scope::Name, block: None, msg: None, pending: None, retry: None, saved: rule, undo_enabled: None, ctx: DisallowContext::default(), scroll: 0.0 }
    }

    fn pattern(&self) -> String {
        match (self.scope, &self.path) {
            (Scope::Path, Some(p)) => p.clone(),
            _ => self.name.clone(),
        }
    }
}

/// From a process menu: re-read the process through a handle verified by PID + creation
/// time and use ITS image, not the text the row showed. Anything unverifiable (exited, PID
/// reused, image unreadable, a protected program) leaves the confirm button off.
pub fn open_process(app: &mut App, pid: u32, create: i64) {
    super::setup::close(app);
    app.cpu = None;
    app.peek = None;
    let mut p = DisallowPanel::new(None);
    match verify(pid, create) {
        Ok(path) => {
            p.name = crate::osutil::file_name(&path).to_string();
            p.path = Some(path);
        }
        Err(why) => p.block = Some(why),
    }
    existing(&mut p);
    persistence_note(&mut p);
    app.disallow = Some(p);
}

/// From the Rules tab: switching a Disallow rule ON asks first.
pub fn open_rule(app: &mut App, id: u32) {
    let Some(r) = features::get().and_then(|f| f.rules.get(id)) else { return };
    super::setup::close(app);
    app.cpu = None;
    app.peek = None;
    let mut p = DisallowPanel::new(Some(id));
    p.name = r.pattern.clone();
    if let Some(why) = protect::pattern_refusal(&r.pattern) {
        p.block = Some(why.text());
    }
    existing(&mut p);
    p.saved = Some(id);
    persistence_note(&mut p);
    app.disallow = Some(p);
}

/// The live image of the process, or why it cannot be used.
fn verify(pid: u32, create: i64) -> Result<String, String> {
    if create == 0 {
        return Err("Hayate has no start time for this process, so it cannot tell it apart from another one. Nothing can be saved from it.".into());
    }
    let h = crate::osutil::open_verified(pid, create, windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION).map_err(|_| "The process has exited (or its PID was reused), so its program cannot be confirmed. Right-click a running copy.".to_string())?;
    let path = crate::osutil::image_of_handle(h.0).ok_or_else(|| "Windows would not tell Hayate which program file this is, so nothing can be saved from it.".to_string())?;
    protect::check(h.0, pid, Some(&path)).map_err(|why| format!("{} Disallow cannot be turned on for it.", why.text()))?;
    Ok(path)
}

/// Pick up what already exists for the chosen pattern (undo target, other settings, on/off).
fn existing(p: &mut DisallowPanel) {
    let Some(ft) = features::get() else { return };
    p.ctx = ft.rules.disallow_context(&p.pattern());
    if p.rule.is_none() {
        p.saved = p.ctx.id.filter(|_| p.ctx.flag);
    }
}

fn persistence_note(p: &mut DisallowPanel) {
    if let Some(status) = features::get().and_then(|f| f.rules.persistence()) {
        if let Some(Err(e)) = &*status.lock().unwrap() {
            p.msg = Some((false, format!("The last rules write failed: {e}. What you see may differ from disk until a save succeeds.")));
        }
    }
}

/// Turning Disallow on would change what an existing rule does: it has other settings (they stop
/// being applied while Disallow is on) or it is currently off (the whole rule goes on).
fn needs_whole_rule(c: &DisallowContext) -> bool {
    c.id.is_some() && !c.other.is_empty() && !(c.enabled && c.flag)
}

/// Pure: has the disk acknowledged the write? `Some(Ok)` / `Some(Err)` = final; a write that
/// is still unacknowledged after `SAVE_TIMEOUT` is a failure to confirm (never silently "saved").
pub fn poll_save(status: &crate::rules::SaveStatus, since: Instant, now: Instant) -> Option<Result<(), String>> {
    if let Some(r) = status.lock().unwrap().clone() {
        return Some(r);
    }
    (now.saturating_duration_since(since) >= SAVE_TIMEOUT).then(|| Err(format!("the disk did not confirm the write within {} s (it may be busy, unavailable or hung)", SAVE_TIMEOUT.as_secs())))
}

/// Copies named like the chosen pattern in the LAST list refresh. A count of what was seen,
/// not of what Blink will end.
fn running_matches(app: &App, p: &DisallowPanel) -> usize {
    let pat = p.pattern();
    let by_path = crate::rules::model::kind_of(&pat) == crate::rules::model::MatchKind::Path;
    app.snap
        .procs
        .iter()
        .filter(|q| if by_path { q.meta.path.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(&pat)) } else { q.name.eq_ignore_ascii_case(&pat) })
        .count()
}

/// The count sentence (pure, tested): a number of matching copies SEEN, never a promise.
pub fn seen_text(n: usize, pat: &str) -> String {
    if n == 0 {
        return "No copy was seen running in the last list refresh. Turning it on ends nothing until one starts (Hayate also checks the running programs once when you confirm).".to_string();
    }
    format!(
        "{n} cop{} named like {pat} {} seen running in the last list refresh. After you confirm, Hayate tries to end the ones it is allowed to; this is a count of matches, not a promise. Skipped or surviving: protected programs, ones needing administrator rights, other sessions' programs, ones covered by a more specific rule, and copies that start or exit meanwhile. Save it OFF to end nothing.",
        if n == 1 { "y" } else { "ies" },
        if n == 1 { "was" } else { "were" }
    )
}

const LIMITS: &str = "Unsaved work in a program Hayate ends is lost; the program gets no chance to save. This does not stop Windows from starting the program: it may appear for a moment before Hayate ends it. It only works while Hayate is running, and only for programs Hayate is allowed to end: an elevated program, or one in another user's session, needs Hayate run as administrator (access denied is shown in Rules). Windows system programs, services and Hayate itself are never ended. A program that restarts faster than 30 times in 10 s pauses its rule for 15 s; survivors are swept right after. Turn it off or remove it in Rules at any time; nothing is started again.";

pub fn overlay(app: &mut App, g: &mut Gfx, body: Rect) {
    let Some(mut p) = app.disallow.take() else { return };
    paint(app, g, body, &mut p);
    app.disallow = Some(p);
}

struct Line {
    t: String,
    font: Font,
    col: u32,
    h: f32,
}

struct Btn {
    id: usize,
    label: String,
    accent: bool,
    on: bool,
}

fn lines(g: &mut Gfx, out: &mut Vec<Line>, text: &str, w: f32, font: Font, col: u32, h: f32, gap: f32) {
    if text.is_empty() {
        return;
    }
    for t in wrap(g, text, w, font, 40) {
        out.push(Line { t, font, col, h });
    }
    if let Some(l) = out.last_mut() {
        l.h += gap;
    }
}

fn paint(app: &mut App, g: &mut Gfx, body: Rect, p: &mut DisallowPanel) {
    // disk acknowledgement (or the timeout) turns "saving…" into the final message
    if let Some(pd) = &p.pending {
        if let Some(result) = poll_save(&pd.status, pd.since, Instant::now()) {
            let done = pd.done.clone();
            p.pending = None;
            match result {
                Ok(()) => {
                    p.retry = None;
                    p.msg = Some((true, done));
                }
                Err(e) => {
                    p.msg = Some((false, format!("NOT confirmed on disk: {e}. The change works until Hayate closes, then it may be lost. Use Retry saving, or Undo.")));
                    p.retry = Some(done);
                }
            }
        }
    }
    let ft = features::get();
    let n_run = running_matches(app, p);
    let pat = p.pattern();
    let live = p.saved.and_then(|id| ft.and_then(|f| f.rules.get(id)));
    let is_on = live.as_ref().is_some_and(|r| r.blocks());
    let problem = ft.and_then(|f| f.rules.worker_problem());
    let whole = needs_whole_rule(&p.ctx);

    g.fill_a(body, BG, 0.72);
    app.hits.push((body, Hit::Disallow(BACKDROP)));
    let w = (body.w - 40.0).clamp(300.0, 560.0).min((body.w - 8.0).max(1.0));
    let iw = (w - 36.0).max(1.0);

    // ---- text blocks, measured first so the layout is known
    let mut c: Vec<Line> = Vec::new();
    let choose = p.rule.is_none() && p.path.is_some() && p.block.is_none();
    if let Some(b) = &p.block {
        lines(g, &mut c, b, iw, f(12.0), HOT_TX, 17.0, 0.0);
    }
    let what = if p.block.is_some() && p.name.is_empty() && p.rule.is_none() {
        String::new()
    } else if crate::rules::model::kind_of(&pat) == crate::rules::model::MatchKind::Path {
        format!("Hayate will end any process started from exactly this file: {pat}")
    } else {
        format!("Hayate will end every process named {pat}, wherever the file is, and every copy that starts later.")
    };
    lines(g, &mut c, &what, iw, f(12.0), TX1, 17.0, 4.0);
    if let Some(why) = &problem {
        lines(g, &mut c, &format!("Disallow cannot run: {why}. Nothing is ended, so it cannot be turned on."), iw, f(12.0).b(), HOT_TX, 17.0, 4.0);
    }
    if p.ctx.id.is_some() && !p.ctx.other.is_empty() && p.block.is_none() {
        let off = if p.ctx.enabled { "" } else { " That rule is currently OFF: turning Disallow on turns the WHOLE rule on." };
        lines(g, &mut c, &format!("A saved rule for {pat} already has: {}. While Disallow is on those settings are NOT applied (Disallow wins); they stay saved.{off}", p.ctx.other), iw, f(12.0), WARN_TX, 17.0, 4.0);
    }
    if p.rule.is_none() && p.ctx.id.is_some() && p.ctx.enabled && !p.ctx.flag {
        lines(g, &mut c, "That rule is ON, so a Disallow that is saved but OFF cannot live inside it: Save, keep OFF is unavailable and the rule keeps doing what it does now. Turn that rule off in Rules first, or choose Disallow.", iw, f(11.5), TX3, 16.0, 4.0);
    }
    let now = if p.block.is_some() && p.rule.is_none() {
        String::new()
    } else if p.rule.is_some() && live.is_none() {
        "Disallow is off. Nothing is ended.".to_string()
    } else if is_on {
        "This rule is ON.".to_string()
    } else {
        seen_text(n_run, &pat)
    };
    lines(g, &mut c, &now, iw, f(12.0).b(), if is_on || n_run > 0 { WARN_TX } else { TX2 }, 17.0, 8.0);
    lines(g, &mut c, LIMITS, iw, f(11.0), TX3, 15.0, 0.0);
    let scope_h = if choose { 2.0 * 26.0 + 8.0 } else { 0.0 };
    let content_h = scope_h + c.iter().map(|l| l.h).sum::<f32>();

    // ---- footer: message lines + wrapped button rows
    let mut foot: Vec<Line> = Vec::new();
    if let Some((ok, t)) = &p.msg {
        lines(g, &mut foot, t, iw, f(11.5), if *ok { OK_TX } else { HOT_TX }, 16.0, 0.0);
    }
    let saving = p.pending.is_some();
    let can = ft.is_some() && p.block.is_none() && !saving;
    let gone = p.rule.is_some() && live.is_none();
    let confirm_label = if gone {
        "Not disallowed".to_string()
    } else if is_on {
        "Already on".to_string()
    } else if whole {
        "Turn the whole rule on + Disallow".to_string()
    } else if n_run > 0 {
        format!("Disallow, try to end {n_run} seen now")
    } else {
        "Disallow future starts".to_string()
    };
    let mut btns: Vec<Btn> = Vec::new();
    if p.saved.is_some() && live.is_some() && !saving {
        btns.push(Btn { id: UNDO, label: "Allow running again".into(), accent: false, on: true });
    }
    if p.retry.is_some() && !saving && ft.is_some() {
        btns.push(Btn { id: RETRY, label: "Retry saving".into(), accent: false, on: true });
    }
    if p.rule.is_none() {
        let on = can && !is_on && !(p.ctx.id.is_some() && p.ctx.enabled);
        btns.push(Btn { id: KEEP_OFF, label: "Save, keep OFF".into(), accent: false, on });
    }
    btns.push(Btn { id: CONFIRM, label: confirm_label, accent: true, on: can && !is_on && !gone && problem.is_none() });
    let bw = |g: &mut Gfx, s: &str| (g.measure(s, f(12.0).b()) + 28.0).min(iw);
    // flow into rows
    let mut rows: Vec<Vec<(usize, f32)>> = vec![Vec::new()];
    let mut used = 0.0f32;
    for (i, b) in btns.iter().enumerate() {
        let wd = bw(g, &b.label);
        if used > 0.0 && used + wd > iw {
            rows.push(Vec::new());
            used = 0.0;
        }
        rows.last_mut().unwrap().push((i, wd));
        used += wd + 8.0;
    }
    let foot_h = foot.iter().map(|l| l.h).sum::<f32>() + if foot.is_empty() { 0.0 } else { 8.0 } + rows.len() as f32 * 36.0;

    let head_h = 14.0 + 28.0 + 24.0;
    let want = head_h + content_h + 8.0 + foot_h + 12.0;
    let max_h = (body.h - 16.0).max(1.0);
    let h = want.min(max_h);
    let vp_h = (h - head_h - 8.0 - foot_h - 12.0).max(0.0);
    let max_scroll = (content_h - vp_h).max(0.0);
    p.scroll = p.scroll.clamp(0.0, max_scroll);
    let r = Rect::new(body.x + (body.w - w) / 2.0, body.y + ((body.h - h) / 2.0).clamp(8.0, 60.0), w, h);
    g.round(r, 8.0, PAN);
    g.round_stroke(r, 8.0, LN, 1.0);
    app.hits.push((r, Hit::Disallow(PANEL_BG)));
    let x = r.x + 18.0;
    let mut y = r.y + 14.0;

    // ---- title + close
    g.text("Disallow running", Rect::new(x, y, iw - 70.0, 26.0), f(15.0).b(), TX1);
    let cr = Rect::new(r.r() - 18.0 - 62.0, y, 62.0, 26.0);
    button(g, cr, "Close", app.hover == Some(Hit::Disallow(CLOSE)), true);
    app.hits.push((cr, Hit::Disallow(CLOSE)));
    y += 28.0;
    let subtitle = if p.name.is_empty() { "No program".to_string() } else { p.name.clone() };
    g.text(&subtitle, Rect::new(x, y, iw, 20.0), f(12.5).b(), TX2);
    y += 24.0;

    // ---- scrolling text area
    let vp = Rect::new(r.x + 1.0, y, r.w - 2.0, vp_h);
    let hit_start = app.hits.len();
    g.clip(vp);
    let mut cy = y - p.scroll;
    if choose {
        let path = p.path.clone().unwrap_or_default();
        for (id, scope, label) in [(SCOPE_NAME, Scope::Name, format!("Every copy of {} (any folder)", p.name)), (SCOPE_PATH, Scope::Path, format!("Only this file: {path}"))] {
            let row = Rect::new(x, cy, iw, 24.0);
            let on = p.scope == scope;
            if app.hover == Some(Hit::Disallow(id)) {
                g.round(row, 5.0, PAN2);
            }
            let dot = Rect::new(row.x + 4.0, row.y + 5.0, 14.0, 14.0);
            g.round_stroke(dot, 7.0, if on { ACC } else { LN }, 1.5);
            if on {
                g.circle(dot.x + 7.0, dot.y + 7.0, 4.0, ACC);
            }
            g.text(&label, Rect::new(row.x + 26.0, row.y, row.w - 28.0, row.h), f(12.0), if on { TX1 } else { TX2 });
            app.hits.push((row, Hit::Disallow(id)));
            cy += 26.0;
        }
        cy += 8.0;
    }
    for l in &c {
        g.text(&l.t, Rect::new(x, cy, iw, l.h.max(15.0)), l.font, l.col);
        cy += l.h;
    }
    g.unclip();
    let mut i = hit_start;
    while i < app.hits.len() {
        if let Some(v) = visible_rect(app.hits[i].0, vp) {
            app.hits[i].0 = v;
            i += 1;
        } else {
            app.hits.remove(i);
        }
    }
    if max_scroll > 0.0 {
        let th = (vp.h * vp.h / content_h).max(12.0).min(vp.h);
        let ty = vp.y + (vp.h - th) * p.scroll / max_scroll;
        g.round(Rect::new(r.r() - 8.0, ty, 3.0, th), 1.5, LN);
    }
    y += vp_h + 8.0;

    // ---- pinned footer
    for l in &foot {
        g.text(&l.t, Rect::new(x, y, iw, l.h), l.font, l.col);
        y += l.h;
    }
    if !foot.is_empty() {
        y += 8.0;
    }
    for row in &rows {
        let mut bx = x;
        for &(i, wd) in row {
            let b = &btns[i];
            let br = Rect::new(bx, y, wd, 30.0);
            if b.on {
                if b.accent {
                    accent(g, br, &b.label, app.hover == Some(Hit::Disallow(b.id)), true);
                } else {
                    button(g, br, &b.label, app.hover == Some(Hit::Disallow(b.id)), true);
                }
                app.hits.push((br, Hit::Disallow(b.id)));
            } else {
                button(g, br, &b.label, false, false);
            }
            bx += wd + 8.0;
        }
        y += 36.0;
    }
}

pub fn close(app: &mut App) {
    app.disallow = None;
}

pub fn wheel(app: &mut App, delta: f32) -> bool {
    match app.disallow.as_mut() {
        Some(p) => {
            p.scroll = (p.scroll - delta * 0.5).max(0.0);
            true
        }
        None => false,
    }
}

fn start_pending(p: &mut DisallowPanel, status: Option<crate::rules::SaveStatus>, done: String) {
    match status {
        Some(status) => {
            p.pending = Some(Pending { status, done, since: Instant::now() });
            p.retry = None;
            p.msg = Some((true, "Saving rule to disk…".into()));
        }
        None => p.msg = Some((false, "Rules engine has no file to save to: the change lasts until Hayate closes.".into())),
    }
}

pub fn click(app: &mut App, h: Hit) -> Action {
    let Hit::Disallow(id) = h else { return Action::None };
    let Some(p) = app.disallow.as_mut() else { return Action::None };
    match id {
        CLOSE | BACKDROP => app.disallow = None,
        PANEL_BG => return Action::None,
        SCOPE_NAME | SCOPE_PATH => {
            p.scope = if id == SCOPE_PATH { Scope::Path } else { Scope::Name };
            p.msg = None;
            p.scroll = 0.0;
            existing(p);
        }
        CONFIRM | KEEP_OFF => {
            if p.pending.is_some() || p.block.is_some() {
                return Action::None;
            }
            let Some(ft) = features::get() else { return Action::None };
            let enable = id == CONFIRM;
            let pat = p.pattern();
            existing(p);
            let whole = needs_whole_rule(&p.ctx);
            let res = match p.rule {
                Some(rid) => {
                    if let (true, Some(why)) = (enable, ft.rules.worker_problem()) {
                        Err(format!("Disallow cannot run: {why}"))
                    } else if ft.rules.set_enabled(rid, true) {
                        Ok(crate::rules::disallow::DisallowSaved { id: rid, created: false, was_enabled: p.ctx.enabled })
                    } else {
                        Err("That rule no longer exists.".to_string())
                    }
                }
                None => ft.rules.set_disallow(&pat, if enable { DisallowMode::Enable { confirm_whole_rule: whole } } else { DisallowMode::KeepOff }),
            };
            match res {
                Ok(saved) => {
                    // undo puts back the switch the rule had before, never leaves other settings quietly on
                    p.remember_save(saved);
                    let done = if enable {
                        format!("Saved to disk: {pat} is disallowed. Hayate ends it after each start while Hayate runs. The result of each attempt shows in Rules.")
                    } else {
                        format!("Saved to disk, OFF: {pat} is not ended. Switch it on in Rules (you will be asked again).")
                    };
                    start_pending(p, ft.rules.persistence(), done);
                    existing(p);
                    p.saved = Some(saved.id);
                }
                Err(e) => p.msg = Some((false, e)),
            }
        }
        RETRY => {
            let (Some(ft), Some(done)) = (features::get(), p.retry.clone()) else { return Action::None };
            if p.pending.is_some() {
                return Action::None;
            }
            // a real write of the current rules, not a re-send of the same in-memory change
            let status = ft.rules.save_again();
            start_pending(p, Some(status), done);
        }
        UNDO => {
            let (Some(ft), Some(rid)) = (features::get(), p.saved) else { return Action::None };
            let before = ft.rules.get(rid);
            let pat = before.as_ref().map(|r| r.pattern.clone()).unwrap_or_else(|| p.pattern());
            if ft.rules.clear_disallow(rid, p.undo_enabled) {
                let left = ft.rules.get(rid);
                let done = match left {
                    Some(r) => format!("Saved to disk: {pat} may run again. Nothing more is ended; programs already ended stay ended. Its other saved settings remain, and the rule is {}.", if r.enabled { "ON, so they apply again" } else { "OFF, so none of them apply" }),
                    None => format!("Saved to disk: {pat} may run again. Nothing more is ended; programs already ended stay ended."),
                };
                start_pending(p, ft.rules.persistence(), done);
                p.saved = None;
                p.undo_enabled = None;
                existing(p);
            } else {
                p.msg = Some((false, "That rule no longer exists.".into()));
                p.saved = None;
            }
        }
        _ => return Action::None,
    }
    Action::Repaint
}

/// The one-line truth shown in Rules for a Disallow rule. `broken`: the worker that ends
/// programs is not running, so an enabled rule is NOT being enforced.
pub fn rule_state(r: &crate::rules::Rule, broken: bool) -> &'static str {
    if !r.actions.disallow {
        ""
    } else if r.blocks() && broken {
        "ON but NOT ENFORCED · worker not running"
    } else if r.blocks() {
        "ON · ends it after it starts"
    } else if r.kind() == crate::rules::model::MatchKind::Glob {
        "ignored (patterns cannot be disallowed)"
    } else {
        "OFF · nothing is ended"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_opened_panel_preserves_the_pre_enable_switch_for_undo() {
        let mut p = DisallowPanel::new(Some(7));
        p.remember_save(crate::rules::disallow::DisallowSaved { id: 7, created: false, was_enabled: false });
        assert_eq!(p.undo_enabled, Some(false));
        assert_eq!(p.saved, Some(7));
        p.remember_save(crate::rules::disallow::DisallowSaved { id: 7, created: false, was_enabled: true });
        assert_eq!(p.undo_enabled, Some(true));
    }
    use std::sync::{Arc, Mutex};

    #[test]
    fn seen_count_is_a_label_not_a_promise() {
        let none = seen_text(0, "a.exe");
        assert!(none.contains("No copy was seen") && none.contains("ends nothing"), "{none}");
        let one = seen_text(1, "a.exe");
        assert!(one.contains("1 copy") && one.contains("was seen"), "{one}");
        let many = seen_text(7, "a.exe");
        assert!(many.contains("7 copies") && many.contains("were seen"), "{many}");
        for t in [&one, &many] {
            assert!(!t.contains("will be ended"), "no promise: {t}");
            for q in ["not a promise", "protected programs", "administrator", "other sessions", "more specific rule", "start or exit meanwhile"] {
                assert!(t.contains(q), "{q} missing in {t}");
            }
        }
    }

    #[test]
    fn limits_text_names_every_limit_including_unsaved_work() {
        for q in ["Unsaved work", "does not stop Windows from starting", "only works while Hayate is running", "administrator", "never ended", "30 times in 10 s", "15 s", "swept"] {
            assert!(LIMITS.contains(q), "{q}");
        }
        // unsaved work comes FIRST: a clipped text can never lose it
        assert!(LIMITS.starts_with("Unsaved work"));
    }

    #[test]
    fn an_unacknowledged_write_times_out_instead_of_locking_the_panel() {
        let status: crate::rules::SaveStatus = Arc::new(Mutex::new(None));
        let t0 = Instant::now();
        assert_eq!(poll_save(&status, t0, t0), None, "still waiting");
        assert_eq!(poll_save(&status, t0, t0 + SAVE_TIMEOUT - Duration::from_millis(1)), None);
        let late = poll_save(&status, t0, t0 + SAVE_TIMEOUT).expect("timed out");
        assert!(late.unwrap_err().contains("did not confirm"), "a timeout is a failure to confirm, never 'saved'");
        // an acknowledged write wins whatever the clock says
        *status.lock().unwrap() = Some(Ok(()));
        assert_eq!(poll_save(&status, t0, t0 + SAVE_TIMEOUT * 3), Some(Ok(())));
        *status.lock().unwrap() = Some(Err("disk full".into()));
        assert_eq!(poll_save(&status, t0, t0), Some(Err("disk full".into())));
    }

    #[test]
    fn whole_rule_confirmation_is_needed_only_when_the_rule_would_change() {
        let c = |id, enabled, flag, other: &str| DisallowContext { id, enabled, flag, other: other.into() };
        assert!(!needs_whole_rule(&c(None, false, false, "")), "new rule");
        assert!(!needs_whole_rule(&c(Some(1), false, true, "")), "a saved Disallow-only rule");
        assert!(needs_whole_rule(&c(Some(1), false, false, "High")), "off rule with settings");
        assert!(needs_whole_rule(&c(Some(1), true, false, "High")), "on rule: its settings stop applying");
        assert!(needs_whole_rule(&c(Some(1), false, true, "High")), "off Disallow+settings rule: whole rule goes on");
        assert!(!needs_whole_rule(&c(Some(1), true, true, "High")), "already blocking: nothing changes");
    }

    #[test]
    fn rule_state_never_calls_an_unenforced_rule_on() {
        let on = crate::rules::Rule::new("a.exe", crate::rules::Actions { disallow: true, ..Default::default() });
        assert_eq!(rule_state(&on, false), "ON · ends it after it starts");
        assert!(rule_state(&on, true).contains("NOT ENFORCED"));
        let off = crate::rules::Rule { enabled: false, ..on };
        assert_eq!(rule_state(&off, true), "OFF · nothing is ended");
    }
}
