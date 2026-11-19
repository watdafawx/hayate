//! Service properties: opened from a service's right-click menu, drawn over the
//! tab body like the CPU panel (nothing modal, nothing before a click).
//!
//! This is Blink's own panel, not the native Windows one. Windows has no
//! documented way to open the Services console on one service or on its
//! Properties page: mmc.exe takes only a console file, /a, /32 and /64
//! (learn.microsoft.com/windows-server/administration/windows-commands/mmc).
//! So the panel shows what the SCM reports and changes the startup type, and a
//! clearly named button opens the full Windows Services list.
//!
//! Opening reads the service (query rights only). Nothing is written until the
//! user picks a type and presses Apply; the write goes through
//! `svcctl::apply_startup` (read, identity, "still what you saw", write, read
//! back).

use super::cpu::wrap;
use super::setup::accent;
use super::*;
use crate::svcctl::{self, Config, ScmError, StartKind, StartupDone, Target};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A read / write that Windows has not answered after this long stops being waited on by the panel
/// (the worker and its busy guard live on; nothing already sent is cancelled or undone).
const READ_WAIT: Duration = Duration::from_secs(20);
const APPLY_WAIT: Duration = Duration::from_secs(60);

const CLOSE: usize = 0;
const APPLY: usize = 1;
const OPEN_SCM: usize = 2;
const COPY: usize = 3;
const BACKDROP: usize = 4;
const PANEL_BG: usize = 5;
/// startup type i = KIND + i
const KIND: usize = 10;

type Slot<T> = Arc<Mutex<Option<T>>>;

pub struct Props {
    t: Target,
    /// the last good read
    cfg: Option<Config>,
    /// why there is none
    err: Option<String>,
    sel: Option<StartKind>,
    /// last result: (ok, text)
    msg: Option<(bool, String)>,
    read: Option<Slot<Result<Config, ScmError>>>,
    apply: Option<Slot<Result<StartupDone, String>>>,
    /// after a failed Apply the choice goes back to what the service really has
    reset_sel: bool,
    scroll: f32,
    /// this service had work in flight at the last paint (survives closing the panel)
    was_busy: bool,
    read_at: Option<Instant>,
    apply_at: Option<Instant>,
}

/// Open the panel for one captured service and read it. Writes nothing.
pub fn open(app: &mut App, t: Target) {
    super::setup::close(app);
    super::cpu::close(app);
    let mut p = Props { t, cfg: None, err: None, sel: None, msg: None, read: None, apply: None, reset_sel: true, scroll: 0.0, was_busy: false, read_at: None, apply_at: None };
    reload(&mut p);
    app.svcprops = Some(p);
}

fn reload(p: &mut Props) {
    let slot: Slot<Result<Config, ScmError>> = Arc::new(Mutex::new(None));
    p.read = Some(slot.clone());
    p.read_at = Some(Instant::now());
    svcctl::spawn_read(p.t.name.clone(), move |r| {
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        super::icons::poke();
    });
}

/// Close the panel and let go of everything it registered. A startup change already sent keeps
/// running (a committed write cannot be taken back by closing a window): its result goes to the
/// Services header, and the busy registry keeps the service locked until it ends.
pub fn close(app: &mut App) {
    if let Some(p) = app.svcprops.take() {
        if p.apply.is_some() {
            crate::ui::services::report(format!("{}: the startup change is still being applied; the result will show here", p.t.name));
        }
    }
    app.hits.retain(|(_, h)| !matches!(h, Hit::SvcProp(_)));
}

/// The panel belongs to the Services tab: on any other tab it is gone (and so are its hit areas).
fn on_services(app: &mut App) -> bool {
    if app.svcprops.is_some() && app.tab != Tab::Services {
        close(app);
    }
    app.svcprops.is_some()
}

pub fn wheel(app: &mut App, delta: f32) -> bool {
    if !on_services(app) {
        return false;
    }
    match app.svcprops.as_mut() {
        Some(p) => {
            p.scroll -= delta * 0.5;
            true
        }
        None => false,
    }
}

pub fn overlay(app: &mut App, g: &mut Gfx, body: Rect) {
    if !on_services(app) {
        return;
    }
    let Some(mut p) = app.svcprops.take() else { return };
    paint(app, g, body, &mut p);
    app.svcprops = Some(p);
}

/// Take finished background results. Pure state, no drawing.
fn pump(p: &mut Props) {
    // Work in flight on this service lives in the shared registry, not in this panel: it survives
    // closing and reopening it. When it ends (here or in a panel opened earlier) read the service again.
    let busy = svcctl::busy_work(&p.t.key()).is_some();
    if p.was_busy && !busy && p.apply.is_none() {
        p.reset_sel = true;
        reload(p);
    }
    p.was_busy = busy;
    // a reply that never comes ends in words, not a permanent "Reading…" / spinner
    if p.read.is_some() && p.read_at.is_some_and(|t| t.elapsed() > READ_WAIT) {
        p.read = None;
        p.cfg = None;
        p.err = Some(format!("Windows has not answered the read of {} after {} s. Close and reopen this panel to try again.", p.t.name, READ_WAIT.as_secs()));
    }
    if p.apply.is_some() && p.apply_at.is_some_and(|t| t.elapsed() > APPLY_WAIT) {
        // the write may still land; Blink does not claim otherwise and does not cancel it
        p.apply = None;
        p.msg = Some((false, format!("Windows has not answered after {} s. The change may still complete; Hayate does not cancel it. Reading the service again.", APPLY_WAIT.as_secs())));
        p.reset_sel = true;
        reload(p);
    }
    if let Some(slot) = p.read.clone() {
        let got = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(r) = got {
            p.read = None;
            match r {
                // without a recorded image path there is no baseline: shown read-only, never changed
                Ok(c) if svcctl::same_service(&p.t, &c) || (p.t.path.is_empty() && svcctl::same_name(&p.t, &c)) => {
                    if p.reset_sel || p.sel.is_none() {
                        p.sel = c.kind();
                        p.reset_sel = false;
                    }
                    p.cfg = Some(c);
                    p.err = None;
                }
                Ok(c) => {
                    p.cfg = None;
                    p.err = Some(format!("{} is no longer the service you opened (its image is now {}). Close this and reopen it from the list.", p.t.name, c.path));
                }
                Err(e) => {
                    p.cfg = None;
                    p.err = Some(format!("Could not read {}: {}", p.t.name, e.message()));
                }
            }
        }
    }
    if let Some(slot) = p.apply.clone() {
        let got = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(r) = got {
            p.apply = None;
            crate::ui::services::bump();
            match r {
                Ok(d) => {
                    p.sel = d.after.kind();
                    p.cfg = Some(d.after);
                    p.msg = Some((true, capitalize(&d.note)));
                }
                Err(e) => {
                    p.msg = Some((false, capitalize(&e)));
                    p.reset_sel = true;
                    reload(p);
                }
            }
        }
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Can Apply run now: a good fresh read (the delayed flag included), a changeable
/// current type, a different pick, and nothing in flight on this service (here or in a
/// panel opened earlier: the busy registry is process-wide).
fn can_apply(p: &Props) -> bool {
    match (&p.cfg, p.sel) {
        (Some(c), Some(sel)) => p.t.baseline_problem().is_none() && !crate::ui::services::is_protected(&p.t) && p.apply.is_none() && p.read.is_none() && c.changeable() && c.kind().is_some_and(|k| k != sel) && svcctl::busy_work(&p.t.key()).is_none(),
        _ => false,
    }
}

fn paint(app: &mut App, g: &mut Gfx, body: Rect, p: &mut Props) {
    pump(p);
    g.fill_a(body, BG, 0.72);
    app.hits.push((body, Hit::SvcProp(BACKDROP)));

    let w = (body.w - 40.0).clamp(300.0, 560.0);
    let iw = w - 36.0;
    let label_w = 96.0;
    let vw = iw - label_w;

    let c = p.cfg.clone();
    let display = c.as_ref().map(|c| c.display.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| if p.t.display.is_empty() { p.t.name.clone() } else { p.t.display.clone() });

    // ---- text blocks, measured first so the height is known
    let desc = c.as_ref().map(|c| c.description.clone()).unwrap_or_default();
    let desc_lines = if desc.is_empty() { Vec::new() } else { wrap(g, &desc, vw, f(12.0), 4) };
    let path_lines = c.as_ref().map(|c| wrap(g, &c.binary, vw, f(11.5).m(), 3)).unwrap_or_default();
    let dep_text = c.as_ref().map(|c| if c.dependencies.is_empty() { "none".to_string() } else { c.dependencies.join(", ") }).unwrap_or_default();
    let dep_lines = if dep_text.is_empty() { Vec::new() } else { wrap(g, &dep_text, vw, f(12.0), 2) };
    let err_lines = p.err.as_ref().map(|e| wrap(g, e, iw, f(12.0), 5)).unwrap_or_default();
    let elevated = crate::osutil::is_elevated();

    // notes under the type list: what the choice means
    let mut notes: Vec<(String, u32)> = Vec::new();
    if let (Some(c), Some(sel)) = (&c, p.sel) {
        if c.start == 2 && c.delayed.is_none() {
            notes.push(("Windows did not report the delayed-start setting, so Hayate cannot be sure of the current startup type and will not change it. Close and reopen this panel to read it again.".into(), WARN_TX));
        } else if c.kind().is_none() {
            notes.push(("This is a driver start type (Boot / System). Hayate does not change it.".into(), WARN_TX));
        }
        if sel == StartKind::Disabled && matches!(c.status.state, svcctl::st::RUNNING | svcctl::st::START_PENDING | svcctl::st::PAUSED) {
            notes.push(("Disabling does not stop it: it keeps running until you Stop it (right-click the service) or restart Windows.".into(), TX2));
        }
        if let Some(t) = c.trigger_note() {
            notes.push((t.into(), TX3));
        }
        if c.delayed == Some(true) && c.start != 2 {
            notes.push(("Its delayed-start flag is set, but it has no effect until the type is Automatic.".into(), TX3));
        }
    }
    if let Some(why) = p.t.baseline_problem() {
        if c.is_some() {
            notes.push((format!("{why}. This panel is read-only for it."), WARN_TX));
        }
    }
    if crate::ui::services::is_protected(&p.t) {
        notes.push(("Windows protects this service: it refuses startup changes, even from an administrator. Apply is off.".into(), WARN_TX));
    }
    if let Some(w) = svcctl::busy_work(&p.t.key()) {
        notes.push((format!("This service is {} right now. Apply is off until that finishes.", w.progress()), WARN_TX));
    }
    if !elevated && c.is_some() {
        notes.push(("Windows service permissions determine whether this change is allowed; restart as administrator if access is denied.".into(), TX3));
    }
    let note_lines: Vec<(Vec<String>, u32)> = notes.iter().map(|(t, col)| (wrap(g, t, iw, f(11.5), 3), *col)).collect();
    let msg_lines = p.msg.as_ref().map(|(_, t)| wrap(g, t, iw, f(12.0), 4)).unwrap_or_default();
    let limit = "Windows has no way to open its own Properties page for one service from another program. The button opens the full Services list; find this service there.";
    let limit_lines = wrap(g, limit, iw, f(11.0), 3);

    let row_h = |n: usize| (n.max(1) as f32) * 16.0 + 6.0;
    let facts_h = 22.0 // key
        + if desc_lines.is_empty() { 0.0 } else { row_h(desc_lines.len()) }
        + row_h(path_lines.len().max(1))
        + 22.0 // account
        + 22.0 // status
        + row_h(dep_lines.len().max(1));
    let kinds_h = 4.0 * 30.0 + 26.0;
    let notes_h: f32 = note_lines.iter().map(|(l, _)| l.len() as f32 * 15.0 + 4.0).sum();
    let msg_h = if msg_lines.is_empty() { 0.0 } else { msg_lines.len() as f32 * 16.0 + 8.0 };
    let err_h = err_lines.len() as f32 * 16.0 + if err_lines.is_empty() { 0.0 } else { 8.0 };
    let want = 14.0 + 28.0 + 26.0 + 22.0 + facts_h + 10.0 + if p.cfg.is_some() { kinds_h + notes_h } else { 30.0 } + err_h + msg_h + 44.0 + limit_lines.len() as f32 * 15.0 + 16.0;
    let max_h = (body.h - 24.0).max(1.0);
    let h = want.min(max_h);
    let max_scroll = (want - h).max(0.0);
    p.scroll = p.scroll.clamp(0.0, max_scroll);

    let r = Rect::new(body.x + (body.w - w) / 2.0, body.y + ((body.h - h) / 2.0).clamp(12.0, 60.0), w, h);
    g.round(r, 8.0, PAN);
    g.round_stroke(r, 8.0, LN, 1.0);
    app.hits.push((r, Hit::SvcProp(PANEL_BG)));
    let x = r.x + 18.0;
    let mut y = r.y + 14.0;

    // ---- title + close (fixed)
    g.text("Service properties", Rect::new(x, y, iw - 70.0, 26.0), f(15.0).b(), TX1);
    let cr = Rect::new(r.r() - 18.0 - 62.0, y, 62.0, 26.0);
    button(g, cr, "Close", app.hover == Some(Hit::SvcProp(CLOSE)), true);
    app.hits.push((cr, Hit::SvcProp(CLOSE)));
    y += 30.0;
    let vp = Rect::new(r.x, y, r.w, (r.b() - y - 6.0).max(0.0));
    let hit_start = app.hits.len();
    g.clip(vp);
    y -= p.scroll;

    g.text(&display, Rect::new(x, y, iw, 22.0), f(14.0).b(), TX1);
    y += 26.0;

    let fact = |g: &mut Gfx, y: &mut f32, label: &str, v: &str, font: Font, col: u32| {
        g.text(label, Rect::new(x, *y, label_w - 6.0, 18.0), f(11.5), TX3);
        g.text(v, Rect::new(x + label_w, *y, vw, 18.0), font, col);
        *y += 22.0;
    };
    fact(g, &mut y, "Service name", &p.t.name, f(12.0).m(), TX1);
    if !desc_lines.is_empty() {
        g.text("Description", Rect::new(x, y, label_w - 6.0, 18.0), f(11.5), TX3);
        for (i, l) in desc_lines.iter().enumerate() {
            g.text(l, Rect::new(x + label_w, y + i as f32 * 16.0, vw, 16.0), f(12.0), TX2);
        }
        y += row_h(desc_lines.len());
    }
    match &c {
        Some(c) => {
            g.text("Program", Rect::new(x, y, label_w - 6.0, 18.0), f(11.5), TX3);
            for (i, l) in path_lines.iter().enumerate() {
                g.text(l, Rect::new(x + label_w, y + i as f32 * 16.0, vw, 16.0), f(11.5).m(), TX2);
            }
            y += row_h(path_lines.len().max(1));
            fact(g, &mut y, "Runs as", if c.account.is_empty() { "unknown" } else { &c.account }, f(12.0), TX2);
            let st = if c.status.pid != 0 { format!("{} · PID {}", svcctl::state_label(c.status.state), c.status.pid) } else { svcctl::state_label(c.status.state).to_string() };
            fact(g, &mut y, "Status", &st, f(12.0), TX2);
            g.text("Depends on", Rect::new(x, y, label_w - 6.0, 18.0), f(11.5), TX3);
            for (i, l) in dep_lines.iter().enumerate() {
                g.text(l, Rect::new(x + label_w, y + i as f32 * 16.0, vw, 16.0), f(12.0), TX2);
            }
            y += row_h(dep_lines.len().max(1));
        }
        None => {
            y += row_h(1) + 22.0 * 2.0 + row_h(1);
        }
    }
    y += 10.0;

    // ---- startup type
    if p.cfg.is_none() {
        if err_lines.is_empty() {
            g.text("Reading…", Rect::new(x, y, iw, 20.0), f(12.0), TX3);
        }
        y += 30.0;
    } else if let Some(cfg) = &c {
        g.text("STARTUP TYPE", Rect::new(x, y, iw, 18.0), f(10.5).b(), TX4);
        y += 22.0;
        let cur = cfg.kind();
        let changeable = cfg.changeable() && p.t.baseline_problem().is_none() && p.apply.is_none() && svcctl::busy_work(&p.t.key()).is_none();
        for (i, k) in StartKind::ALL.iter().enumerate() {
            let rr = Rect::new(x - 6.0, y, iw + 12.0, 28.0);
            let on = p.sel == Some(*k);
            let hov = changeable && app.hover == Some(Hit::SvcProp(KIND + i));
            if on {
                g.round(rr, 6.0, ACC_BG);
            } else if hov {
                g.round(rr, 6.0, PAN2);
            }
            let (cx, cy) = (rr.x + 16.0, rr.y + rr.h / 2.0);
            g.circle(cx, cy, 6.0, if on { ACC } else { LN });
            g.circle(cx, cy, 5.0, if on { ACC_BG } else { PAN });
            if on {
                g.circle(cx, cy, 2.8, ACC);
            }
            g.text(k.label(), Rect::new(rr.x + 32.0, rr.y, rr.w - 120.0, rr.h), f(12.5), if changeable || on { TX1 } else { TX3 });
            if cur == Some(*k) {
                g.text("current", Rect::new(rr.r() - 76.0, rr.y, 66.0, rr.h), f(11.0).r(), TX3);
            }
            if changeable {
                app.hits.push((rr, Hit::SvcProp(KIND + i)));
            }
            y += 30.0;
        }
        y += 4.0;
        for (lines, col) in &note_lines {
            for l in lines {
                g.text(l, Rect::new(x, y, iw, 15.0), f(11.5), *col);
                y += 15.0;
            }
            y += 4.0;
        }
        y += 22.0;
    }
    for l in &err_lines {
        g.text(l, Rect::new(x, y, iw, 16.0), f(12.0), HOT_TX);
        y += 16.0;
    }
    if !err_lines.is_empty() {
        y += 8.0;
    }
    if let Some((ok, _)) = &p.msg {
        for l in &msg_lines {
            g.text(l, Rect::new(x, y, iw, 16.0), f(12.0), if *ok { OK_TX } else { HOT_TX });
            y += 16.0;
        }
        y += 8.0;
    }

    // ---- buttons
    let bw = |g: &mut Gfx, s: &str| g.measure(s, f(12.0)) + 26.0;
    let (aw, ow, cw) = (bw(g, "Apply"), bw(g, "Open Windows Services"), bw(g, "Copy name"));
    let ar = Rect::new(r.r() - 18.0 - aw, y, aw, 28.0);
    if can_apply(p) {
        accent(g, ar, "Apply", app.hover == Some(Hit::SvcProp(APPLY)), false);
        app.hits.push((ar, Hit::SvcProp(APPLY)));
    } else {
        button(g, ar, "Apply", false, false);
    }
    let orr = Rect::new(ar.x - 8.0 - ow, y, ow, 28.0);
    button(g, orr, "Open Windows Services", app.hover == Some(Hit::SvcProp(OPEN_SCM)), true);
    app.hits.push((orr, Hit::SvcProp(OPEN_SCM)));
    let cpr = Rect::new(x - 0.0, y, cw, 28.0);
    if cpr.r() < orr.x - 6.0 {
        button(g, cpr, "Copy name", app.hover == Some(Hit::SvcProp(COPY)), true);
        app.hits.push((cpr, Hit::SvcProp(COPY)));
    }
    y += 38.0;
    for l in &limit_lines {
        g.text(l, Rect::new(x, y, iw, 15.0), f(11.0), TX3);
        y += 15.0;
    }
    g.unclip();

    // hits scrolled out of the viewport must not catch clicks
    let mut i = hit_start;
    while i < app.hits.len() {
        match super::cpu::visible_rect(app.hits[i].0, vp) {
            Some(r) => {
                app.hits[i].0 = r;
                i += 1;
            }
            None => {
                app.hits.remove(i);
            }
        }
    }
    if max_scroll > 0.0 {
        let th = (vp.h * h / want).max(12.0).min(vp.h);
        let ty = vp.y + (vp.h - th) * p.scroll / max_scroll;
        g.round(Rect::new(r.r() - 8.0, ty, 3.0, th), 1.5, LN);
    }
}

pub fn click(app: &mut App, h: Hit) -> Action {
    let Hit::SvcProp(id) = h else { return Action::None };
    let Some(p) = app.svcprops.as_mut() else { return Action::None };
    match id {
        CLOSE | BACKDROP => app.svcprops = None,
        PANEL_BG => return Action::None,
        COPY => return Action::Clip(p.t.name.clone()),
        OPEN_SCM => {
            if let Err(e) = svcctl::open_services_console() {
                p.msg = Some((false, e));
            }
        }
        APPLY => {
            if can_apply(p) {
                if let (Some(sel), Some(cur)) = (p.sel, p.cfg.as_ref().and_then(|c| c.kind())) {
                    let slot: Slot<Result<StartupDone, String>> = Arc::new(Mutex::new(None));
                    p.apply = Some(slot.clone());
                    p.apply_at = Some(Instant::now());
                    p.msg = Some((true, format!("Changing to {}…", sel.label())));
                    let name = if p.t.display.is_empty() { p.t.name.clone() } else { p.t.display.clone() };
                    let seen = p.t.clone();
                    svcctl::spawn_startup(p.t.clone(), cur, sel, move |r| {
                        // a refusal while elevated greys the quick controls for this service too
                        crate::ui::services::note_startup_result(&seen, &r);
                        // the Services header gets the result too: the panel may be closed by then
                        crate::ui::services::report(match &r {
                            Ok(d) => format!("{name}: {}", d.note),
                            Err(e) => format!("{name}: {e}"),
                        });
                        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                        super::icons::poke();
                    });
                }
            }
        }
        n if (KIND..KIND + 4).contains(&n) => {
            if p.apply.is_none() && p.t.baseline_problem().is_none() && p.cfg.as_ref().is_some_and(|c| c.changeable()) {
                p.sel = Some(StartKind::ALL[n - KIND]);
                p.msg = None;
            }
        }
        _ => {}
    }
    Action::Repaint
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(cfg: Option<Config>, sel: Option<StartKind>) -> Props {
        Props { t: complete_target(), cfg, err: None, sel, msg: None, read: None, apply: None, reset_sel: false, scroll: 0.0, was_busy: false, read_at: None, apply_at: None }
    }
    /// what a list row gives: name, image, a changeable startup type
    fn complete_target() -> Target {
        Target { name: "PanelTestSvc".into(), path: "C:\\x\\p.exe".into(), start: 3, ..Default::default() }
    }
    fn cfg(start: u32) -> Config {
        Config { start, delayed: Some(false), ..Default::default() }
    }

    #[test]
    fn apply_needs_a_read_a_changeable_type_and_a_different_pick() {
        assert!(!can_apply(&props(None, Some(StartKind::Manual))), "nothing read: no write");
        assert!(!can_apply(&props(Some(cfg(3)), Some(StartKind::Manual))), "same as current");
        assert!(can_apply(&props(Some(cfg(3)), Some(StartKind::Disabled))));
        assert!(!can_apply(&props(Some(cfg(1)), Some(StartKind::Disabled))), "driver start type");
        let mut p = props(Some(cfg(3)), Some(StartKind::Disabled));
        p.apply = Some(Arc::new(Mutex::new(None)));
        assert!(!can_apply(&p), "one write at a time");
        let mut p = props(Some(cfg(3)), Some(StartKind::Disabled));
        p.read = Some(Arc::new(Mutex::new(None)));
        assert!(!can_apply(&p), "not while a re-read is in flight");
    }

    #[test]
    fn opening_selects_the_current_type_and_writes_nothing() {
        let mut p = props(None, None);
        p.t = Target { name: "Svc".into(), path: String::new(), ..Default::default() };
        p.reset_sel = true;
        let slot: Slot<Result<Config, ScmError>> = Arc::new(Mutex::new(Some(Ok(Config { name: "svc".into(), start: 4, ..Default::default() }))));
        p.read = Some(slot);
        pump(&mut p);
        assert_eq!(p.sel, Some(StartKind::Disabled));
        assert!(!can_apply(&p), "selection equals current: nothing to apply on open");
    }

    #[test]
    fn a_different_service_or_a_failed_read_closes_the_controls() {
        let mut p = props(None, None);
        p.t = Target { name: "Svc".into(), path: "C:\\a.exe".into(), ..Default::default() };
        p.read = Some(Arc::new(Mutex::new(Some(Ok(Config { name: "Svc".into(), path: "C:\\b.exe".into(), start: 3, ..Default::default() })))));
        pump(&mut p);
        assert!(p.cfg.is_none() && p.err.as_deref().unwrap().contains("no longer the service"));
        p.read = Some(Arc::new(Mutex::new(Some(Err(ScmError::Gone)))));
        pump(&mut p);
        assert!(p.cfg.is_none() && p.err.as_deref().unwrap().contains("no longer exists"));
        assert!(!can_apply(&p));
    }

    #[test]
    fn a_service_without_a_recorded_path_is_shown_but_never_changed() {
        let mut p = props(None, None);
        p.t = Target { name: "NoBase".into(), ..Default::default() };
        p.reset_sel = true;
        p.read = Some(Arc::new(Mutex::new(Some(Ok(Config { name: "NoBase".into(), start: 3, delayed: Some(false), ..Default::default() })))));
        pump(&mut p);
        assert!(p.cfg.is_some() && p.err.is_none(), "read-only view works");
        p.sel = Some(StartKind::Disabled);
        assert!(!can_apply(&p), "no baseline, no Apply");
    }

    #[test]
    fn an_unreadable_delayed_flag_blocks_apply_until_a_fresh_read_has_it() {
        let mut c = cfg(2);
        c.delayed = None;
        assert!(!can_apply(&props(Some(c), Some(StartKind::Manual))), "not known to be Automatic: nothing to change from");
        assert!(can_apply(&props(Some(cfg(2)), Some(StartKind::Manual))));
    }

    #[test]
    fn work_in_flight_on_the_service_blocks_apply_even_for_a_reopened_panel() {
        let mut p = props(Some(cfg(3)), Some(StartKind::Disabled));
        assert!(can_apply(&p));
        let g = svcctl::hold_for_test(&p.t.key(), svcctl::Work::Startup);
        assert!(!can_apply(&p), "the busy registry outlives the panel that started the work");
        // it ends: the panel reads the service again, it does not trust what it showed before
        p.was_busy = true;
        drop(g);
        pump(&mut p);
        assert!(p.reset_sel && p.read.is_some());
    }

    #[test]
    fn a_read_or_write_that_never_answers_ends_in_words_not_forever() {
        let mut p = props(Some(cfg(3)), Some(StartKind::Disabled));
        p.read = Some(Arc::new(Mutex::new(None)));
        p.read_at = Some(Instant::now() - READ_WAIT - Duration::from_secs(1));
        pump(&mut p);
        assert!(p.read.is_none() && p.cfg.is_none() && p.err.as_deref().unwrap().contains("has not answered"));
        assert!(!can_apply(&p));
        let mut p = props(Some(cfg(3)), Some(StartKind::Disabled));
        p.apply = Some(Arc::new(Mutex::new(None)));
        p.apply_at = Some(Instant::now() - APPLY_WAIT - Duration::from_secs(1));
        pump(&mut p);
        assert!(p.apply.is_none() && p.read.is_some(), "stops waiting and reads the service again");
        let m = p.msg.as_ref().unwrap();
        assert!(!m.0 && m.1.contains("may still complete") && m.1.contains("does not cancel"), "{}", m.1);
    }

    #[test]
    fn the_panel_goes_with_its_tab_and_releases_its_hit_areas() {
        let mut app = App::new();
        app.tab = Tab::Services;
        app.svcprops = Some(props(Some(cfg(3)), Some(StartKind::Manual)));
        app.hits.push((Rect::new(0.0, 0.0, 10.0, 10.0), Hit::SvcProp(APPLY)));
        app.hits.push((Rect::new(0.0, 0.0, 10.0, 10.0), Hit::Svc(1)));
        app.switch_tab(Tab::Processes);
        assert!(app.svcprops.is_none());
        assert!(!app.hits.iter().any(|(_, h)| matches!(h, Hit::SvcProp(_))), "no dead click targets left behind");
        assert!(app.hits.iter().any(|(_, h)| matches!(h, Hit::Svc(1))), "other hit areas are not touched");
        // a tab change that does not go through switch_tab (tray, toast) is caught as well
        app.tab = Tab::Services;
        app.svcprops = Some(props(Some(cfg(3)), Some(StartKind::Manual)));
        app.tab = Tab::Watch;
        assert!(!wheel(&mut app, 1.0) && app.svcprops.is_none());
    }

    #[test]
    fn closing_with_a_change_in_flight_does_not_pretend_it_was_cancelled() {
        let mut app = App::new();
        app.tab = Tab::Services;
        let mut p = props(Some(cfg(3)), Some(StartKind::Disabled));
        p.apply = Some(Arc::new(Mutex::new(None)));
        app.svcprops = Some(p);
        let _l = crate::ui::services::MSG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        close(&mut app);
        assert!(app.svcprops.is_none());
        let m = crate::ui::services::last_message();
        assert!(m.contains("still being applied") && m.contains("result will show here"), "{m}");
    }

    #[test]
    fn failed_apply_reports_and_rereads() {
        let mut p = props(Some(cfg(3)), Some(StartKind::Disabled));
        p.apply = Some(Arc::new(Mutex::new(Some(Err("not changed: access denied".into())))));
        pump(&mut p);
        assert_eq!(p.msg.as_ref().map(|m| m.0), Some(false));
        assert!(p.msg.as_ref().unwrap().1.starts_with("Not changed"));
        assert!(p.reset_sel && p.read.is_some(), "re-read to show what the service really has");
    }
}
