//! Install offer bar (portable copy), the install panel and Settings
//! (install status, uninstall). Panels are drawn over the tab body inside
//! Blink's own window: no dialogs, nothing modal, nothing before a click.

use super::*;
use crate::setup::logic::{Choice, Where, INSTALL_ALL_HINT, INSTALL_ALL_LABEL, INSTALL_USER_LABEL, REPLACE_USER_SUB, REPLACE_USER_TITLE};

const BAR_H: f32 = 38.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Panel {
    Install,
    Settings,
}

// Hit::Setup ids
const B_INSTALL: usize = 0;
const B_NOT_NOW: usize = 1;
const B_NEVER: usize = 2;
const BACKDROP: usize = 10;
const T_ADMIN: usize = 11;
const T_AUTO: usize = 12;
const T_UPDATE: usize = 13;
const P_INSTALL: usize = 14;
const P_CANCEL: usize = 15;
const PANEL_BG: usize = 16;
pub const GEAR: usize = 20;
const S_INSTALL: usize = 21;
const T_DATA: usize = 22;
const S_UNINSTALL: usize = 23;
const S_CONFIRM: usize = 24;
const S_CLOSE: usize = 25;
const T_BUILTIN: usize = 26;
const T_CCD: usize = 27;
const T_JUMP: usize = 28;
const N_COPY: usize = 29;
const N_SAVE: usize = 30;
const T_TM: usize = 31;
const U_CHECK: usize = 32;
/// Settings: height of the Updates block between Install and CPU
const UPD_H: f32 = 86.0;
const Z_DEC: usize = 33;
const Z_INC: usize = 34;
const Z_RESET: usize = 35;
const S_UNINSTALL_ALL: usize = 36;
const T_REPLACE: usize = 37;
const P_INSTALL_ALL: usize = 38;
const T_TM_REFRESH: usize = 39;
/// Settings: height of the CPU block between Install and Leak alerts
const CPU_H: f32 = 78.0;
/// Settings: height of the Taskbar block between CPU and Leak alerts
const TASK_H: f32 = 214.0;
/// Settings: height of the Task Manager block between Taskbar and Leak alerts
const TM_H: f32 = 130.0;
/// Settings: the × of the user's leak-ignore entry (plus its index)
const S_UNIGNORE: usize = 100;

#[derive(Default)]
pub struct SetupView {
    pub panel: Option<Panel>,
    pub choice: Choice,
    pub remove_data: bool,
    pub confirm: bool,
    pub error: Option<String>,
    /// install bar shown (portable, not answered)
    pub bar: bool,
    /// Settings: built-in leak-ignore names expanded
    pub builtin_open: bool,
    /// Settings: how far the content is scrolled (short windows)
    pub scroll: f32,
}

impl SetupView {
    pub fn new() -> Self {
        let st = crate::setup::status();
        SetupView { bar: crate::setup::bar_visible(), choice: Choice { update: st.update_source.is_some(), replace_user: crate::setup::user_install_exists(), ..Default::default() }, ..Default::default() }
    }
}

/// The install bar at the top of the body; returns the body below it.
pub fn bar(app: &mut App, g: &mut Gfx, body: Rect) -> Rect {
    if !app.setup.bar {
        return body;
    }
    let r = Rect::new(body.x, body.y, body.w, BAR_H);
    g.fill(r, ACC_BG);
    g.fill(Rect::new(r.x, r.b() - 1.0, r.w, 1.0), ACC_LN);
    g.text("\u{E896}", Rect::new(r.x + 14.0, r.y, 18.0, r.h), f(13.0).icon(), ACC);
    let btns = [(B_NEVER, "Don't ask again"), (B_NOT_NOW, "Run without installing"), (B_INSTALL, "Install just for me…")];
    let mut x = r.r() - 10.0;
    for (id, label) in btns {
        let w = g.measure(label, f(12.0)) + 24.0;
        x -= w;
        let br = Rect::new(x, r.y + 6.0, w, 26.0);
        let hov = app.hover == Some(Hit::Setup(id));
        if id == B_INSTALL {
            g.round(br, 6.0, if hov { 0x6fd6e4 } else { ACC });
            g.text(label, br, f(12.0).b().c(), BG);
        } else {
            button(g, br, label, hov, true);
        }
        app.hits.push((br, Hit::Setup(id)));
        x -= 8.0;
    }
    let dir = &crate::setup::status().exe_dir;
    g.text(&format!("Hayate is running from {dir} without being installed. Install it for a Start menu shortcut, notifications and auto-update?"), Rect::new(r.x + 40.0, r.y, x - r.x - 48.0, r.h), f(12.0), ACC_TX);
    Rect::new(body.x, body.y + BAR_H, body.w, body.h - BAR_H)
}

/// Settings gear at the bottom of the rail.
pub fn rail_button(app: &mut App, g: &mut Gfx, h: f32) {
    let br = Rect::new(0.0, h - STATUS_H - 44.0, RAIL_W - 1.0, 38.0);
    let on = app.setup.panel == Some(Panel::Settings);
    let hov = app.hover == Some(Hit::Setup(GEAR));
    if on {
        g.round(Rect::new(0.0, br.y + 10.0, 2.0, 18.0), 1.0, ACC);
    }
    g.text("\u{E115}", br, f(15.0).icon().c(), if on || hov { TX1 } else { TX3 });
    app.hits.push((br, Hit::Setup(GEAR)));
    if hov {
        let tr = Rect::new(RAIL_W + 2.0, br.y + 8.0, g.measure("Settings", f(11.5)) + 16.0, 22.0);
        g.round(tr, 5.0, PAN3);
        g.round_stroke(tr, 5.0, LN, 1.0);
        g.text("Settings", tr, f(11.5).c(), TX1);
    }
}

pub(super) fn accent(g: &mut Gfx, r: Rect, label: &str, hov: bool, danger: bool) {
    let c = if danger { if hov { 0xf38a8a } else { HOT } } else if hov { 0x6fd6e4 } else { ACC };
    g.round(r, 6.0, c);
    g.text(label, r, f(12.0).b().c(), BG);
}

/// One option row: title, subline, toggle on the right.
#[allow(clippy::too_many_arguments)]
fn option(app: &mut App, g: &mut Gfx, x: f32, y: f32, w: f32, id: usize, title: &str, sub: &str, on: bool, enabled: bool) {
    let rr = Rect::new(x, y, w, 44.0);
    if enabled && app.hover == Some(Hit::Setup(id)) {
        g.round(rr, 6.0, PAN2);
    }
    g.text(title, Rect::new(x + 10.0, y + 4.0, w - 60.0, 20.0), f(12.5), if enabled { TX1 } else { TX3 });
    g.text(sub, Rect::new(x + 10.0, y + 22.0, w - 60.0, 18.0), f(11.0), TX3);
    super::rules::toggle(g, Rect::new(rr.r() - 40.0, y + 14.5, 28.0, 15.0), on, enabled);
    if enabled {
        app.hits.push((rr, Hit::Setup(id)));
    }
}

/// The open panel, centred over the body (the bar and tab stay visible).
pub fn overlay(app: &mut App, g: &mut Gfx, body: Rect) {
    let Some(p) = app.setup.panel else { return };
    g.fill_a(body, BG, 0.72);
    app.hits.push((body, Hit::Setup(BACKDROP)));
    let w = (body.w - 40.0).clamp(300.0, 640.0);
    let leak = (p == Panel::Settings).then(|| leak_layout(g, w - 36.0, app.setup.builtin_open));
    let tm = (p == Panel::Settings).then(|| tm_layout(g, w - 36.0));
    let want = match p {
        Panel::Install => 370.0 + 60.0 + if crate::setup::user_install_exists() { 46.0 } else { 0.0 },
        Panel::Settings => 250.0 + UPD_H + CPU_H + TASK_H + tm.as_ref().map_or(TM_H, |t| t.h) + leak.as_ref().map(|l| l.h + 8.0).unwrap_or(0.0),
    } + app.setup.error.as_deref().map_or(0.0, err_h);
    // a short window scrolls the Settings content instead of pushing Close off screen
    let h = if p == Panel::Settings { want.min((body.h - 24.0).max(240.0)) } else { want };
    app.setup.scroll = app.setup.scroll.clamp(0.0, (want - h).max(0.0));
    let scroll = if p == Panel::Settings { app.setup.scroll } else { 0.0 };
    let r = Rect::new(body.x + (body.w - w) / 2.0, body.y + ((body.h - h) / 2.0).clamp(12.0, 80.0), w, h);
    g.round(r, 8.0, PAN);
    g.round_stroke(r, 8.0, LN, 1.0);
    app.hits.push((r, Hit::Setup(PANEL_BG)));
    let (x, iw) = (r.x + 18.0, r.w - 36.0);
    let mut y = r.y + 14.0;
    let st = crate::setup::status();
    match p {
        Panel::Install => {
            g.text("Install Hayate just for me", Rect::new(x, y, iw, 24.0), f(15.0).b(), TX1);
            y += 28.0;
            g.text(&format!("Copies this blink.exe to {}", st.install_dir), Rect::new(x, y, iw, 18.0), f(12.0), TX2);
            y += 18.0;
            g.text("Only for your Windows account, no administrator needed. Remove it any time in Settings or in Windows' Installed apps.", Rect::new(x, y, iw, 18.0), f(11.5), TX3);
            y += 26.0;
            g.fill(Rect::new(x, y, iw, 1.0), SEP);
            y += 6.0;
            let c = app.setup.choice;
            option(app, g, x - 10.0, y, iw + 20.0, T_ADMIN, "Run as administrator by default", "Start-menu Hayate asks UAC at each start and shows temperatures; \u{201c}Hayate (no admin)\u{201d} stays plain", c.admin_default, true);
            y += 46.0;
            option(app, g, x - 10.0, y, iw + 20.0, T_AUTO, "Start with Windows", "Hidden in the tray at sign-in, never elevated. \u{201c}Install for everyone\u{201d} starts it for every user (Start with Windows for everyone)", c.autostart, true);
            y += 46.0;
            match &st.update_source {
                Some(src) => option(app, g, x - 10.0, y, iw + 20.0, T_UPDATE, &format!("Auto-update from {src}"), "This copy came from a publish folder: new builds there swap in by themselves", c.update, true),
                None => option(app, g, x - 10.0, y, iw + 20.0, T_UPDATE, "Auto-update", "Off: this exe is not in a publish folder", false, false),
            }
            y += 54.0;
            if crate::setup::user_install_exists() {
                option(app, g, x - 10.0, y, iw + 20.0, T_REPLACE, REPLACE_USER_TITLE, REPLACE_USER_SUB, c.replace_user, true);
                y += 46.0;
            }
            g.text("Writes", Rect::new(x, y, iw, 16.0), f(11.0).b(), TX3);
            y += 16.0;
            g.text(&format!("{}\\blink.exe · Start-menu shortcuts{}", st.install_dir, if c.autostart { " · Startup shortcut" } else { "" }), Rect::new(x, y, iw, 16.0), f(11.0), TX3);
            y += 16.0;
            g.text(&format!("Installed-apps entry + toast click handler under HKCU{}",if c.update && st.update_source.is_some() { " · update.ini" } else { "" }), Rect::new(x, y, iw, 16.0), f(11.0), TX3);
            y += 24.0;
            if let Some(e) = &app.setup.error {
                g.text(e, Rect::new(x, y, iw, 18.0), f(11.5), HOT_TX);
            }
            let by = r.b() - 44.0;
            // second choice: every user of this PC (UAC), above the buttons
            g.text(INSTALL_ALL_HINT, Rect::new(x, by - 54.0, iw, 16.0), f(11.0), TX3);
            let er = Rect::new(x, by - 36.0, iw, 28.0);
            button(g, er, INSTALL_ALL_LABEL, app.hover == Some(Hit::Setup(P_INSTALL_ALL)), true);
            app.hits.push((er, Hit::Setup(P_INSTALL_ALL)));
            let ir = Rect::new(r.r() - 18.0 - 170.0, by, 170.0, 30.0);
            accent(g, ir, if c.admin_default { "Install just for me (UAC)" } else { INSTALL_USER_LABEL }, app.hover == Some(Hit::Setup(P_INSTALL)), false);
            app.hits.push((ir, Hit::Setup(P_INSTALL)));
            let cr = Rect::new(ir.x - 98.0, by, 90.0, 30.0);
            button(g, cr, "Cancel", app.hover == Some(Hit::Setup(P_CANCEL)), true);
            app.hits.push((cr, Hit::Setup(P_CANCEL)));
        }
        Panel::Settings => {
            g.text("Settings", Rect::new(x, y, iw, 24.0), f(15.0).b(), TX1);
            // Zoom row in the header: Zoom  [-] 125% [+] [Reset]
            {
                let (bh, zy) = (24.0, y);
                let mut zx = x + iw;
                zx -= 56.0;
                let rr = Rect::new(zx, zy, 56.0, bh);
                button(g, rr, "Reset", app.hover == Some(Hit::Setup(Z_RESET)), true);
                app.hits.push((rr, Hit::Setup(Z_RESET)));
                zx -= 30.0;
                let ir = Rect::new(zx, zy, 26.0, bh);
                button(g, ir, "+", app.hover == Some(Hit::Setup(Z_INC)), crate::zoom::next(crate::zoom::percent()) != crate::zoom::percent());
                app.hits.push((ir, Hit::Setup(Z_INC)));
                zx -= 50.0;
                g.text(&format!("{}%", crate::zoom::percent()), Rect::new(zx, zy, 46.0, bh), f(12.0).b().c(), TX1);
                zx -= 30.0;
                let dr = Rect::new(zx, zy, 26.0, bh);
                button(g, dr, "\u{2212}", app.hover == Some(Hit::Setup(Z_DEC)), crate::zoom::prev(crate::zoom::percent()) != crate::zoom::percent());
                app.hits.push((dr, Hit::Setup(Z_DEC)));
                zx -= 44.0;
                g.text("Zoom", Rect::new(zx, zy, 40.0, bh), f(12.0).c(), TX3);
            }
            let by = r.b() - 44.0;
            let close = Rect::new(r.r() - 18.0 - 90.0, by, 90.0, 30.0);
            let vp = Rect::new(r.x + 1.0, r.y + 42.0, r.w - 2.0, by - 6.0 - app.setup.error.as_deref().map_or(0.0, |e| err_h(e) + 2.0) - (r.y + 42.0));
            let n0 = app.hits.len();
            g.clip(vp);
            y = r.y + 44.0 - scroll;
            g.text("INSTALL", Rect::new(x, y, iw, 16.0), f(10.5).b(), TX4);
            y += 18.0;
            match st.at {
                Where::Portable => {
                    g.text(&format!("Portable · {}", st.exe_dir), Rect::new(x, y, iw, 18.0), f(12.5), TX1);
                    y += 18.0;
                    let note = match &st.installed {
                        Some(d) => format!("Hayate is also installed in {d}; this copy changes nothing there."),
                        None => "Nothing is installed: no shortcuts, no registry, no autostart.".into(),
                    };
                    g.text(&note, Rect::new(x, y, iw, 18.0), f(11.5), TX3);
                    y += 28.0;
                    let ir = Rect::new(x, y, 160.0, 30.0);
                    accent(g, ir, if st.installed.is_some() { "Install this copy…" } else { "Install Hayate…" }, app.hover == Some(Hit::Setup(S_INSTALL)), false);
                    app.hits.push((ir, Hit::Setup(S_INSTALL)));
                }
                Where::Machine => {
                    g.text(&format!("Installed for all users · {}", st.exe_dir), Rect::new(x, y, iw, 18.0), f(12.5), TX1);
                    y += 18.0;
                    g.text("Also in Windows Settings > Apps > Installed apps. Asks for administrator permission, then asks you to confirm.", Rect::new(x, y, iw, 18.0), f(11.5), TX3);
                    y += 28.0;
                    let ur = Rect::new(x, y, 150.0, 30.0);
                    button(g, ur, "Uninstall Hayate\u{2026}", app.hover == Some(Hit::Setup(S_UNINSTALL_ALL)), true);
                    app.hits.push((ur, Hit::Setup(S_UNINSTALL_ALL)));
                }
                Where::User => {
                    g.text(&format!("Installed · {}", st.install_dir), Rect::new(x, y, iw, 18.0), f(12.5), TX1);
                    y += 18.0;
                    g.text("Start menu shortcuts · notification-centre clicks open Watch", Rect::new(x, y, iw, 18.0), f(11.5), TX3);
                    y += 24.0;
                    let data = crate::osutil::data_dir();
                    option(app, g, x - 10.0, y, iw + 20.0, T_DATA, "Also delete my data", &format!("{}: rules, Watch history, settings, log", data.display()), app.setup.remove_data, true);
                    y += 50.0;
                    if app.setup.confirm {
                        g.text("Remove Hayate from this PC?", Rect::new(x, y, iw - 230.0, 30.0), f(12.5).b(), WARN_TX);
                        let ur = Rect::new(x + iw - 130.0, y, 130.0, 30.0);
                        accent(g, ur, "Uninstall now", app.hover == Some(Hit::Setup(S_CONFIRM)), true);
                        app.hits.push((ur, Hit::Setup(S_CONFIRM)));
                    } else {
                        let ur = Rect::new(x, y, 150.0, 30.0);
                        button(g, ur, "Uninstall Hayate…", app.hover == Some(Hit::Setup(S_UNINSTALL)), true);
                        app.hits.push((ur, Hit::Setup(S_UNINSTALL)));
                    }
                }
            }
            update_block(app, g, x, r.y + 196.0 - scroll, iw);
            cpu_block(app, g, x, r.y + 196.0 + UPD_H - scroll, iw);
            taskbar_block(app, g, x, r.y + 196.0 + UPD_H + CPU_H - scroll, iw);
            if let Some(t) = &tm {
                taskmgr_block(app, g, x, r.y + 196.0 + UPD_H + CPU_H + TASK_H - scroll, iw, t);
            }
            if let Some(l) = &leak {
                leak_paint(app, g, x, r.y + 196.0 + UPD_H + CPU_H + TASK_H + tm.as_ref().map_or(0.0, |t| t.h) - scroll, iw, l);
            }
            g.unclip();
            // clicks only where the content is visible (hits are not clipped by the drawing)
            let mut k = n0;
            while k < app.hits.len() {
                match super::cpu::visible_rect(app.hits[k].0, vp) {
                    Some(v) => {
                        app.hits[k].0 = v;
                        k += 1;
                    }
                    None => {
                        app.hits.remove(k);
                    }
                }
            }
            // the content is `vp.h` plus what the short window cuts off
            super::nav::paint_bar(app, g, super::nav::BAR_SETTINGS, vp, vp.h + (want - h).max(0.0), scroll);
            if let Some(e) = &app.setup.error {
                // the helper's detail can be long: up to three lines, drawn upward from the buttons
                let ls = super::cpu::wrap(g, e, iw, f(11.5), 3);
                let top = by - 6.0 - ERR_LINE_H * ls.len() as f32;
                for (n, t) in ls.iter().enumerate() {
                    g.text(t, Rect::new(x, top + ERR_LINE_H * n as f32, iw, 18.0), f(11.5), HOT_TX);
                }
            }
            button(g, close, if app.setup.confirm { "Cancel" } else { "Close" }, app.hover == Some(Hit::Setup(S_CLOSE)), true);
            app.hits.push((close, Hit::Setup(S_CLOSE)));
        }
    }
}

/// Settings "Updates" block: the manual check that also overrides a stuck
/// (rolled back / declined) update.
fn update_block(app: &mut App, g: &mut Gfx, x: f32, y: f32, w: f32) {
    let on = crate::update::enabled();
    g.fill(Rect::new(x, y - 8.0, w, 1.0), SEP);
    g.text("UPDATES", Rect::new(x, y, 70.0, 16.0), f(10.5).b(), TX4);
    g.text(&update_hint(on), Rect::new(x + 76.0, y, w - 76.0, 16.0), f(11.0), TX3);
    let running = crate::update::manual_status().is_some_and(|s| s.2);
    let br = Rect::new(x, y + 22.0, 150.0, 28.0);
    let live = on && !running;
    button(g, br, "Check for updates now", live && app.hover == Some(Hit::Setup(U_CHECK)), live);
    if live {
        app.hits.push((br, Hit::Setup(U_CHECK)));
    }
    if let Some((text, ok, _)) = crate::update::manual_status() {
        g.text(&text, Rect::new(br.r() + 12.0, y + 22.0, (w - br.w - 12.0).max(40.0), 28.0), f(11.5), if ok { TX2 } else { HOT_TX });
    }
}

fn update_hint(on: bool) -> String {
    if on {
        "re-reads the update source; retries a build that was rolled back".into()
    } else {
        "off: this copy is not an installed copy with an update source".into()
    }
}

/// Settings "CPU" block: the CCD preset opt-in (the same switch as the Rules tab).
fn cpu_block(app: &mut App, g: &mut Gfx, x: f32, y: f32, w: f32) {
    g.fill(Rect::new(x, y - 8.0, w, 1.0), SEP);
    g.text("CPU", Rect::new(x, y, 30.0, 16.0), f(10.5).b(), TX4);
    g.text("process menu: CPU affinity… and CPU sets… are always there", Rect::new(x + 34.0, y, w - 34.0, 16.0), f(11.0), TX3);
    let on = crate::features::get().map(|f| f.rules.settings().ccd_presets_enabled).unwrap_or(false);
    let ccds = crate::rules::cpu::ccds().len();
    let sub = if ccds >= 2 {
        format!("One-click Always CPU sets / affinity per CCD ({} groups here). Applies nothing by itself.", ccds)
    } else {
        "This PC has fewer than two L3 cache groups, so no presets would be offered.".to_string()
    };
    option(app, g, x - 10.0, y + 18.0, w + 20.0, T_CCD, "CCD presets in the process menu", &sub, on, true);
}

/// Settings "Taskbar" block: the Jump List switch and the Nilesoft Shell snippet.
fn taskbar_block(app: &mut App, g: &mut Gfx, x: f32, y: f32, w: f32) {
    let ok = crate::taskbar::allowed();
    let on = ok && crate::taskbar::enabled();
    g.fill(Rect::new(x, y - 8.0, w, 1.0), SEP);
    g.text("TASKBAR", Rect::new(x, y, 60.0, 16.0), f(10.5).b(), TX4);
    g.text("right-click Hayate's icon in the taskbar", Rect::new(x + 66.0, y, w - 66.0, 16.0), f(11.0), TX3);
    let sub = if ok {
        "Main Hayate icon only (not the admin / no-admin shortcuts). UAC only for Restart as administrator."
    } else {
        "Off in a portable copy: its entries would point at this folder. Install Hayate to use it."
    };
    option(app, g, x - 10.0, y + 18.0, w + 20.0, T_JUMP, "Taskbar icon menu", sub, on, ok);
    let ny = y + 70.0;
    g.text("Empty-taskbar menu (Nilesoft Shell)", Rect::new(x, ny, w, 18.0), f(12.5), if ok { TX1 } else { TX3 });
    g.text("Windows cannot extend that menu; Nilesoft Shell can. Hayate never edits shell.nss.", Rect::new(x, ny + 18.0, w, 16.0), f(11.0), TX3);
    let by = ny + 40.0;
    let mut bx = x;
    for (id, label, bw) in [(N_COPY, "Copy snippet", 104.0), (N_SAVE, "Save snippet file", 128.0)] {
        let br = Rect::new(bx, by, bw, 26.0);
        button(g, br, label, ok && app.hover == Some(Hit::Setup(id)), ok);
        if ok {
            app.hits.push((br, Hit::Setup(id)));
        }
        bx += bw + 8.0;
    }
    if let Some((text, bad)) = crate::taskbar::note() {
        // an actionable error is long: wrap it, never clip the way out
        for (n, line) in super::cpu::wrap(g, &text, w, f(11.0), 4).iter().enumerate() {
            g.text(line, Rect::new(x, by + 32.0 + 15.0 * n as f32, w, 16.0), f(11.0), if bad { HOT_TX } else { TX2 });
        }
    }
}

/// Settings "Task Manager" block: the opt-in redirect (src/taskmgr). Default off.
struct TmLayout {
    ok: bool,
    st: crate::taskmgr::logic::Status,
    /// the status text and the recovery hint, wrapped to the block's width
    lines: Vec<String>,
    hint: Vec<String>,
    /// "launcher is older than this Blink" + the Refresh button (wrapped text; empty: not offered)
    refresh: Vec<String>,
    offer: crate::taskmgr::Offer,
    /// real height of the block (what the next block starts after)
    h: f32,
}

const TM_LINE_H: f32 = 15.0;
const TM_HINT_H: f32 = 13.0;
const ERR_LINE_H: f32 = 15.0;
/// room for an error text under Settings: one line, up to three for a long one (~95 characters a line)
fn err_h(e: &str) -> f32 {
    let lines = e.chars().count().div_ceil(95).clamp(1, 3) as f32;
    22.0 + ERR_LINE_H * (lines - 1.0)
}
/// the Refresh launcher button row (28 high) and the gap above it
const TM_REFRESH_BTN_H: f32 = 28.0;

fn tm_layout(g: &mut Gfx, w: f32) -> TmLayout {
    use crate::taskmgr::logic::Status;
    let ok = crate::taskmgr::allowed();
    let st = if ok { crate::taskmgr::ui_status() } else { Status::Off };
    let line = if ok { crate::taskmgr::describe(&st) } else { "Off in a portable copy.".into() };
    let lines = super::cpu::wrap(g, &line, w, f(11.0), 8); // the longest status (PendingRemoval, ~5 lines at 300 px) must keep its actionable tail
    let hint = super::cpu::wrap(g, crate::taskmgr::logic::RECOVERY_HINT, w, f(9.5), 4);
    // the launcher is an older build than this Blink: say so, once, with the way out (never a pop-up)
    let offer = if ok { crate::taskmgr::offer(&st) } else { crate::taskmgr::Offer::None };
    let refresh = if offer == crate::taskmgr::Offer::None { Vec::new() } else { super::cpu::wrap(g, crate::taskmgr::offer_text(offer), w, f(11.0), 5) };
    let btn_h = if crate::taskmgr::offer_button(offer).is_some() { 4.0 + TM_REFRESH_BTN_H } else { 0.0 };
    let refresh_h = if refresh.is_empty() { 0.0 } else { 6.0 + TM_LINE_H * refresh.len() as f32 + btn_h };
    // 70: the toggle row above; 6 gap; 18: room before the next block's rule
    let h = 70.0 + TM_LINE_H * lines.len() as f32 + refresh_h + 6.0 + TM_HINT_H * hint.len() as f32 + 18.0;
    TmLayout { ok, st, lines, hint, refresh, offer, h }
}

fn taskmgr_block(app: &mut App, g: &mut Gfx, x: f32, y: f32, w: f32, l: &TmLayout) {
    use crate::taskmgr::logic::Status;
    let (ok, st) = (l.ok, &l.st);
    let on = matches!(st, Status::On | Status::Stale(_));
    let usable = ok && !matches!(st, Status::Foreign(_));
    g.fill(Rect::new(x, y - 8.0, w, 1.0), SEP);
    g.text("TASK MANAGER", Rect::new(x, y, 100.0, 16.0), f(10.5).b(), TX4);
    g.text("Ctrl+Shift+Esc, Win+X, taskbar menu, Run", Rect::new(x + 106.0, y, w - 106.0, 16.0), f(11.0), TX3);
    let sub = if ok { "Asks for administrator rights once. Every user on this PC. A Hayate started this way runs as administrator." } else { "Install Hayate first: it needs a protected copy in Program Files." };
    option(app, g, x - 10.0, y + 18.0, w + 20.0, T_TM, "Open Hayate instead of Task Manager", sub, on, usable);
    let bad = matches!(st, Status::Stale(_));
    for (n, t) in l.lines.iter().enumerate() {
        g.text(t, Rect::new(x, y + 70.0 + TM_LINE_H * n as f32, w, 16.0), f(11.0), if bad { HOT_TX } else { TX2 });
    }
    let mut ry = y + 70.0 + TM_LINE_H * l.lines.len() as f32;
    if !l.refresh.is_empty() {
        ry += 6.0;
        for (n, t) in l.refresh.iter().enumerate() {
            g.text(t, Rect::new(x, ry + TM_LINE_H * n as f32, w, 16.0), f(11.0), WARN_TX);
        }
        ry += TM_LINE_H * l.refresh.len() as f32;
        if let Some(label) = crate::taskmgr::offer_button(l.offer) {
            let br = Rect::new(x, ry + 4.0, g.measure(label, f(12.0)) + 28.0, TM_REFRESH_BTN_H);
            let live = !crate::taskmgr::busy();
            button(g, br, label, live && app.hover == Some(Hit::Setup(T_TM_REFRESH)), live);
            if live {
                app.hits.push((br, Hit::Setup(T_TM_REFRESH)));
            }
            ry = br.b();
        }
    }
    let hy = ry + 6.0;
    for (n, t) in l.hint.iter().enumerate() {
        g.text(t, Rect::new(x, hy + TM_HINT_H * n as f32, w, 14.0), f(9.5), TX4);
    }
}

/// Settings "Leak alerts" block, laid out before painting (the panel grows with it).
struct LeakLayout {
    /// the user's names and their pills (relative to the block's top-left)
    user: Vec<(String, Rect)>,
    /// built-in names wrapped to lines (empty while collapsed)
    builtin: Vec<String>,
    h: f32,
}

const PILL_H: f32 = 24.0;

fn leak_layout(g: &mut Gfx, w: f32, builtin_open: bool) -> LeakLayout {
    let mut y = 22.0;
    let mut x = 0.0;
    let mut user = Vec::new();
    for n in crate::leak::user_ignored() {
        let pw = (g.measure(&n, f(12.0)) + 12.0 + 22.0).min(w);
        if x > 0.0 && x + pw > w {
            x = 0.0;
            y += PILL_H + 6.0;
        }
        user.push((n, Rect::new(x, y, pw, PILL_H)));
        x += pw + 6.0;
    }
    y += if user.is_empty() { 18.0 } else { PILL_H } + 8.0;
    let mut builtin = Vec::new();
    if builtin_open {
        let mut line = String::new();
        for n in crate::leak::builtin_ignored() {
            let next = if line.is_empty() { n.to_string() } else { format!("{line}, {n}") };
            if !line.is_empty() && g.measure(&next, f(11.0)) > w - 14.0 {
                builtin.push(format!("{line},"));
                line = n.to_string();
            } else {
                line = next;
            }
        }
        if !line.is_empty() {
            builtin.push(line);
        }
    }
    LeakLayout { user, h: y + 22.0 + builtin.len() as f32 * 16.0, builtin }
}

fn leak_paint(app: &mut App, g: &mut Gfx, x: f32, y: f32, w: f32, l: &LeakLayout) {
    g.fill(Rect::new(x, y - 8.0, w, 1.0), SEP);
    g.text("LEAK ALERTS", Rect::new(x, y, w, 16.0), f(10.5).b(), TX4);
    g.text("apps Hayate never reports as leaking memory", Rect::new(x + 86.0, y, w - 86.0, 16.0), f(11.0), TX3);
    if l.user.is_empty() {
        g.text("None of yours yet. Ignore an app from a \u{201c}Leak suspected\u{201d} row in Watch, or in History.", Rect::new(x, y + 18.0, w, 22.0), f(11.5), TX3);
    }
    for (k, (n, pr)) in l.user.iter().enumerate() {
        let r = Rect::new(x + pr.x, y + pr.y, pr.w, pr.h);
        let xr = Rect::new(r.r() - 22.0, r.y, 22.0, r.h);
        let hov = app.hover == Some(Hit::Setup(S_UNIGNORE + k));
        g.round(r, 5.0, PAN2);
        g.round_stroke(r, 5.0, if hov { HOT_LN } else { LN }, 1.0);
        g.text(n, Rect::new(r.x + 9.0, r.y, r.w - 31.0, r.h), f(12.0), TX1);
        g.text("\u{E711}", xr, f(9.0).icon().c(), if hov { HOT_TX } else { TX3 });
        app.hits.push((xr, Hit::Setup(S_UNIGNORE + k)));
    }
    let ty = y + l.h - 22.0 - l.builtin.len() as f32 * 16.0;
    let n = crate::leak::builtin_ignored().len();
    let arrow = if app.setup.builtin_open { "\u{25BE}" } else { "\u{25B8}" };
    let label = format!("{arrow}  Built-in ({n}): system, runtimes and compilers whose memory grows by design");
    let tr = Rect::new(x, ty, w, 22.0);
    g.text(&label, tr, f(11.5), if app.hover == Some(Hit::Setup(T_BUILTIN)) { TX1 } else { TX3 });
    app.hits.push((tr, Hit::Setup(T_BUILTIN)));
    for (k, line) in l.builtin.iter().enumerate() {
        g.text(line, Rect::new(x + 14.0, ty + 22.0 + k as f32 * 16.0, w - 14.0, 16.0), f(11.0), TX3);
    }
}

/// Wheel over the open Settings panel (short windows scroll it).
pub fn wheel(app: &mut App, delta: f32) -> bool {
    if app.setup.panel == Some(Panel::Settings) {
        app.setup.scroll -= delta * 0.5;
        true
    } else {
        false
    }
}

pub fn close(app: &mut App) {
    crate::taskbar::clear_note();
    app.setup.scroll = 0.0;
    app.setup.panel = None;
    app.setup.confirm = false;
    app.setup.error = None;
}

pub fn click(app: &mut App, h: Hit) -> Action {
    let Hit::Setup(id) = h else { return Action::None };
    let s = &mut app.setup;
    match id {
        B_INSTALL | S_INSTALL => {
            s.error = None;
            s.panel = Some(Panel::Install);
        }
        B_NOT_NOW => {
            s.bar = false;
            crate::setup::not_now();
        }
        B_NEVER => {
            s.bar = false;
            crate::setup::never_ask();
        }
        GEAR => {
            app.cpu = None;
            app.disallow = None;
            let s = &mut app.setup;
            if s.panel == Some(Panel::Settings) {
                close(app);
            } else {
                close(app);
                app.setup.panel = Some(Panel::Settings);
            }
        }
        BACKDROP | P_CANCEL | S_CLOSE => {
            if s.confirm && id == S_CLOSE {
                s.confirm = false;
            } else {
                close(app);
            }
        }
        T_ADMIN => s.choice.admin_default = !s.choice.admin_default,
        T_AUTO => s.choice.autostart = !s.choice.autostart,
        T_UPDATE => s.choice.update = !s.choice.update,
        T_REPLACE => s.choice.replace_user = !s.choice.replace_user,
        P_INSTALL_ALL => return Action::InstallAll(s.choice),
        T_DATA => s.remove_data = !s.remove_data,
        T_BUILTIN => s.builtin_open = !s.builtin_open,
        T_JUMP => {
            // a portable copy writes nothing outside itself: the switch is dead there
            if crate::taskbar::allowed() {
                crate::setup::set_jumplist(!crate::taskbar::enabled());
                crate::taskbar::changed();
            }
        }
        N_COPY => match crate::taskbar::snippet() {
            Ok(t) => {
                crate::taskbar::set_note_text("Copied. Save it as a .nss file and import it from shell.nss.".into(), false);
                return Action::Clip(t);
            }
            Err(e) => crate::taskbar::set_note_text(e, true),
        },
        N_SAVE => crate::taskbar::save_snippet(),
        T_TM => {
            // one UAC prompt, then Blink's elevated helper does it all (src/taskmgr)
            use crate::taskmgr::logic::Status;
            if crate::taskmgr::allowed() && !crate::taskmgr::busy() {
                match crate::taskmgr::ui_status() {
                    Status::Foreign(_) => {}
                    st => return Action::TaskMgr(!matches!(st, Status::On | Status::Stale(_))),
                }
            }
        }
        T_TM_REFRESH => {
            // the same helper path as the switch: one UAC prompt (none when this Blink is elevated)
            if crate::taskmgr::allowed() && !crate::taskmgr::busy() && matches!(crate::taskmgr::offer(&crate::taskmgr::ui_status()), crate::taskmgr::Offer::Refresh | crate::taskmgr::Offer::Repair) {
                return Action::TaskMgrRefresh;
            }
        }
        U_CHECK => crate::update::check_now(),
        Z_DEC => return Action::Zoom(crate::zoom::Cmd::Out),
        Z_INC => return Action::Zoom(crate::zoom::Cmd::In),
        Z_RESET => return Action::Zoom(crate::zoom::Cmd::Reset),
        T_CCD => {
            // only the opt-in changes: nothing is applied or saved to any rule
            if let Some(ft) = crate::features::get() {
                let mut st = ft.rules.settings();
                st.ccd_presets_enabled = !st.ccd_presets_enabled;
                ft.rules.set_settings(st);
            }
        }
        _ if id >= S_UNIGNORE => {
            if let Some(n) = crate::leak::user_ignored().get(id - S_UNIGNORE) {
                crate::leak::set_user_ignored(n, false);
            }
        }
        S_UNINSTALL => s.confirm = true,
        S_UNINSTALL_ALL => return Action::UninstallAll,
        P_INSTALL => return Action::Install(s.choice),
        S_CONFIRM => return Action::Uninstall(s.remove_data),
        _ => return Action::None,
    }
    Action::Repaint
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_block_hint_says_why_the_button_is_off() {
        assert!(update_hint(true).contains("rolled back"));
        assert!(update_hint(false).starts_with("off:"));
    }
}
