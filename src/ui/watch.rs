//! Watch tab: services/drivers installed/removed/changed, new/updated/unsigned
//! apps, camera/mic/location use. Feed grouped by hour (repeats of one event
//! collapse into a ×N row); alert routing and health on the right.

use super::*;
use crate::features;
use crate::watch::{AlertType, Event, Filter, Kind, SigState};

pub const CHIPS: [&str; 9] = ["All", "Unread", "Services", "Drivers", "Apps", "Unsigned", "Privacy", "Memory", "Network"];

const ROW_H: f32 = 46.0;
const SEL_H: f32 = 34.0;
const BAND_H: f32 = 28.0;
/// Hit::Button id of the "Ignore leaks" button on the selected row (plus its event index).
const IGNORE_BTN: usize = 1000;

#[derive(Default)]
pub struct WatchView {
    pub chip: usize,
    pub sel: Option<u64>,
    pub scroll: f32,
    /// keyboard: event ids of the feed rows of the last paint, in display order
    pub nav: super::nav::ListNav<u64>,
    widths: Widths,
    /// why Windows would hold toasts back now (None: it shows them); read on the I/O worker
    notif: crate::bgio::Cell<Option<String>>,
}

fn filter(chip: usize, text: &str) -> Filter {
    let mut f = Filter { text: text.to_string(), ..Default::default() };
    match chip {
        1 => f.unread_only = true,
        2 => f.kinds = vec![Kind::ServiceInstalled, Kind::ServiceRemoved, Kind::ServiceStarted, Kind::ServiceStopped, Kind::StartTypeChanged, Kind::ServiceCrashed],
        3 => f.kinds = vec![Kind::DriverInstalled, Kind::DriverRemoved],
        4 => f.kinds = vec![Kind::NewApp, Kind::AppUpdated],
        5 => f.unsigned_only = true,
        6 => f.kinds = vec![Kind::Camera, Kind::Microphone, Kind::Location],
        7 => f.kinds = vec![Kind::MemoryLeak, Kind::MemoryPressure],
        8 => f.kinds = vec![Kind::Network],
        _ => {}
    }
    f
}

/// The feed as shown: filtered, newest first by event time (the store keeps
/// insertion order, which a late catch-up can put out of time order).
fn list(app: &App) -> Vec<Event> {
    let Some(ft) = features::get() else { return Vec::new() };
    let mut v = ft.watch.events(&filter(app.watch.chip, &app.search));
    v.sort_by_key(|e| std::cmp::Reverse(e.time));
    v
}

/// Kind glyph, its colour, and the severity rail (warn / hot / privacy kinds only).
fn kind_style(k: Kind) -> (&'static str, u32, Option<u32>) {
    match k {
        Kind::ServiceInstalled => ("\u{E710}", ACC, None),
        Kind::DriverInstalled => ("\u{E950}", ACC, None),
        Kind::ServiceRemoved | Kind::DriverRemoved => ("\u{E74D}", WARN, Some(WARN)),
        Kind::ServiceStarted => ("\u{E768}", TX3, None),
        Kind::ServiceStopped => ("\u{E71A}", TX3, None),
        Kind::StartTypeChanged => ("\u{E8AB}", WARN, Some(WARN)),
        Kind::ServiceCrashed => ("\u{E7BA}", HOT, Some(HOT)),
        Kind::NewApp => ("\u{E768}", TX3, None),
        Kind::AppUpdated => ("\u{E895}", OK, None),
        Kind::Camera => ("\u{E714}", PRIV, Some(PRIV)),
        Kind::Microphone => ("\u{E720}", PRIV, Some(PRIV)),
        Kind::Location => ("\u{E81D}", PRIV, Some(PRIV)),
        Kind::MemoryLeak => ("\u{E9D9}", WARN, Some(WARN)),
        Kind::MemoryPressure => ("\u{E7BA}", HOT, Some(HOT)),
        Kind::Network => ("\u{E839}", WARN, Some(WARN)),
    }
}

/// Short kind label shown after the name.
fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::ServiceInstalled => "Service installed",
        Kind::DriverInstalled => "Driver installed",
        Kind::ServiceRemoved => "Service removed",
        Kind::DriverRemoved => "Driver removed",
        Kind::ServiceStarted => "Started",
        Kind::ServiceStopped => "Stopped",
        Kind::StartTypeChanged => "Start type changed",
        Kind::ServiceCrashed => "Service crashed",
        Kind::NewApp => "New app",
        Kind::AppUpdated => "App updated",
        Kind::Camera => "Camera in use",
        Kind::Microphone => "Microphone in use",
        Kind::Location => "Location in use",
        Kind::MemoryLeak => "Leak suspected",
        Kind::MemoryPressure => "Memory almost full",
        Kind::Network => "Connections failing",
    }
}

/// Events about one app/service binary show its icon. Drivers (.sys: no icon),
/// removed services (binary often gone) and system-wide memory pressure keep
/// the event-type glyph.
fn has_app_icon(k: Kind) -> bool {
    !matches!(k, Kind::DriverInstalled | Kind::DriverRemoved | Kind::ServiceRemoved | Kind::MemoryPressure)
}

/// Local hour bucket (hours since the epoch, local time).
fn hour_of(t: u64, tz: i64) -> i64 {
    (t as i64 + tz).div_euclid(3600)
}

/// "Today · Mon 16 Nov", "Yesterday · Sun 15 Nov", "3 days ago · Fri 13 Nov".
fn day_label(t: u64, now: u64, tz: i64) -> String {
    let de = (t as i64 + tz).div_euclid(86400);
    let rel = match (now as i64 + tz).div_euclid(86400) - de {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        n => format!("{n} days ago"),
    };
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MO: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (_, m, d) = civil(de);
    format!("{rel} · {} {d} {}", WD[de.rem_euclid(7) as usize], MO[(m - 1) as usize])
}

/// Days since 1970-01-01 → (year, month 1-12, day). Howard Hinnant's algorithm.
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

pub(crate) fn clock(t: u64) -> String {
    let s = (t as i64 + local_offset()).rem_euclid(86400);
    format!("{:02}:{:02}", s / 3600, s / 60 % 60)
}

fn local_offset() -> i64 {
    use windows::Win32::System::Time::*;
    unsafe {
        let mut tz = TIME_ZONE_INFORMATION::default();
        let r = GetTimeZoneInformation(&mut tz);
        let bias = tz.Bias + if r == 2 { tz.DaylightBias } else { 0 };
        -(bias as i64) * 60
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn unread() -> usize {
    features::get().map(|f| f.watch.unread()).unwrap_or(0)
}

/// How many events from `i` on repeat it (same kind and name, same hour): they show as one ×N row.
fn run_len(evs: &[Event], i: usize, tz: i64) -> usize {
    let e = &evs[i];
    let h = hour_of(e.time, tz);
    evs[i..].iter().take_while(|x| x.kind == e.kind && x.name == e.name && hour_of(x.time, tz) == h).count()
}

enum Item {
    /// hour bucket, events, unread, day label on the first band of a day
    Band(i64, usize, usize, Option<String>),
    /// first event index, run length
    Row(usize, usize),
}

pub fn paint(app: &mut App, g: &mut Gfx, body: Rect) {
    let area = body.inset(12.0, 10.0);
    let Some(ft) = features::get() else {
        g.text("Watch not started", area, f(13.0).c(), TX3);
        return;
    };
    let side_w = if area.w > 760.0 { 252.0 } else { 0.0 };
    let main = Rect::new(area.x, area.y, area.w - if side_w > 0.0 { side_w + 10.0 } else { 0.0 }, area.h);
    let evs = list(app);
    // feed_paint records the rows; an empty or hidden feed has none
    let page = app.watch.nav.page;
    app.watch.nav.begin(page);

    // ---- toolbar: chips, Mark all read
    let mr = Rect::new(main.r() - 110.0, main.y, 110.0, 24.0);
    button(g, mr, "Mark all read", app.hover == Some(Hit::Button(1)), true);
    app.hits.push((mr, Hit::Button(1)));
    let mut x = main.x;
    let n_unread = ft.watch.unread();
    for (i, c) in CHIPS.iter().enumerate() {
        let on = app.watch.chip == i;
        // counts that cost nothing: the active chip's and the unread total
        let count = if on { Some(evs.len()) } else if i == 1 && n_unread > 0 { Some(n_unread) } else { None };
        let w = chip_w(&mut app.watch.widths, g, c, count);
        if x + w > mr.x - 8.0 {
            break;
        }
        let r = chip(&mut app.watch.widths, g, x, main.y, c, count, on, app.hover == Some(Hit::Chip(i)));
        app.hits.push((r, Hit::Chip(i)));
        x += w + 4.0;
    }

    // ---- feed
    let feed = Rect::new(main.x, main.y + 34.0, main.w, main.b() - main.y - 34.0);
    if feed.h > 40.0 {
        panel(g, feed);
        let view = Rect::new(feed.x + 1.0, feed.y + 1.0, feed.w - 2.0, feed.h - 5.0);
        if evs.is_empty() {
            let st = ft.watch.status();
            let msg = if let Some(e) = st.eventlog_error { format!("Event log: {e}") } else { "No events yet. Service installs, driver changes, new/updated apps and camera/mic use show here.".to_string() };
            g.text(&msg, Rect::new(view.x, view.y + 20.0, view.w, 40.0), f(12.0).c(), TX3);
        } else {
            feed_paint(app, g, view, &evs);
        }
    }

    if side_w > 0.0 {
        side_paint(app, g, Rect::new(main.r() + 10.0, area.y, side_w, area.h));
    }
}

fn feed_paint(app: &mut App, g: &mut Gfx, view: Rect, evs: &[Event]) {
    let tz = local_offset();
    let now = now_unix();
    let now_h = hour_of(now, tz);
    // layout pass: hour bands (with their counts) and collapsed rows
    let mut items: Vec<Item> = Vec::new();
    let mut last_day = i64::MIN;
    let mut i = 0;
    while i < evs.len() {
        let h = hour_of(evs[i].time, tz);
        let end = i + evs[i..].iter().take_while(|e| hour_of(e.time, tz) == h).count();
        let unread = evs[i..end].iter().filter(|e| e.unread).count();
        let day = h.div_euclid(24);
        let day_lbl = (day != last_day).then(|| day_label(evs[i].time, now, tz));
        last_day = day;
        items.push(Item::Band(h, end - i, unread, day_lbl));
        let mut j = i;
        while j < end {
            let n = run_len(evs, j, tz);
            items.push(Item::Row(j, n));
            j += n;
        }
        i = end;
    }
    let sel_id = app.watch.sel;
    let height = |it: &Item| match it {
        Item::Band(..) => BAND_H,
        Item::Row(j, _) => ROW_H + if sel_id == Some(evs[*j].id) { SEL_H } else { 0.0 },
    };
    let total: f32 = items.iter().map(height).sum();
    // keyboard: record the row order; a key's new selection scrolls into view
    app.watch.nav.page = super::nav::page_rows(view.h, ROW_H);
    let (mut top, mut sel_top) = (0.0f32, None);
    for it in &items {
        let h = height(it);
        if let Item::Row(j, _) = it {
            let s = sel_id == Some(evs[*j].id);
            if s {
                sel_top = Some((top, h));
            }
            app.watch.nav.push(evs[*j].id, s);
        }
        top += h;
    }
    if let Some((t, h)) = sel_top.filter(|_| app.watch.nav.take_reveal()) {
        app.watch.scroll = super::nav::reveal(app.watch.scroll, t, h, view.h);
    }
    app.watch.scroll = app.watch.scroll.clamp(0.0, (total - view.h).max(0.0));
    // the scroll bar takes its gutter from the rows only while the feed overflows
    let gut = super::nav::gutter(total, view.h);
    g.clip(view);
    let mut y = view.y - app.watch.scroll;
    let mut first = true;
    for it in &items {
        let h = height(it);
        if y + h >= view.y && y <= view.b() {
            let r = Rect::new(view.x, y, view.w - gut, h);
            match it {
                Item::Band(hr, n, unread, day) => {
                    g.fill(r, PAN2);
                    if !first {
                        g.fill(Rect::new(r.x, r.y, r.w, 1.0), LN);
                    }
                    g.fill(Rect::new(r.x, r.b() - 1.0, r.w, 1.0), SEP);
                    let hh = hr.rem_euclid(24);
                    let label = if *hr == now_h { format!("{hh:02}:00 – now") } else { format!("{hh:02}:00") };
                    let lw = app.watch.widths.get(g, &label, f(11.0).b());
                    g.text(&label, Rect::new(r.x + 14.0, r.y, lw + 2.0, r.h), f(11.0).b(), TX2);
                    let mut c = if *n == 1 { "1 event".to_string() } else { format!("{n} events") };
                    if *unread > 0 {
                        c.push_str(&format!(" · {unread} unread"));
                    }
                    g.text(&c, Rect::new(r.x + 14.0 + lw + 8.0, r.y, 220.0, r.h), f(11.0).m(), TX3);
                    if let Some(d) = day {
                        g.text(d, Rect::new(r.r() - 252.0, r.y, 240.0, r.h), f(11.5).r(), TX3);
                    }
                }
                Item::Row(j, n) => {
                    let row = Rect::new(r.x, r.y, r.w, ROW_H);
                    event_row(app, g, row, &evs[*j..*j + *n], *j);
                }
            }
        }
        first = false;
        y += h;
    }
    g.unclip();
    let sc = app.watch.scroll;
    super::nav::paint_bar(app, g, super::nav::BAR_WATCH, view, total, sc);
}

/// One feed row: `run` is the event plus its repeats (first is the newest).
fn event_row(app: &mut App, g: &mut Gfx, r: Rect, run: &[Event], i: usize) {
    let e = &run[0];
    let unread = run.iter().any(|x| x.unread);
    let sel = app.watch.sel == Some(e.id);
    let hov = app.hover == Some(Hit::Event(i));
    let full = Rect::new(r.x, r.y, r.w, r.h + if sel { SEL_H } else { 0.0 });
    let bg = if sel {
        ACC_BG
    } else if hov {
        PAN2
    } else {
        PAN
    };
    if bg != PAN {
        g.fill(full, bg);
    }
    g.fill(Rect::new(full.x, full.b() - 1.0, full.w, 1.0), SEP);
    app.hits.push((full, Hit::Event(i)));
    let k = e.kind();
    let (glyph, col, sev) = kind_style(k);
    // a leak row for an app now on the ignore list reads as settled: no rail, dim text
    let ignored = k == Kind::MemoryLeak && crate::leak::is_ignored(&e.name);
    let sev = if ignored { None } else { sev };
    if let Some(c) = sev {
        rail(g, r, c);
    } else if sel {
        g.fill(Rect::new(full.x, full.y, 2.0, full.h), ACC);
    }
    if unread {
        g.circle(r.x + 10.0, r.y + r.h / 2.0, 3.0, ACC);
    }

    // icon tile: the app's icon with the kind glyph on its corner, or the glyph alone
    let tile = Rect::new(r.x + 22.0, r.y + 8.0, 30.0, 30.0);
    if unread {
        g.round(tile, 7.0, if bg == PAN { PAN2 } else { PAN3 });
    } else {
        g.round_stroke(tile, 7.0, if bg == PAN { SEP } else { LN }, 1.0);
    }
    let icon = has_app_icon(k) && super::icons::draw(&mut app.icons, g, &e.path, 0, Rect::new(tile.x + 5.0, tile.y + 5.0, 20.0, 20.0));
    if icon {
        let (cx, cy) = (tile.r() - 3.5, tile.b() - 3.5);
        g.circle(cx, cy, 7.5, LN);
        g.circle(cx, cy, 6.5, PAN);
        g.text(glyph, Rect::new(cx - 7.0, cy - 7.0, 14.0, 14.0), f(7.5).icon().c(), col);
    } else {
        g.text(glyph, tile, f(14.0).icon().c(), col);
    }

    // right column: time over the signer badge
    let meta = if sel { TX2 } else { TX3 };
    let right = r.r() - 12.0;
    g.text(&clock(e.time), Rect::new(right - 80.0, r.y + 5.0, 80.0, 18.0), f(11.5).m().r(), meta);
    let badge_x = sig_badge(app, g, e, right, r.y + 24.0, sel);
    let text_r = (right - 90.0).min(badge_x - 12.0);

    // line 1: name, kind, ×N
    let tx = tile.r() + 12.0;
    let avail = (text_r - tx).max(40.0);
    let wd = &mut app.watch.widths;
    let nf = if unread { f(13.0).b() } else { f(13.0) };
    let kl = if ignored { "Leak suspected · ignored" } else { kind_label(k) };
    let kw = wd.get(g, kl, f(11.5));
    let xn = (run.len() > 1).then(|| format!("×{}", run.len()));
    let xw = xn.as_ref().map(|s| wd.get(g, s, f(10.5).b().m()) + 10.0).unwrap_or(0.0);
    let nw = wd.get(g, &e.name, nf).min((avail - kw - xw - 16.0).max(avail * 0.45));
    let l1 = Rect::new(tx, r.y + 5.0, nw + 1.0, 20.0);
    g.text(&e.name, l1, nf, if ignored { TX3 } else if unread { TX1 } else { TX2 });
    let mut x = tx + nw + 8.0;
    let kr = Rect::new(x, l1.y + 1.0, (tx + avail - x).max(0.0).min(kw + 1.0), 20.0);
    g.text(kl, kr, f(11.5), meta);
    x += kw + 8.0;
    if let Some(s) = xn {
        if x + xw <= tx + avail {
            let br = Rect::new(x, l1.y + 3.0, xw, 16.0);
            g.round(br, 4.0, if bg == PAN { PAN3 } else { LN });
            g.text(&s, br, f(10.5).b().m().c(), TX2);
        }
    }

    // line 2: who, then what changed (in TX2: the part people read)
    let l2y = r.y + 24.0;
    let mut x = tx;
    let end = tx + avail;
    let who = if e.actor.is_empty() { String::new() } else { format!("by {}{}", e.actor, if e.actor_likely { " (likely)" } else { "" }) };
    let change = matches!(k, Kind::StartTypeChanged | Kind::AppUpdated);
    if !who.is_empty() {
        let s = if e.detail.is_empty() { who } else { format!("{who} · ") };
        let w = wd.get(g, &s, f(11.5));
        g.text(&s, Rect::new(x, l2y, (end - x).max(0.0).min(w + 1.0), 18.0), f(11.5), meta);
        x += w;
    }
    if !e.detail.is_empty() && x < end - 20.0 {
        g.text(&e.detail, Rect::new(x, l2y, end - x, 18.0), f(11.5), if change || sel { TX2 } else { meta });
    } else if e.actor.is_empty() && e.detail.is_empty() {
        g.text(&e.path, Rect::new(x, l2y, end - x, 18.0), f(11.5), meta);
    }

    if sel {
        let mut dr = Rect::new(tx, r.b() - 4.0, r.r() - tx - 12.0, SEL_H - 4.0);
        // leak row: one click puts the app on (or takes it off) leak-ignore.txt
        if k == Kind::MemoryLeak && !e.name.is_empty() && !crate::leak::is_builtin(&e.name) {
            let label = if ignored { "Stop ignoring leaks".to_string() } else { format!("Ignore leaks for {}", e.name) };
            let bw = (app.watch.widths.get(g, &label, f(12.0)) + 24.0).min(dr.w * 0.5);
            let br = Rect::new(dr.r() - bw, dr.y + 2.0, bw, 24.0);
            button(g, br, &label, app.hover == Some(Hit::Button(IGNORE_BTN + i)), true);
            app.hits.push((br, Hit::Button(IGNORE_BTN + i)));
            dr.w -= bw + 10.0;
        }
        let mut lines = Vec::new();
        if !e.path.is_empty() {
            lines.push(e.path.clone());
        }
        if !e.key.is_empty() && e.key != e.name {
            lines.push(format!("key {}", e.key));
        }
        if run.len() > 1 {
            lines.push(format!("{} times, first {}", run.len(), clock(run[run.len() - 1].time)));
        }
        g.text(&lines.join("   ·   "), dr, f(11.0).m(), TX2);
    }
}

/// Signer badge right-aligned at `right`; returns its left edge. Signer names
/// are plain text (Microsoft quieter than other vendors); only Unsigned and a
/// bad signature get a filled badge.
fn sig_badge(app: &mut App, g: &mut Gfx, e: &Event, right: f32, y: f32, sel: bool) -> f32 {
    let (text, glyph, fill, border, tc) = match e.sig {
        SigState::Signed | SigState::Catalog => {
            let t = if e.publisher.is_empty() { "Signed".to_string() } else { e.publisher.clone() };
            let ms = t.starts_with("Microsoft");
            (t, "\u{EA18}", 0, 0, if ms && !sel { TX3 } else { TX2 })
        }
        SigState::Unsigned => ("Unsigned".to_string(), "\u{E7BA}", WARN_BG, WARN_LN, WARN_TX),
        SigState::Invalid => ("Bad signature".to_string(), "\u{E7BA}", HOT_BG, HOT_LN, HOT_TX),
        SigState::Unknown => (e.publisher.clone(), "", 0, 0, if sel { TX2 } else { TX3 }),
    };
    if text.is_empty() {
        return right;
    }
    let tw = app.watch.widths.get(g, &text, f(11.0)).min(170.0);
    let gw = if glyph.is_empty() { 0.0 } else { 14.0 };
    let w = 6.0 + gw + tw + 6.0;
    let r = Rect::new(right - w, y, w, 18.0);
    if fill != 0 {
        g.round(r, 4.0, fill);
        g.round_stroke(r, 4.0, border, 1.0);
    }
    if !glyph.is_empty() {
        g.text(glyph, Rect::new(r.x + 5.0, r.y, 12.0, r.h), f(9.5).icon().c(), tc);
    }
    g.text(&text, Rect::new(r.x + 6.0 + gw, r.y, tw + 1.0, r.h), f(11.0), tc);
    r.x
}

/// Section of an alert type in the routing panel.
fn alert_section(a: AlertType) -> usize {
    match a {
        AlertType::ServiceInstalled | AlertType::ServiceRemoved | AlertType::StartTypeChanged | AlertType::ServiceCrashed | AlertType::ServiceStartStop => 0,
        AlertType::NewApp | AlertType::AppUpdated | AlertType::Unsigned => 1,
        AlertType::Camera | AlertType::Microphone | AlertType::Location => 2,
        AlertType::MemoryLeak | AlertType::MemoryPressure => 3,
    }
}
const SECTIONS: [&str; 4] = ["SERVICES AND DRIVERS", "APPS", "PRIVACY", "MEMORY"];

fn side_paint(app: &mut App, g: &mut Gfx, r: Rect) {
    let Some(ft) = features::get() else { return };
    // Do not disturb / notifications off: say so, toasts would not pop up
    app.watch.notif.refresh("notification state", std::time::Duration::from_secs(5), || {
        let r = crate::notify::blocked_reason();
        super::icons::poke();
        Some(r)
    });
    let why = app.watch.notif.get().flatten();
    let alerts = ft.watch.alerts();
    let n_hint = why.as_ref().map(|w| w.lines().count()).unwrap_or(0);
    let alerts_h = 32.0 + 6.0 + alerts.len() as f32 * 26.0 + SECTIONS.len() as f32 * 24.0 + 8.0 + 2.0 * 34.0 + if n_hint > 0 { 8.0 + n_hint as f32 * 16.0 } else { 0.0 } + 8.0;
    g.clip(r);

    // ---- alerts panel
    let p = Rect::new(r.x, r.y, r.w, alerts_h);
    panel(g, p);
    g.text("ALERTS", Rect::new(p.x + 12.0, p.y, 80.0, 32.0), f(11.0).b(), TX2);
    g.text("toast = Windows notification", Rect::new(p.x + 80.0, p.y, p.w - 92.0, 32.0), f(11.0).r(), TX3);
    g.fill(Rect::new(p.x + 1.0, p.y + 31.0, p.w - 2.0, 1.0), SEP);
    let mut y = p.y + 32.0 + 6.0;
    let mut sec = usize::MAX;
    for (i, (a, on)) in alerts.into_iter().enumerate() {
        let s = alert_section(a);
        if s != sec {
            sec = s;
            g.text(SECTIONS[s], Rect::new(p.x + 12.0, y + 8.0, p.w - 24.0, 14.0), f(10.5).b(), TX4);
            y += 24.0;
        }
        let rr = Rect::new(p.x + 1.0, y, p.w - 2.0, 26.0);
        if app.hover == Some(Hit::Alert(i)) {
            g.fill(rr, PAN2);
        }
        super::rules::toggle(g, Rect::new(rr.r() - 39.0, rr.y + 5.5, 28.0, 15.0), on, true);
        g.text(a.label(), Rect::new(p.x + 12.0, rr.y, rr.w - 60.0, rr.h), f(12.0), if on { TX2 } else { TX3 });
        app.hits.push((rr, Hit::Alert(i)));
        y += 26.0;
    }
    y += 8.0;
    let dnd = ft.watch.dnd_until();
    let now = now_unix();
    let br = Rect::new(p.x + 12.0, y, p.w - 24.0, 28.0);
    if dnd > now {
        g.round(br, 6.0, WARN_BG);
        g.round_stroke(br, 6.0, WARN_LN, 1.0);
        g.text(&format!("Snoozed · {} min left (click to resume)", (dnd - now).div_ceil(60)), br, f(12.0).c(), WARN_TX);
    } else {
        button(g, br, "Snooze alerts 3 h", app.hover == Some(Hit::Button(2)), true);
    }
    app.hits.push((br, Hit::Button(2)));
    y += 34.0;
    let tr = Rect::new(p.x + 12.0, y, p.w - 24.0, 28.0);
    button(g, tr, "Send test notification", app.hover == Some(Hit::Button(3)), true);
    app.hits.push((tr, Hit::Button(3)));
    y += 34.0;
    if let Some(why) = &why {
        y += 2.0;
        for (k, line) in why.lines().enumerate() {
            g.text(line, Rect::new(p.x + 12.0, y, p.w - 24.0, 16.0), if k == 0 { f(11.5).b() } else { f(11.0) }, if k == 0 { WARN } else { TX3 });
            y += 16.0;
        }
    }

    // ---- health panel
    let st = ft.watch.status();
    let mut rows: Vec<(u32, &str, String)> = Vec::new();
    let state = |e: &Option<String>| match e {
        None => (OK, "OK".to_string()),
        Some(m) => (HOT, m.clone()),
    };
    let (c, v) = state(&st.eventlog_error);
    rows.push((c, "Event log", v));
    let (c, v) = state(&st.scm_notify_error);
    rows.push((c, "SCM notify", v));
    rows.push((if st.consent_keys == 6 { OK } else { WARN }, "Privacy keys", format!("{} / 6", st.consent_keys)));
    rows.push((TX4, "Services cached", st.services_cached.to_string()));
    rows.push((TX4, "Apps seen", st.apps_seen.to_string()));
    let hp = Rect::new(r.x, p.b() + 8.0, r.w, 26.0 + rows.len() as f32 * 22.0 + 8.0);
    if hp.b() > r.b() {
        // short window: one line, a dot per check and the first problem (or "all OK")
        let cp = Rect::new(r.x, p.b() + 6.0, r.w, 28.0);
        if cp.b() <= r.b() {
            panel(g, cp);
            g.text("HEALTH", Rect::new(cp.x + 12.0, cp.y, 60.0, cp.h), f(10.5).b(), TX4);
            for (k, (c, _, _)) in rows.iter().enumerate() {
                g.circle(cp.x + 70.0 + k as f32 * 10.0, cp.y + 14.0, 3.0, *c);
            }
            let bad = rows.iter().find(|(c, _, _)| *c == HOT || *c == WARN);
            let (t, c) = match bad {
                Some((c, label, _)) => (label.to_string(), *c),
                None => ("all OK".to_string(), TX2),
            };
            let x0 = cp.x + 70.0 + rows.len() as f32 * 10.0 + 4.0;
            g.text(&t, Rect::new(x0, cp.y, cp.r() - 12.0 - x0, cp.h), f(11.0).r(), c);
        }
        g.unclip();
        return;
    }
    panel(g, hp);
    g.text("HEALTH", Rect::new(hp.x + 12.0, hp.y + 6.0, hp.w - 24.0, 18.0), f(10.5).b(), TX4);
    let mut y = hp.y + 26.0;
    for (c, label, value) in rows {
        g.circle(hp.x + 15.0, y + 11.0, 3.0, c);
        g.text(label, Rect::new(hp.x + 25.0, y, 110.0, 22.0), f(11.5), TX3);
        g.text(&value, Rect::new(hp.x + 135.0, y, hp.w - 147.0, 22.0), f(11.0).m().r(), if c == HOT { HOT } else { TX2 });
        y += 22.0;
    }
    g.unclip();
}

pub fn click(app: &mut App, h: Hit) -> Action {
    let Some(ft) = features::get() else { return Action::None };
    match h {
        Hit::Chip(i) => {
            app.watch.chip = i;
            app.watch.scroll = 0.0;
        }
        Hit::Button(1) => ft.watch.mark_all_read(),
        Hit::Button(3) => return Action::TestToast,
        Hit::Button(b) if b >= IGNORE_BTN => {
            if let Some(e) = list(app).get(b - IGNORE_BTN) {
                crate::leak::set_user_ignored(&e.name, !crate::leak::is_ignored(&e.name));
            }
        }
        Hit::Button(2) => {
            ft.watch.set_dnd(if ft.watch.dnd_until() > now_unix() { 0 } else { 3 * 3600 });
        }
        Hit::Alert(i) => {
            if let Some((a, on)) = ft.watch.alerts().get(i).copied() {
                ft.watch.set_alert(a, !on);
            }
        }
        Hit::Event(i) => {
            let evs = list(app);
            if let Some(e) = evs.get(i) {
                app.watch.sel = if app.watch.sel == Some(e.id) { None } else { Some(e.id) };
                // a ×N row reads as one: opening it marks every repeat read
                for x in &evs[i..i + run_len(&evs, i, local_offset())] {
                    if x.unread {
                        ft.watch.mark_read(x.id);
                    }
                }
            }
        }
        _ => return Action::None,
    }
    Action::Repaint
}

/// Up / Down / Page / Home / End: select (expand) a feed row by event id. Only
/// looking: unlike a click it marks nothing read; Enter does that.
pub fn nav(app: &mut App, m: super::nav::Move) {
    let sel = app.watch.sel;
    if let Some(id) = app.watch.nav.step(sel.is_some(), |&id| Some(id) == sel, m) {
        app.watch.sel = Some(id);
    }
}

/// Enter: the selected row (and its ×N repeats) marked read, as a click on it does.
pub fn open_selected(app: &mut App) {
    let (Some(ft), Some(id)) = (features::get(), app.watch.sel) else { return };
    let evs = list(app);
    if let Some(i) = evs.iter().position(|e| e.id == id) {
        for x in &evs[i..i + run_len(&evs, i, local_offset())] {
            if x.unread {
                ft.watch.mark_read(x.id);
            }
        }
    }
}

/// Context menu data for an event row.
pub fn event_at(app: &App, i: usize) -> Option<Event> {
    list(app).get(i).cloned()
}
