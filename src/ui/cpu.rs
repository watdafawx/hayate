//! CPU affinity / CPU sets panel: opened from a process's context menu, drawn
//! over the tab body like Settings (no dialog, nothing modal, nothing before a
//! click). Two clearly separate scopes:
//! - "This process only": applied now to the one process, never saved.
//! - "Always for name.exe": the saved per-exe rule (rules.ini), used by every
//!   copy now and at each start.
//! Nothing here is applied by opening the panel or by changing the selection.

use super::setup::accent;
use super::*;
use crate::features;
use crate::rules::cpu::{self, Change, Cur, Kind, Section};

const CLOSE: usize = 0;
const APPLY: usize = 1;
const RESET: usize = 2;
const SAVE: usize = 3;
const DROP: usize = 4;
const BACKDROP: usize = 5;
const PANEL_BG: usize = 6;
/// chip i = CHIP + i (0 = all, 1 = clear, 2.. = sections)
const CHIP: usize = 10;
/// tile for logical CPU n = TILE + n
const TILE: usize = 100;

const TW: f32 = 34.0;
const TH: f32 = 26.0;
const SMT_GAP: f32 = 2.0;
const CORE_GAP: f32 = 6.0;
const ROW_GAP: f32 = 6.0;
const LABEL_H: f32 = 22.0;
const SECTION_GAP: f32 = 8.0;

pub struct CpuPanel {
    kind: Kind,
    pid: u32,
    create: i64,
    name: String,
    sections: Vec<Section>,
    /// what Windows reports now
    cur: Cur,
    sel: u64,
    scroll: f32,
    /// last result: (ok, text)
    msg: Option<(bool, String)>,
    /// the saved rule's mask for this kind
    saved: Option<u64>,
    gone: bool,
    wd: Widths,
    refreshed: std::time::Instant,
    pending: Option<(crate::rules::SaveStatus, String, Option<u64>)>,
}

/// Open the panel for one process. Reads current state; changes nothing.
pub fn open(app: &mut App, kind: Kind, pid: u32, create: i64, name: &str) {
    super::setup::close(app);
    let topo = cpu::topology();
    let ccds = cpu::ccds();
    let sections = if topo.supported().is_ok() { cpu::sections(topo, ccds) } else { Vec::new() };
    let mut p = CpuPanel {
        kind,
        pid,
        create,
        name: name.to_string(),
        sections,
        cur: Cur::Unreadable("not read".into()),
        sel: 0,
        scroll: 0.0,
        msg: None,
        saved: features::get().and_then(|f| f.rules.saved_cpu(name, kind)),
        gone: false,
        wd: Widths::default(),
        refreshed: std::time::Instant::now(),
        pending: None,
    };
    if let Err(why) = topo.supported() {
        p.msg = Some((false, why));
    }
    refresh(&mut p);
    p.sel = match &p.cur {
        Cur::Mask(m) => *m & topo.valid,
        Cur::Default => 0,
        Cur::Unreadable(_) => if kind == Kind::Affinity { topo.valid } else { 0 },
    };
    if let Some(status) = features::get().and_then(|f| f.rules.persistence()) {
        if let Some(Err(e)) = &*status.lock().unwrap() {
            p.msg = Some((false, format!("Last rule write failed: {e}. Session rules may differ from disk. Retry Save or Remove.")));
        }
    }
    app.cpu = Some(p);
}

/// Re-read the process; a failure becomes the visible message.
fn refresh(p: &mut CpuPanel) {
    p.refreshed = std::time::Instant::now();
    match cpu::read(p.pid, p.create) {
        Ok(rb) => p.cur = rb.of(p.kind).clone(),
        Err(f) => {
            p.cur = Cur::Unreadable(f.msg.clone());
            p.gone |= f.gone;
            p.msg = Some((false, f.msg));
        }
    }
}

/// "0-15" and the count: "0-15 · 16 of 32 CPUs".
fn describe(m: u64, total: u32) -> String {
    format!("{} · {} of {total} CPUs", crate::rules::model::format_cpus(m), m.count_ones())
}

pub(super) fn wrap(g: &mut Gfx, s: &str, w: f32, font: Font, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let next = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if !cur.is_empty() && g.measure(&next, font) > w {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        } else {
            cur = next;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max {
        lines.truncate(max);
        if let Some(l) = lines.last_mut() {
            l.push('…');
        }
    }
    lines
}

/// Where each tile and section label goes, relative to the grid's top-left.
#[derive(Debug, PartialEq)]
pub struct GridLayout {
    pub tiles: Vec<(u8, Rect)>,
    /// (section index, label y)
    pub labels: Vec<(usize, f32)>,
    pub h: f32,
}

/// Flow every CPU of every section into rows of at most `w` px: SMT siblings
/// sit close together, cores wrap as a unit.
pub fn layout_grid(sections: &[Section], w: f32) -> GridLayout {
    let mut tiles = Vec::new();
    let mut labels = Vec::new();
    let mut y = 0.0f32;
    for (si, s) in sections.iter().enumerate() {
        labels.push((si, y));
        y += LABEL_H;
        let mut x = 0.0f32;
        let mut row_used = false;
        for core in &s.cores {
            let cw = core.len() as f32 * TW + (core.len() as f32 - 1.0) * SMT_GAP;
            if row_used && x + cw > w {
                x = 0.0;
                y += TH + ROW_GAP;
            }
            for (k, cpu) in core.iter().enumerate() {
                tiles.push((*cpu, Rect::new(x + k as f32 * (TW + SMT_GAP), y, TW, TH)));
            }
            x += cw + CORE_GAP;
            row_used = true;
        }
        y += TH + SECTION_GAP;
    }
    GridLayout { tiles, labels, h: (y - SECTION_GAP).max(0.0) }
}

fn btn_w(g: &mut Gfx, label: &str) -> f32 {
    g.measure(label, f(12.0)) + 26.0
}

/// Intersection of `r` with the visible viewport (None when hidden).
pub(super) fn visible_rect(r: Rect, vp: Rect) -> Option<Rect> {
    visible(r, vp)
}

fn visible(r: Rect, vp: Rect) -> Option<Rect> {
    let (x0, y0) = (r.x.max(vp.x), r.y.max(vp.y));
    let (x1, y1) = (r.r().min(vp.r()), r.b().min(vp.b()));
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

pub fn overlay(app: &mut App, g: &mut Gfx, body: Rect) {
    let Some(mut p) = app.cpu.take() else { return };
    paint(app, g, body, &mut p);
    app.cpu = Some(p);
}

fn paint(app: &mut App, g: &mut Gfx, body: Rect, p: &mut CpuPanel) {
    if !p.gone && p.refreshed.elapsed().as_secs() >= 1 {
        let message = p.msg.take();
        refresh(p);
        if p.msg.is_none() { p.msg = message; }
    }
    if let Some((status, _, _)) = &p.pending {
        let result = status.lock().unwrap().clone();
        if let Some(result) = result {
            let (_, message, previous) = p.pending.take().unwrap();
            p.msg = Some(match result {
                Ok(()) => (true, message),
                Err(e) => {
                    p.saved = previous;
                    (false, format!("Not saved to disk: {e}. Retry Save or Remove; current session may already be changed."))
                }
            });
        }
    }
    let topo = cpu::topology();
    let total = topo.count();
    if !p.gone && !app.snap.procs.is_empty() && !app.snap.procs.iter().any(|q| q.pid == p.pid && q.create == p.create) {
        p.gone = true;
        p.msg = Some((false, "The process has exited.".into()));
    }
    g.fill_a(body, BG, 0.72);
    app.hits.push((body, Hit::Cpu(BACKDROP)));

    let w = (body.w - 40.0).clamp(300.0, 640.0);
    let iw = w - 36.0;
    let supported = topo.supported().is_ok();

    // ---- fixed text blocks, measured first so the panel height is known
    let explain = match p.kind {
        Kind::Affinity => "Hard limit: threads run only on the CPUs you pick. Children inherit it. Protected processes refuse it.",
        Kind::CpuSets => "Scheduler selection, not a hard limit: hard affinity takes precedence, and threads can set their own.",
    };
    let explain_lines = wrap(g, explain, iw, f(11.5), 2);
    let msg_lines = p.msg.as_ref().map(|(_, t)| wrap(g, t, iw, f(11.5), 3)).unwrap_or_default();
    let msg_h = if msg_lines.is_empty() { 0.0 } else { msg_lines.len() as f32 * 16.0 + 6.0 };

    // chips: wrap into rows
    let mut chips: Vec<(usize, String, u64)> = Vec::new();
    if supported {
        chips.push((CHIP, "All CPUs".into(), topo.valid));
        if p.sections.len() >= 2 {
            for (i, s) in p.sections.iter().enumerate() {
                chips.push((CHIP + 2 + i, s.label.clone(), s.mask));
            }
        }
        chips.push((CHIP + 1, "Clear".into(), 0));
    }
    let mut chip_rows = 1usize;
    {
        let mut x = 0.0;
        for (_, label, _) in &chips {
            let cw = chip_w(&mut p.wd, g, label, None);
            if x > 0.0 && x + cw > iw {
                chip_rows += 1;
                x = 0.0;
            }
            x += cw + 6.0;
        }
    }
    let chips_h = if supported { chip_rows as f32 * 30.0 } else { 0.0 };

    let head_h = 14.0 + 28.0 + 22.0 + explain_lines.len() as f32 * 15.0 + 8.0 + 22.0 + 4.0;
    let rows_h = 2.0 * 46.0;
    let foot_h = msg_h + rows_h + 14.0;
    let grid = layout_grid(&p.sections, iw);
    let want = head_h + chips_h + grid.h + 6.0 + foot_h;
    let max_h = (body.h - 24.0).max(1.0);
    let h = want.min(max_h);
    let vp_h = grid.h;
    let max_scroll = (want - h).max(0.0);
    p.scroll = p.scroll.clamp(0.0, max_scroll);

    let r = Rect::new(body.x + (body.w - w) / 2.0, body.y + ((body.h - h) / 2.0).clamp(12.0, 60.0), w, h);
    g.round(r, 8.0, PAN);
    g.round_stroke(r, 8.0, LN, 1.0);
    app.hits.push((r, Hit::Cpu(PANEL_BG)));
    let x = r.x + 18.0;
    let mut y = r.y + 14.0;

    // ---- title row + close
    g.text(p.kind.title(), Rect::new(x, y, iw - 70.0, 26.0), f(15.0).b(), TX1);
    let cr = Rect::new(r.r() - 18.0 - 62.0, y, 62.0, 26.0);
    button(g, cr, "Close", app.hover == Some(Hit::Cpu(CLOSE)), true);
    app.hits.push((cr, Hit::Cpu(CLOSE)));
    y += 28.0;
    let content_vp = Rect::new(r.x, y, r.w, (r.b() - y - 8.0).max(0.0));
    let hit_start = app.hits.len();
    g.clip(content_vp);
    y -= p.scroll;
    let who = format!("{} · PID {}", p.name, p.pid);
    let who_w = g.measure(&who, f(12.5)) + 4.0;
    g.text(&who, Rect::new(x, y, iw.min(who_w), 20.0), f(12.5), TX2);
    if p.gone {
        g.text("exited", Rect::new(x + who_w.min(iw - 60.0) + 8.0, y, 56.0, 20.0), f(11.5), HOT_TX);
    }
    y += 22.0;
    for l in &explain_lines {
        g.text(l, Rect::new(x, y, iw, 15.0), f(11.5), TX3);
        y += 15.0;
    }
    y += 8.0;

    // ---- now / selected
    let half = (iw * 0.5).floor();
    let now_txt = match (&p.cur, p.kind) {
        _ if p.gone => ("unknown, process exited".to_string(), WARN_TX),
        (Cur::Mask(m), Kind::Affinity) if *m == topo.valid => (format!("All {total} CPUs, no limit"), TX1),
        (Cur::Mask(m), _) => (describe(*m, total), TX1),
        (Cur::Default, _) => ("Default, Windows chooses".to_string(), TX1),
        (Cur::Unreadable(_), _) => ("unreadable".to_string(), WARN_TX),
    };
    g.text("Now", Rect::new(x, y, 30.0, 20.0), f(12.0), TX3);
    g.text(&now_txt.0, Rect::new(x + 34.0, y, half - 40.0, 20.0), f(12.0).m(), now_txt.1);
    g.text("Selected", Rect::new(x + half, y, 56.0, 20.0), f(12.0), TX3);
    let (sel_txt, sel_c) = if p.sel == 0 { ("none".to_string(), WARN_TX) } else { (describe(p.sel, total), TX1) };
    g.text(&sel_txt, Rect::new(x + half + 60.0, y, iw - half - 60.0, 20.0), f(12.0).m(), sel_c);
    y += 24.0;

    // ---- chips
    if supported {
        let mut cx = 0.0;
        for (id, label, mask) in &chips {
            let cw = chip_w(&mut p.wd, g, label, None);
            if cx > 0.0 && cx + cw > iw {
                cx = 0.0;
                y += 30.0;
            }
            let on = p.sel == *mask;
            let hov = app.hover == Some(Hit::Cpu(*id));
            let cr = chip(&mut p.wd, g, x + cx, y, label, None, on, hov);
            app.hits.push((cr, Hit::Cpu(*id)));
            cx += cw + 6.0;
        }
        y += 30.0;
    }

    // ---- CPU grid (scrolls when the window is short)
    let vp = Rect::new(x - 6.0, y, iw + 12.0, vp_h);
    if !supported {
        g.text("CPU selection is off on this PC.", Rect::new(x, y + 2.0, iw, 18.0), f(12.0), TX2);
    } else if vp_h > 0.0 {
        g.clip(vp);
        let gx = x;
        let gy = y;
        for (si, ly) in &grid.labels {
            let s = &p.sections[*si];
            let ry = gy + ly;
            if ry + LABEL_H >= vp.y && ry <= vp.b() {
                let lw = g.measure(&s.label, f(12.0).b()) + 4.0;
                g.text(&s.label, Rect::new(gx, ry, lw.min(iw), LABEL_H), f(12.0).b(), TX1);
                g.text(&s.detail(), Rect::new(gx + lw + 8.0, ry, (iw - lw - 8.0).max(0.0), LABEL_H), f(11.0), TX3);
                g.text(&crate::rules::model::format_cpus(s.mask), Rect::new(gx + iw * 0.55, ry, iw * 0.45, LABEL_H), f(11.0).m().r(), TX3);
            }
        }
        let now_mask = if let Cur::Mask(m) = &p.cur { Some(*m) } else { None };
        for (cpu_i, tr) in &grid.tiles {
            let t = Rect::new(gx + tr.x, gy + tr.y, tr.w, tr.h);
            let Some(hit_r) = visible(t, vp) else { continue };
            let id = TILE + *cpu_i as usize;
            let on = p.sel & (1u64 << cpu_i) != 0;
            let hov = app.hover == Some(Hit::Cpu(id));
            g.round(t, 5.0, if on { ACC_BG } else if hov { PAN3 } else { PAN2 });
            g.round_stroke(t, 5.0, if on { if hov { ACC } else { ACC_LN } } else { LN }, 1.0);
            g.text(&cpu_i.to_string(), t, f(11.5).m().c(), if on { ACC_TX } else { TX2 });
            // dot = in effect now (only when it is a real restriction)
            if let Some(m) = now_mask {
                if m != topo.valid && m & (1u64 << cpu_i) != 0 {
                    g.circle(t.x + 5.0, t.y + 5.0, 1.8, OK);
                }
            }
            app.hits.push((hit_r, Hit::Cpu(id)));
        }
        g.unclip();
    }
    y += vp_h + 6.0;

    // ---- result line
    if let Some((ok, _)) = &p.msg {
        for l in &msg_lines {
            g.text(l, Rect::new(x, y, iw, 16.0), f(11.5), if *ok { OK_TX } else { HOT_TX });
            y += 16.0;
        }
        y += 6.0;
    }

    // ---- this process only
    let have_sel = p.sel != 0 && supported;
    let reset_label = match p.kind {
        Kind::Affinity => "Reset to all CPUs",
        Kind::CpuSets => "Clear to default",
    };
    let live = !p.gone && supported;
    let rw = btn_w(g, reset_label);
    let aw = btn_w(g, "Apply selection");
    let ry = y;
    let br = Rect::new(r.r() - 18.0 - rw, ry + 4.0, rw, 28.0);
    button(g, br, reset_label, app.hover == Some(Hit::Cpu(RESET)), live);
    if live {
        app.hits.push((br, Hit::Cpu(RESET)));
    }
    let ar = Rect::new(br.x - 8.0 - aw, ry + 4.0, aw, 28.0);
    let can_apply = live && have_sel;
    if can_apply {
        accent(g, ar, "Apply selection", app.hover == Some(Hit::Cpu(APPLY)), false);
        app.hits.push((ar, Hit::Cpu(APPLY)));
    } else {
        button(g, ar, "Apply selection", false, false);
    }
    let lw = ar.x - x - 12.0;
    g.text("This process only", Rect::new(x, ry, lw, 20.0), f(12.5).b(), TX1);
    let note = if p.gone { "Exited: nothing to change".to_string() } else { format!("Changes PID {} only, right now", p.pid) };
    g.text(&note, Rect::new(x, ry + 20.0, lw, 18.0), f(11.0), TX3);
    y += 46.0;

    // ---- always rule
    let ft = features::get();
    let ry = y;
    let dw = btn_w(g, "Remove from rule");
    let sw = btn_w(g, "Save rule");
    let dr = Rect::new(r.r() - 18.0 - dw, ry + 4.0, dw, 28.0);
    let save_failed = ft.as_ref().and_then(|f| f.rules.persistence()).is_some_and(|s| matches!(&*s.lock().unwrap(), Some(Err(_))));
    let can_drop = ft.is_some() && (p.saved.is_some() || save_failed) && p.pending.is_none();
    button(g, dr, "Remove from rule", app.hover == Some(Hit::Cpu(DROP)), can_drop);
    if can_drop {
        app.hits.push((dr, Hit::Cpu(DROP)));
    }
    let sr = Rect::new(dr.x - 8.0 - sw, ry + 4.0, sw, 28.0);
    let can_save = ft.is_some() && have_sel && p.pending.is_none() && (p.saved != Some(p.sel) || save_failed);
    if can_save {
        accent(g, sr, "Save rule", app.hover == Some(Hit::Cpu(SAVE)), false);
        app.hits.push((sr, Hit::Cpu(SAVE)));
    } else {
        button(g, sr, "Save rule", false, false);
    }
    let lw = sr.x - x - 12.0;
    g.text(&format!("Always for {}", p.name), Rect::new(x, ry, lw, 20.0), f(12.5).b(), TX1);
    let note = match (ft.is_some(), p.saved) {
        (false, _) => "Rules engine not started".to_string(),
        (true, Some(m)) => format!("Rule: {} · {}", crate::rules::model::format_cpus(m), ft.as_ref().unwrap().rules.cpu_rule_note(&p.name)),
        (true, None) => "No CPU setting in the per-exe rule".to_string(),
    };
    g.text(&note, Rect::new(x, ry + 20.0, lw, 18.0), f(11.0), TX3);
    g.unclip();
    let mut i = hit_start;
    while i < app.hits.len() {
        if let Some(r) = visible(app.hits[i].0, content_vp) {
            app.hits[i].0 = r;
            i += 1;
        } else { app.hits.remove(i); }
    }
    if max_scroll > 0.0 {
        let th = (content_vp.h * h / want).max(12.0).min(content_vp.h);
        let ty = content_vp.y + (content_vp.h - th) * p.scroll / max_scroll;
        g.round(Rect::new(r.r() - 8.0, ty, 3.0, th), 1.5, LN);
    }
}

pub fn close(app: &mut App) {
    app.cpu = None;
}

pub fn wheel(app: &mut App, delta: f32) -> bool {
    match app.cpu.as_mut() {
        Some(p) => {
            p.scroll -= delta * 0.5;
            true
        }
        None => false,
    }
}

pub fn click(app: &mut App, h: Hit) -> Action {
    let Hit::Cpu(id) = h else { return Action::None };
    let Some(p) = app.cpu.as_mut() else { return Action::None };
    let topo = cpu::topology();
    match id {
        CLOSE | BACKDROP => {
            app.cpu = None;
        }
        PANEL_BG => return Action::None,
        APPLY | RESET => {
            let change = if id == RESET { Change::Reset } else { Change::Set(p.sel) };
            match cpu::apply_manual(p.pid, p.create, p.kind, change) {
                Ok(text) => {
                    p.msg = Some((true, text));
                    refresh_keep(p);
                    if id == RESET {
                        p.sel = if p.kind == Kind::Affinity { topo.valid } else { 0 };
                    }
                }
                Err(fail) => {
                    p.gone |= fail.gone;
                    p.msg = Some((false, fail.msg));
                }
            }
        }
        SAVE | DROP => {
            let Some(ft) = features::get() else { return Action::None };
            let value = if id == SAVE { Some(p.sel) } else { None };
            match ft.rules.set_cpu_field(&p.name, p.kind, value) {
                Ok(rule_id) => {
                    if value.is_some() && rule_id != 0 {
                        let rules = ft.rules.clone();
                        crate::bgio::submit_fast("apply CPU rule", move || rules.apply_running(Some(rule_id)));
                    }
                    let previous = p.saved;
                    p.saved = ft.rules.saved_cpu(&p.name, p.kind);
                    p.msg = Some((
                        true,
                        match value {
                            Some(m) => format!(
                                "Saved for {}: {} {}. Check enabled state, rule priority and application results in Rules.",
                                p.name,
                                p.kind.title().to_lowercase(),
                                crate::rules::model::format_cpus(m)
                            ),
                            None => format!("Removed {} from the rule for {}. Running copies keep their current setting.", p.kind.title().to_lowercase(), p.name),
                        },
                    ));
                    if let Some(status) = ft.rules.persistence() {
                        let message = p.msg.take().unwrap().1;
                        p.pending = Some((status, message, previous));
                        p.msg = Some((true, "Saving rule to disk…".into()));
                    }
                    refresh_keep(p);
                }
                Err(e) => p.msg = Some((false, e)),
            }
        }
        i if (CHIP..CHIP + 2 + p.sections.len()).contains(&i) => {
            p.sel = match i - CHIP {
                0 => topo.valid,
                1 => 0,
                k => p.sections[k - 2].mask,
            };
            p.msg = None;
        }
        i if i >= TILE && i < TILE + 64 => {
            let bit = 1u64 << (i - TILE);
            if topo.valid & bit != 0 {
                p.sel ^= bit;
                p.msg = None;
            }
        }
        _ => return Action::None,
    }
    Action::Repaint
}

/// Re-read the process after a change; a failed read joins the result line.
fn refresh_keep(p: &mut CpuPanel) {
    let keep = p.msg.take();
    refresh(p);
    p.msg = match (p.msg.take(), keep) {
        (Some((_, fail)), Some((_, t))) => Some((false, format!("{t} Then: {fail}"))),
        (Some(m), None) => Some(m),
        (None, k) => k,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(label: &str, cores: &[&[u8]]) -> Section {
        let mask = cores.iter().flat_map(|c| c.iter()).fold(0u64, |a, i| a | 1u64 << i);
        Section { label: label.into(), mask, cores: cores.iter().map(|c| c.to_vec()).collect() }
    }

    #[test]
    fn every_cpu_is_placed_once_inside_the_width() {
        let cores: Vec<Vec<u8>> = (0..32u8).step_by(2).map(|i| vec![i, i + 1]).collect();
        let refs: Vec<&[u8]> = cores.iter().map(|c| c.as_slice()).collect();
        let s = vec![sec("A", &refs[..8]), sec("B", &refs[8..])];
        for w in [300.0f32, 400.0, 518.0, 640.0] {
            let l = layout_grid(&s, w - 36.0);
            let mut seen: Vec<u8> = l.tiles.iter().map(|(c, _)| *c).collect();
            seen.sort_unstable();
            assert_eq!(seen, (0..32u8).collect::<Vec<_>>(), "width {w}");
            for (_, r) in &l.tiles {
                assert!(r.x >= 0.0 && r.r() <= w - 36.0 + 0.01, "tile {r:?} outside {w}");
                assert!(r.b() <= l.h + 0.01);
            }
            assert_eq!(l.labels.len(), 2);
        }
    }

    #[test]
    fn narrow_width_wraps_cores_whole() {
        let s = vec![sec("A", &[&[0, 1], &[2, 3], &[4, 5], &[6, 7]])];
        let wide = layout_grid(&s, 600.0);
        let narrow = layout_grid(&s, 150.0);
        assert!(narrow.h > wide.h);
        // SMT siblings stay on one row, side by side
        for pair in [(0u8, 1u8), (2, 3), (4, 5), (6, 7)] {
            let a = narrow.tiles.iter().find(|(c, _)| *c == pair.0).unwrap().1;
            let b = narrow.tiles.iter().find(|(c, _)| *c == pair.1).unwrap().1;
            assert_eq!(a.y, b.y);
            assert!(b.x > a.x);
        }
    }

    #[test]
    fn sixty_four_cpus_fit_and_none_are_dropped() {
        let singles: Vec<Vec<u8>> = (0..64u8).map(|i| vec![i]).collect();
        let refs: Vec<&[u8]> = singles.iter().map(|c| c.as_slice()).collect();
        let l = layout_grid(&[sec("All", &refs)], 300.0);
        assert_eq!(l.tiles.len(), 64);
        assert!(l.tiles.iter().any(|(c, _)| *c == 63));
    }

    #[test]
    fn empty_sections_make_an_empty_grid() {
        let l = layout_grid(&[], 400.0);
        assert!(l.tiles.is_empty());
        assert_eq!(l.h, 0.0);
    }

    #[test]
    fn viewport_clipping() {
        let vp = Rect::new(0.0, 100.0, 200.0, 50.0);
        assert!(visible(Rect::new(10.0, 10.0, 30.0, 20.0), vp).is_none());
        assert_eq!(visible(Rect::new(10.0, 90.0, 30.0, 20.0), vp), Some(Rect::new(10.0, 100.0, 30.0, 10.0)));
        assert!(visible(Rect::new(10.0, 150.0, 30.0, 20.0), vp).is_none());
    }

    #[test]
    fn describe_counts() {
        assert_eq!(describe(0xFF, 16), "0-7 · 8 of 16 CPUs");
    }
}
