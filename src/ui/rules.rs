//! Rules tab: saved per-process rules (Process Lasso style), settings toggles.

use super::*;
use crate::features;

#[derive(Default)]
pub struct RulesView {
    pub sel: Option<u32>,
    pub scroll: f32,
    /// scroll this rule into view at the next paint (set when it is opened from elsewhere)
    pub reveal: Option<u32>,
    /// keyboard: rule ids in display order as of the last paint
    pub nav: super::nav::ListNav<u32>,
}

pub fn paint(app: &mut App, g: &mut Gfx, body: Rect) {
    let area = body.inset(12.0, 10.0);
    let Some(ft) = features::get() else {
        g.text("Rules engine not started", area, f(13.0).c(), DIM);
        return;
    };
    let rules = ft.rules.rules();
    let st = ft.rules.settings();
    let es = crate::etw::status();

    // header line
    let src = match es.source {
        crate::etw::Source::Etw => ("ETW · instant".to_string(), OK),
        crate::etw::Source::Snapshot => (format!("snapshot 1 s{}", es.error.map(|e| format!(" · {e}")).unwrap_or_default()), WARM),
        crate::etw::Source::Off => ("off".to_string(), HOT),
    };
    g.text("RULES", Rect::new(area.x, area.y, 80.0, 20.0), f(11.0).b(), TX2);
    g.text(&format!("{} saved · applied on process start via {}", rules.len(), src.0), Rect::new(area.x + 52.0, area.y, area.w - 52.0, 20.0), f(11.5), src.1);

    // settings toggles
    let mut y = area.y + 26.0;
    let toggles = [
        (0usize, "CCD presets (CCD0 V-Cache / CCD1 frequency) in rule menus", st.ccd_presets_enabled, true),
        (1usize, "Restraint (ProBalance-lite) · coming in v2", st.restraint_enabled, false),
    ];
    for (i, label, on, enabled) in toggles {
        let r = Rect::new(area.x, y, area.w, 22.0);
        toggle(g, Rect::new(r.x, r.y + 3.5, 28.0, 15.0), on, enabled);
        g.text(label, Rect::new(r.x + 40.0, r.y, r.w - 40.0, r.h), f(12.0), if enabled { TX } else { DIM });
        if enabled {
            app.hits.push((r, Hit::Setting(i)));
        }
        y += 24.0;
    }
    for w in ft.rules.warnings() {
        g.text(&format!("⚠ {w}"), Rect::new(area.x, y, area.w, 18.0), f(11.5), WARM);
        y += 20.0;
    }
    // Disallow rules that are switched on but cannot be enforced: never let them look active
    let worker_broken = ft.rules.worker_problem();
    if let Some(why) = worker_broken.as_ref().filter(|_| ft.rules.rules().iter().any(|r| r.blocks())) {
        g.text(&format!("⚠ Disallow is NOT running: {why}. Rules marked Disallow ON end nothing."), Rect::new(area.x, y, area.w, 18.0), f(11.5), HOT);
        y += 20.0;
    }
    let worker_broken = worker_broken.is_some();
    // the last write to rules.ini failed: what is listed may not be what is on disk
    if let Some(Err(e)) = ft.rules.persistence().and_then(|s| s.lock().unwrap().clone()) {
        g.text(&format!("⚠ Not saved to disk: {e}. Rules shown work until Hayate closes."), Rect::new(area.x, y, area.w, 18.0), f(11.5), HOT);
        y += 20.0;
    }
    y += 6.0;

    // table
    let cols = [("", 34.0), ("Process", 0.28), ("Rule", 0.40), ("Hits", 60.0), ("Last result", 0.0)];
    let fixed: f32 = 34.0 + 60.0;
    // the scroll bar takes its gutter from the columns only while the list overflows (rows are 30 px, the header 24)
    let aw = area.w - super::nav::gutter(rules.len() as f32 * 30.0, area.b() - (y + 24.0));
    let flex = aw - fixed;
    let widths = [34.0, flex * 0.26, flex * 0.40, 60.0, flex * 0.34];
    let hr = Rect::new(area.x, y, area.w, 24.0);
    g.fill(Rect::new(area.x, hr.b() - 1.0, area.w, 1.0), LN);
    let mut x = area.x;
    for (k, (label, _)) in cols.iter().enumerate() {
        g.text(label, Rect::new(x + 6.0, hr.y, widths[k] - 8.0, hr.h), if k == 3 { f(11.5).r() } else { f(11.5) }, DIM);
        x += widths[k];
    }
    y = hr.b();
    let page = app.rules.nav.page;
    app.rules.nav.begin(page);
    for r in rules.iter() {
        app.rules.nav.push(r.id, app.rules.sel == Some(r.id));
    }
    if rules.is_empty() {
        g.text("No rules yet. Right-click a process → Rules → Always… to add one.", Rect::new(area.x, y + 20.0, area.w, 20.0), f(12.0).c(), DIM);
        return;
    }
    let row_h = 30.0;
    let list = Rect::new(area.x, y, area.w, area.b() - y);
    let max_scroll = (rules.len() as f32 * row_h - list.h).max(0.0);
    app.rules.nav.page = super::nav::page_rows(list.h, row_h);
    if let Some(id) = app.rules.reveal.take() {
        if let Some(i) = rules.iter().position(|r| r.id == id) {
            let (top, bottom) = (i as f32 * row_h, (i + 1) as f32 * row_h);
            if top < app.rules.scroll {
                app.rules.scroll = top;
            } else if bottom > app.rules.scroll + list.h {
                app.rules.scroll = bottom - list.h;
            }
        }
    }
    app.rules.scroll = app.rules.scroll.clamp(0.0, max_scroll);
    g.clip(list);
    for (i, r) in rules.iter().enumerate() {
        let ry = list.y + i as f32 * row_h - app.rules.scroll;
        if ry + row_h < list.y || ry > list.b() {
            continue;
        }
        let rr = Rect::new(area.x, ry, aw, row_h);
        let sel = app.rules.sel == Some(r.id);
        if sel {
            g.fill(rr, ACC_BG);
        } else if app.hover == Some(Hit::RuleRow(i)) {
            g.fill(rr, PAN2);
        }
        g.fill(Rect::new(area.x, rr.b() - 1.0, aw, 1.0), SEP);
        app.hits.push((rr, Hit::RuleRow(i)));
        let tr = Rect::new(rr.x + 4.0, rr.y + 7.0, 26.0, 16.0);
        toggle(g, tr, r.enabled, true);
        app.hits.push((tr, Hit::RuleToggle(i)));
        let mut x = area.x + widths[0];
        let col = if r.enabled { TX } else { DIM };
        let ir = Rect::new(x + 6.0, rr.y + 7.0, 16.0, 16.0);
        let (ipath, ipid) = icon_source(&r.pattern, &app.snap);
        if !super::icons::draw(&mut app.icons, g, &ipath, ipid, ir) {
            g.round(Rect::new(ir.x + 2.0, ir.y + 2.0, 12.0, 12.0), 3.0, PAN2);
        }
        g.text(&r.pattern, Rect::new(x + 28.0, rr.y, widths[1] - 32.0, rr.h), f(12.5), col);
        x += widths[1];
        let blocks = r.actions.disallow;
        let state = super::disallow::rule_state(r, worker_broken);
        let sum = if blocks { format!("{} · {state}", r.actions.summary()) } else { r.actions.summary() };
        g.text(&sum, Rect::new(x + 6.0, rr.y, widths[2] - 10.0, rr.h), f(12.0), if blocks && r.enabled { HOT } else if r.enabled { C_CPU } else { DIM });
        x += widths[2];
        g.text(&group_num(r.hits), Rect::new(x, rr.y, widths[3] - 6.0, rr.h), f(12.0).m().r(), col);
        x += widths[3];
        let (lt, lc) = match &r.last {
            None if r.actions.disallow => ("no start seen yet".to_string(), DIM),
            None => ("never matched".to_string(), DIM),
            Some(l) => {
                let lock = if l.protected { "🔒 " } else { "" };
                (format!("{} · {lock}{}", ago(l.time), l.text), if l.ok { DIM } else if l.protected { WARM } else { HOT })
            }
        };
        g.text(&lt, Rect::new(x + 8.0, rr.y, widths[4] - 10.0, rr.h), f(11.5), lc);
    }
    g.unclip();
    let sc = app.rules.scroll;
    super::nav::paint_bar(app, g, super::nav::BAR_RULES, list, rules.len() as f32 * row_h, sc);
}

/// Image for a rule's icon: a full path as is, a bare `name.exe` through a
/// running process of that name; globs get none.
fn icon_source(pattern: &str, snap: &Snapshot) -> (String, u32) {
    if pattern.contains(['*', '?']) {
        return (String::new(), 0);
    }
    if pattern.contains('\\') {
        return (pattern.to_string(), 0);
    }
    snap.procs
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(pattern))
        .and_then(|p| p.meta.path.as_deref().map(|path| (path.to_string(), p.pid)))
        .unwrap_or_default()
}

/// On: ACC track with a BG knob. Off: TRACK with an LN ring and a TX4 knob.
/// Disabled: the on track drops to ACC_LN, the off knob to LN.
pub fn toggle(g: &mut Gfx, r: Rect, on: bool, enabled: bool) {
    let rad = r.h / 2.0;
    if on {
        g.round(r, rad, if enabled { ACC } else { ACC_LN });
    } else {
        g.round(r, rad, TRACK);
        g.round_stroke(r, rad, LN, 1.0);
    }
    let d = r.h - 4.0;
    let cx = if on { r.r() - 2.0 - d / 2.0 } else { r.x + 2.0 + d / 2.0 };
    g.circle(cx, r.y + r.h / 2.0, d / 2.0, if on { BG } else if enabled { TX4 } else { LN });
}

pub fn ago(t: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let d = now.saturating_sub(t);
    if d < 60 {
        format!("{d}s ago")
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

/// Up / Down / Page / Home / End: select by rule id; it scrolls into view at the next paint.
pub fn nav(app: &mut App, m: super::nav::Move) {
    let sel = app.rules.sel;
    if let Some(id) = app.rules.nav.step(sel.is_some(), |&id| Some(id) == sel, m) {
        app.rules.sel = Some(id);
        app.rules.reveal = Some(id);
    }
}

pub fn click(app: &mut App, h: Hit) -> Action {
    let Some(ft) = features::get() else { return Action::None };
    match h {
        Hit::Setting(0) => {
            let mut s = ft.rules.settings();
            s.ccd_presets_enabled = !s.ccd_presets_enabled;
            ft.rules.set_settings(s);
            Action::Repaint
        }
        Hit::RuleToggle(i) => {
            if let Some(r) = ft.rules.rules().get(i).cloned() {
                if r.actions.disallow && !r.enabled {
                    // switching a Disallow rule ON ends programs: explicit confirmation first
                    super::disallow::open_rule(app, r.id);
                } else {
                    ft.rules.set_enabled(r.id, !r.enabled);
                }
            }
            Action::Repaint
        }
        Hit::RuleRow(i) => {
            app.rules.sel = ft.rules.rules().get(i).map(|r| r.id);
            Action::Repaint
        }
        _ => Action::None,
    }
}
