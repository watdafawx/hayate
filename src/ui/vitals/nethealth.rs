//! Vitals > Network adapter detail: Link use bars and the Network health panel (phase 1: passive
//! counters, phase 2: opt-in probes). Port of the design mock; the data and the verdict come from
//! `sample::nethealth`. Nothing here talks to Windows: it draws what the sampler already read.

use super::*;
use crate::sample::nethealth::{self as nh, Lv};
use crate::sample::probe;
use crate::sample::VNic;

/// Link use block under the throughput graph: rule, title row, Down and Up bars, padding.
pub(super) const LINK_USE_H: f32 = 8.0 + 20.0 + 2.0 * 24.0 + 8.0;
const TILE_H: f32 = 78.0;
const TILE_GAP: f32 = 8.0;
pub(super) const BAND_H: f32 = 28.0;
const CHAIN_W: f32 = 440.0;
const LINE_H: f32 = 18.0;
const HINT: &str = "Enable probes for ping, jitter, loss, DNS";
const CHECKED: &str = "Passive counters only. Ping, jitter, loss and DNS come with probes.";
const FOOT: &str = "WARN / HOT at: errors or discards +1 / +10 per 60 s · signal ≤ −67 / −75 dBm · failed connects, resets and retransmits: against this PC's usual level (median of the last hour): WARN at 3 × usual and at least +10/min (+1.5 points for retransmits), HOT at 8 × usual and +40/min (+6 points). Until 15 minutes of history exist, fixed limits: failed connects 30 / 90 per min · resets 60 / 180 per min · retransmits 1 / 3%.";

// ---------- Link use ----------

/// "122 Mb/s" style rate: whole numbers from 100, one decimal below.
fn mbps(v: f64) -> String {
    format!("{} Mb/s", num(v))
}

/// Share of the link, percent; None without a link speed.
pub(super) fn link_pct(mbps_now: f64, link_bps: u64) -> Option<f64> {
    (link_bps > 0 && mbps_now.is_finite()).then(|| mbps_now * 1e6 / link_bps as f64 * 100.0)
}

/// Down and Up of the selected adapter now against its link speed; the tick is the 60 s peak.
pub(super) fn link_use(app: &mut App, g: &mut Gfx, l: &L, r: Rect, n: &VNic) {
    g.fill(Rect::new(r.x + 1.0, r.y, r.w - 2.0, 1.0), SEP);
    let hx = r.x + 13.0;
    let t = "LINK USE · NOW";
    let tw = app.vit.wd.get(g, t, f(11.0).b());
    g.text(t, Rect::new(hx, r.y + 6.0, tw + 2.0, 20.0), f(11.0).b(), TX2);
    g.text(&format!("{} against its link speed · tick = peak of the last 60 s", n.name), Rect::new(hx + tw + 9.0, r.y + 6.0, (r.w - tw - 40.0).max(0.0), 20.0), f(11.5), TX3);
    let link = if n.link_bps > 0 { link(n.link_bps) } else { "—".into() };
    let (vw, pw) = if l.narrow { (176.0, 96.0) } else { (224.0, 112.0) };
    for (k, (name, color, now, series)) in [("Down", C_NET, n.rx_bps / 1e6, &n.rx), ("Up", NET_UP, n.tx_bps / 1e6, &n.tx)].into_iter().enumerate() {
        let y = r.y + 8.0 + 20.0 + k as f32 * 24.0;
        g.text(name, Rect::new(hx, y, 40.0, 24.0), f(12.0), TX2);
        let (px, vx) = (r.r() - 12.0 - pw, r.r() - 12.0 - pw - 12.0 - vw);
        let tx = hx + 40.0 + 12.0;
        let trk = Rect::new(tx, y + 8.0, (vx - 12.0 - tx).max(0.0), 8.0);
        g.fill(trk, TRACK);
        let peak = vmax(series) as f64;
        let pct = link_pct(now, n.link_bps);
        if let Some(p) = pct {
            if now > 0.0 {
                g.fill(Rect::new(trk.x, trk.y, (trk.w * (p / 100.0).min(1.0) as f32).max(2.0), trk.h), color);
            }
            if let Some(pp) = link_pct(peak, n.link_bps).filter(|_| peak > 0.0) {
                let x = (trk.x + trk.w * (pp / 100.0).min(1.0) as f32 - 1.0).clamp(trk.x, trk.r() - 2.0);
                g.fill(Rect::new(x, trk.y - 3.0, 2.0, trk.h + 6.0), TX2);
            }
        }
        // "122 Mb/s" bold, " of 2.5 Gb/s" dim, right aligned as one run
        let of = format!(" of {link}");
        let ow = app.vit.wd.get(g, &of, f(12.0).m());
        g.text(&of, Rect::new(vx + vw - ow, y, ow + 2.0, 24.0), f(12.0).m(), TX3);
        g.text(&mbps(now), Rect::new(vx, y, (vw - ow).max(0.0), 24.0), f(12.0).m().b().r(), TX1);
        let tail = match pct {
            Some(p) => format!("{p:.1}% · peak {}", num(peak)),
            None => format!("peak {}", num(peak)),
        };
        g.text(&tail, Rect::new(px, y, pw, 24.0), f(11.0).m().r(), TX3);
    }
}

// ---------- tiles ----------

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Tile {
    pub label: &'static str,
    /// "passive" for this adapter, "system" for system-wide TCP counters
    pub tag: &'static str,
    pub value: String,
    pub unit: String,
    pub sub: String,
    pub lv: Lv,
    /// last hour of one-minute samples + the usual level (baseline tiles only)
    pub spark: Option<Spark>,
}

/// The 60-minute sparkline of a baseline tile: oldest first, with the dashed usual line.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Spark {
    pub vals: Vec<f32>,
    pub usual: Option<f32>,
}

fn tile(label: &'static str, tag: &'static str, value: String, unit: &str, sub: String, lv: Lv) -> Tile {
    Tile { label, tag, value, unit: unit.into(), sub, lv, spark: None }
}

fn span_text(s: f64) -> String {
    if s >= 59.0 { "60 s".into() } else { format!("{s:.0} s") }
}

/// The passive tiles of one adapter, in the mock's order. Wi-Fi only for a connected wireless
/// adapter; counters read "—" until the sampler has two reads.
pub(super) fn tiles(n: &VNic, h: &nh::Health) -> Vec<Tile> {
    let a = h.adapters.iter().find(|a| a.luid == n.luid);
    let mut out = Vec::with_capacity(8);
    let flaps = a.map_or(0, |a| a.flaps);
    let up = a.is_none_or(|a| a.up);
    let flap_lv = lvl(flaps as f64, nh::TH_FLAP);
    out.push(tile(
        "Link",
        "passive",
        if up { "Up" } else { "Down" }.into(),
        &if up && n.link_bps > 0 { format!("· {}", link(n.link_bps)) } else { String::new() },
        format!("{} · {} flap{} since start", n.kind, flaps, if flaps == 1 { "" } else { "s" }),
        if up { flap_lv } else { Lv::Hot },
    ));
    if let Some(w) = a.and_then(|a| a.wifi.as_ref()) {
        let band = w.band();
        let ch = if w.channel > 0 { format!(" · ch {}", w.channel) } else { String::new() };
        out.push(tile("Wi-Fi signal", "passive", format!("{}", w.rssi), "dBm", format!("quality {}%{ch}{}", w.quality, if band.is_empty() { String::new() } else { format!(" · {band}") }), nh::lvl_low(w.rssi as f64, nh::TH_SIG)));
    }
    match a {
        Some(a) => {
            let sp = span_text(h.span_s);
            let (e, d) = (a.err_in + a.err_out, a.disc_in + a.disc_out);
            out.push(tile("Errors in / out", "passive", format!("{} / {}", a.err_in, a.err_out), "", format!("+{e} in {sp} · {} since up", group_num(a.err_total)), lvl(e as f64, nh::TH_ERR)));
            out.push(tile("Discards in / out", "passive", format!("{} / {}", a.disc_in, a.disc_out), "", format!("+{d} in {sp} · {} since up", group_num(a.disc_total)), lvl(d as f64, nh::TH_DISC)));
            out.push(tile("Packets/s in / out", "passive", format!("{} / {}", group_num(a.pk_in_s.round() as u64), group_num(a.pk_out_s.round() as u64)), "", "this adapter · no limit, context only".into(), Lv::Ok));
        }
        None => {
            for (l, s) in [("Errors in / out", "counters since up"), ("Discards in / out", "counters since up"), ("Packets/s in / out", "this adapter · no limit, context only")] {
                out.push(tile(l, "passive", "—".into(), "", s.into(), Lv::Ok));
            }
        }
    }
    match h.tcp {
        Some(t) => {
            let (l_retr, l_fail, l_rst) = t.levels();
            let bl = |t: Tile, vals: &[f32], usual: Option<f64>| Tile { spark: Some(Spark { vals: vals.to_vec(), usual: usual.map(|u| u as f32) }), ..t };
            let u = t.usual;
            let learn = format!("learning {}/{} min · fixed limits", h.base.len(), baseline::MIN_SAMPLES);
            let retr_sub = format!("{:.1}% of segments · {}", t.retr_pct, u.map_or(format!("learning {}/{}", h.base.len(), baseline::MIN_SAMPLES), |u| format!("usual {:.1}%", u.retr)));
            out.push(bl(tile("TCP retransmits", "system", num(t.retrans_s), "/s", retr_sub, l_retr), &h.base.retr, u.map(|u| u.retr)));
            out.push(bl(tile("Failed connects", "system", format!("{:.0}", t.fail_min), "/min", usual_sub(u.map(|u| u.fail), &learn, "failed connects"), l_fail), &h.base.fail, u.map(|u| u.fail)));
            out.push(bl(tile("TCP resets", "system", format!("{:.0}", t.rst_min), "/min", usual_sub(u.map(|u| u.rst), &learn, "resets"), l_rst), &h.base.rst, u.map(|u| u.rst)));
        }
        None => {
            for (l, s) in [("TCP retransmits", "of segments sent · system"), ("Failed connects", "connection attempts failed · system"), ("TCP resets", "established, then reset · system")] {
                out.push(tile(l, "system", "—".into(), "", s.into(), Lv::Ok));
            }
        }
    }
    out
}

use crate::sample::baseline;
use nh::lvl;

/// The muted line of a baseline tile: "usual 12/min · failed", or the learning note.
fn usual_sub(usual: Option<f64>, learn: &str, what: &str) -> String {
    match usual {
        Some(u) => format!("usual {u:.0}/min · {what}, system"),
        None => learn.to_string(),
    }
}

// ---------- probes (opt-in) ----------

/// What the panel needs to know about the probes of the selected adapter.
pub(super) struct Pr<'a> {
    /// the switch
    pub on: bool,
    /// the adapter has an IPv4 address to probe from
    pub can: bool,
    pub view: &'a probe::View,
    pub gw: String,
    pub dns: String,
    pub inet: String,
    pub st: probe::Settings,
}

pub(super) fn pr<'a>(v: &'a crate::sample::Vit, addr: Option<&crate::sample::net::Addr>) -> Pr<'a> {
    let st = probe::settings();
    let on = probe::enabled();
    let can = addr.is_some_and(|a| probe::cfg_for(0, &a.ipv4, &a.gateway, &a.dns, &st, "", None).is_some());
    Pr { on, can, view: &v.probe, gw: addr.map(|a| a.gateway.clone()).unwrap_or_default(), dns: addr.map(|a| a.dns.clone()).unwrap_or_default(), inet: st.internet.map_or("off".to_string(), |i| i.to_string()), st }
}

impl Pr<'_> {
    /// Probes are on and this adapter can be probed.
    fn active(&self) -> bool {
        self.on && self.can
    }
    /// Results are current.
    fn live(&self) -> bool {
        self.active() && self.view.live
    }
    fn state(&self) -> nh::ProbeState<'_> {
        if !self.active() {
            nh::ProbeState::Off
        } else if !self.view.live {
            nh::ProbeState::Waiting
        } else {
            nh::ProbeState::On(nh::ProbeIn { gw: self.view.gw.as_ref().map(|p| p.stats), inet: self.view.inet.stats, inet_state: self.view.inet_state, dns: self.view.dns, gw_addr: &self.gw, inet_addr: &self.inet, dns_addr: &self.dns, dns_name: &self.st.dns_name })
        }
    }
    /// 1 echo per target plus the lookup
    fn targets_n(&self) -> usize {
        usize::from(self.view.inet_state == probe::InetState::Probed) + usize::from(self.view.gw.is_some()) + usize::from(!matches!(self.view.dns, probe::Dns::NotProbed | probe::Dns::Local))
    }
    /// The internet echo is not sent (switched off in vitals.ini, or not through this adapter).
    fn inet_off(&self) -> Option<&'static str> {
        match self.view.inet_state {
            probe::InetState::Probed => None,
            probe::InetState::Off => Some("internet = none in vitals.ini"),
            probe::InetState::NotRouted => Some("internet not probed on this adapter"),
        }
    }
}

/// "0.8" with a unit, or a dash.
fn ms(v: Option<f32>) -> String {
    v.map_or("—".into(), nh::f1)
}

/// The probe tiles, in the mock's order. Values are dashes until the first results are in.
pub(super) fn probe_tiles(pr: &Pr) -> Vec<Tile> {
    let live = pr.live();
    let ly = pr.state_layers();
    let p = |label, value: String, unit: &str, sub: String, lv| Tile { label, tag: "probe", value, unit: unit.into(), sub, lv, spark: None };
    let none = |label| p(label, "—".into(), "", if pr.active() { "waiting for the first probe".into() } else { String::new() }, Lv::Ok);
    let no_gw = |label| p(label, "—".into(), "", "no gateway on this adapter".into(), Lv::Ok);
    let mut out = Vec::with_capacity(8);
    // gateway ping, jitter, loss
    match (&pr.view.gw, live) {
        (None, _) if pr.live() => {
            out.extend([no_gw("Gateway ping"), no_gw("Gateway jitter"), no_gw("Gateway loss")]);
            out.push(dns_tile(pr, &p));
        }
        (Some(g), true) => {
            let (st, silent) = (&g.stats, ly.gw_silent);
            let (ping, ping_sub) = if silent {
                ("—".to_string(), "no reply: the gateway ignores ping".to_string())
            } else if st.is_dead() {
                ("—".to_string(), format!("no reply for {:.0} s", st.silent_s))
            } else {
                (ms(st.now), format!("avg {} · min {} · max {}", ms(st.avg), ms(st.min), ms(st.max)))
            };
            let l = |v: Option<f32>, t| if silent { Lv::Ok } else if st.is_dead() { Lv::Hot } else { v.map_or(Lv::Ok, |v| lvl(v as f64, t)) };
            let unit = if ping == "—" { "" } else { "ms" };
            out.push(p("Gateway ping", ping, unit, ping_sub, l(st.avg, probe::TH_GW_RTT)));
            out.push(p("Gateway jitter", ms(st.jitter), "ms", "mean |Δ| of consecutive pings".into(), l(st.jitter, probe::TH_GW_JIT)));
            out.push(p("Gateway loss", format!("{:.0}", st.loss_pct), "%", format!("{} of {} lost · last 60 s", st.lost, st.sent), if silent { Lv::Ok } else if st.is_dead() { Lv::Hot } else { lvl(st.loss_pct as f64, probe::TH_LOSS) }));
            out.push(dns_tile(pr, &p));
        }
        _ => {
            out.extend([none("Gateway ping"), none("Gateway jitter"), none("Gateway loss"), none("DNS lookup")]);
        }
    }
    // internet ping, jitter, loss
    if let (true, Some(why)) = (live, pr.inet_off()) {
        let no = |label| p(label, "—".into(), "", why.into(), Lv::Ok);
        out.extend([no("Internet ping"), no("Internet jitter"), no("Internet loss")]);
        out.push(p("Probe traffic", format!("{:.0}", pr.view.kb_min), "KB/min", format!("{} target{} · one round per 2 s", pr.targets_n(), if pr.targets_n() == 1 { "" } else { "s" }), Lv::Ok));
    } else if live {
        let st = &pr.view.inet.stats;
        let silent = ly.inet_silent;
        let (ping, sub) = if silent {
            ("—".to_string(), "no reply: ping to this host is filtered".to_string())
        } else if st.is_dead() {
            ("—".to_string(), format!("no reply for {:.0} s", st.silent_s))
        } else {
            (ms(st.now), format!("avg {} · min {} · max {}", ms(st.avg), ms(st.min), ms(st.max)))
        };
        let l = |v: Option<f32>, t| if silent { Lv::Ok } else if st.is_dead() { Lv::Hot } else { v.map_or(Lv::Ok, |v| lvl(v as f64, t)) };
        let unit = if ping == "—" { "" } else { "ms" };
        out.push(p("Internet ping", ping, unit, sub, l(st.avg, probe::TH_IN_RTT)));
        out.push(p("Internet jitter", ms(st.jitter), "ms", format!("{} · mean |Δ|", pr.inet), l(st.jitter, probe::TH_IN_JIT)));
        out.push(p("Internet loss", format!("{:.0}", st.loss_pct), "%", format!("{} of {} lost · last 60 s", st.lost, st.sent), if silent { Lv::Ok } else if st.is_dead() { Lv::Hot } else { lvl(st.loss_pct as f64, probe::TH_LOSS) }));
        out.push(p("Probe traffic", format!("{:.0}", pr.view.kb_min), "KB/min", format!("{} target{} · one round per 2 s", pr.targets_n(), if pr.targets_n() == 1 { "" } else { "s" }), Lv::Ok));
    } else {
        out.extend([none("Internet ping"), none("Internet jitter"), none("Internet loss")]);
        out.push(p("Probe traffic", "—".into(), "", "one round per 2 s".into(), Lv::Ok));
    }
    out
}

fn dns_tile(pr: &Pr, p: &dyn Fn(&'static str, String, &str, String, Lv) -> Tile) -> Tile {
    let sub = format!("{} via {}", pr.st.dns_name, if pr.dns.is_empty() { "—" } else { &pr.dns });
    match pr.view.dns {
        probe::Dns::Ms(v) => p("DNS lookup", nh::f1(v), "ms", sub, probe::dns_level(pr.view.dns)),
        probe::Dns::Failed => p("DNS lookup", "fail".into(), "", sub, Lv::Hot),
        probe::Dns::NotProbed => p("DNS lookup", "—".into(), "", "no DNS server on this adapter".into(), Lv::Ok),
        probe::Dns::Local => p("DNS lookup", "—".into(), "", "local resolver, not probed".into(), Lv::Ok),
        probe::Dns::Waiting => p("DNS lookup", "—".into(), "", sub, Lv::Ok),
    }
}

impl Pr<'_> {
    /// Levels of the probed layers (all fine without current results).
    fn state_layers(&self) -> nh::Layers {
        match self.state() {
            nh::ProbeState::On(p) => nh::layers(&p),
            _ => nh::Layers::default(),
        }
    }
}

// ---------- verdict ----------

/// The verdict of the selected adapter; "Checking…" until the first diff exists.
pub(super) fn verdict(n: &VNic, h: &nh::Health, pr: &Pr, offender: Option<nh::Offender>, etw_live: bool) -> nh::Verdict {
    let Some(a) = h.adapters.iter().find(|a| a.luid == n.luid) else {
        return nh::Verdict { lv: Lv::Ok, label: "Checking…".into(), cause: "Reading the Windows counters: a few seconds.".into(), adapter: Lv::Ok, layers: None };
    };
    nh::diagnose(&nh::Inputs {
        adapter: &n.name,
        link_up: a.up,
        zero_traffic: n.rx_bps / 1e6 < nh::IDLE_MBPS && n.tx_bps / 1e6 < nh::IDLE_MBPS,
        errors: a.err_in + a.err_out,
        discards: a.disc_in + a.disc_out,
        wifi: a.wifi.as_ref().map(|w| (w.rssi, w.quality)),
        tcp: h.tcp,
        probe: pr.state(),
        offender,
        etw_live,
    })
}

fn colors(lv: Lv) -> (u32, u32, u32, u32) {
    match lv {
        Lv::Ok => (OK_BG, OK_LN, OK_TX, OK),
        Lv::Warn => (WARN_BG, WARN_LN, WARN_TX, WARN),
        Lv::Hot => (HOT_BG, HOT_LN, HOT, HOT),
    }
}

/// Greedy word wrap into at most `max` lines (the last one is cut by the text clip).
fn wrap(wd: &mut Widths, g: &mut Gfx, s: &str, font: Font, w: f32, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let t = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if cur.is_empty() || wd.get(g, &t, font) <= w {
            cur = t;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.truncate(max);
    lines
}

// ---------- targets ----------

/// One item of the targets row: a chip, or a plain note.
#[derive(Clone, Debug, PartialEq)]
enum Item {
    /// (tag, parts as (text, mono))
    Chip(&'static str, Vec<(String, bool)>),
    Note(String, u32),
}

fn items(pr: &Pr) -> Vec<Item> {
    let none = |s: &str| if s.is_empty() { "none".to_string() } else { s.to_string() };
    let mut v = vec![
        Item::Chip("ICMP", vec![("Gateway".into(), false), (none(&pr.gw), true)]),
        Item::Chip("ICMP", vec![("Internet".into(), false), (if pr.view.inet_state == probe::InetState::NotRouted { "not probed here".to_string() } else { pr.inet.clone() }, true)]),
        Item::Chip("DNS", vec![(pr.st.dns_name.clone(), true), ("via".into(), false), (none(&pr.dns), true)]),
    ];
    let note = if !pr.can {
        "no IPv4 address on this adapter: nothing to probe".to_string()
    } else if pr.on {
        "every 2 s · read-only · set in vitals.ini".to_string()
    } else {
        "read-only · set in vitals.ini".to_string()
    };
    v.push(Item::Note(note, TX3));
    if let Some(w) = &pr.st.warn {
        v.push(Item::Note(w.clone(), WARN_TX));
    }
    v
}

const CHIP_H: f32 = 24.0;
const ROW_H: f32 = 28.0;
const KEY_W: f32 = 66.0;

fn item_w(wd: &mut Widths, g: &mut Gfx, it: &Item) -> f32 {
    match it {
        Item::Chip(tag, parts) => {
            let t = wd.get(g, tag, f(9.5).b());
            let body: f32 = parts.iter().map(|(s, mono)| wd.get(g, s, if *mono { f(11.5).m() } else { f(11.5) })).sum::<f32>() + 5.0 * parts.len().saturating_sub(1) as f32;
            10.0 + t + 8.0 + body + 10.0
        }
        Item::Note(s, _) => wd.get(g, s, f(11.5)) + 4.0,
    }
}

/// Item indices per row for a row `w` wide (a row always takes at least one item).
fn flow(wd: &mut Widths, g: &mut Gfx, its: &[Item], w: f32) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = vec![Vec::new()];
    let mut x = 0.0;
    for (i, it) in its.iter().enumerate() {
        let iw = item_w(wd, g, it);
        if x > 0.0 && x + iw > w {
            rows.push(Vec::new());
            x = 0.0;
        }
        rows.last_mut().unwrap().push(i);
        x += iw + 8.0;
    }
    rows
}

// ---------- panel ----------

const GW_H: f32 = 66.0;
const IN_H: f32 = 66.0;
/// Hint box (probes off): padding, two lines.
const HINT_H: f32 = 62.0;

/// Geometry of the Network health panel, from the window width and the verdict text.
pub(super) struct Lay {
    pub h: f32,
    off_h: f32,
    side: bool,
    cols: usize,
    cause: Vec<String>,
    foot: Vec<String>,
    verdict_h: f32,
    tiles_h: f32,
    probe_tiles_h: f32,
    items: Vec<Item>,
    rows: Vec<Vec<usize>>,
    hint: Vec<String>,
}

fn tiles_block(cols: usize, n: usize) -> f32 {
    let rows = n.div_ceil(cols).max(1);
    10.0 + rows as f32 * TILE_H + (rows - 1) as f32 * TILE_GAP + 12.0
}

pub(super) fn layout(app: &mut App, g: &mut Gfx, l: &L, v: &nh::Verdict, tile_n: usize, pr: &Pr, probe_n: usize, off_h: f32) -> Lay {
    let side = l.w >= 820.0;
    let left_w = if side { l.w - 28.0 - CHAIN_W - 18.0 } else { l.w - 28.0 };
    let cause = wrap(&mut app.vit.wd, g, &v.cause, f(12.5), (left_w - 86.0).max(60.0), 4);
    let left = 12.0 + 24.0 + 9.0 + (cause.len().max(1) as f32) * LINE_H + 9.0 + 18.0 + 12.0;
    let chain = 12.0 + 62.0 + 4.0 + 16.0 + 12.0;
    let verdict_h = if side { left.max(chain) } else { left + chain - 12.0 };
    let cols = if l.w >= 820.0 { 4 } else { 2 };
    let tiles_h = tiles_block(cols, tile_n);
    let probe_tiles_h = tiles_block(cols, probe_n);
    let foot = wrap(&mut app.vit.wd, g, FOOT, f(11.0), l.w - 28.0, 3);
    let its = items(pr);
    let rows = flow(&mut app.vit.wd, g, &its, l.w - 28.0 - KEY_W);
    let hint = wrap(&mut app.vit.wd, g, HINT_SUB, f(11.5), (l.w - 28.0 - 28.0 - 150.0).max(80.0), 2);
    let targets_h = 6.0 + rows.len() as f32 * ROW_H + 2.0;
    // probe band, targets, then the graphs and tiles (on) or the hint (off)
    let probe_h = 1.0 + BAND_H + targets_h + if pr.on { 8.0 + GW_H + 8.0 + IN_H + 22.0 + probe_tiles_h } else { 12.0 + HINT_H + 14.0 };
    let h = 33.0 + verdict_h + 1.0 + off_h + probe_h + 1.0 + BAND_H + tiles_h + foot.len() as f32 * 15.0 + 12.0;
    Lay { h, off_h, side, cols, cause, foot, verdict_h, tiles_h, probe_tiles_h, items: its, rows, hint }
}

fn pill(app: &mut App, g: &mut Gfx, x: f32, y: f32, lv: Lv, text: &str, max_w: f32) {
    let (bg, ln, tx, dot) = colors(lv);
    let tw = app.vit.wd.get(g, text, f(12.5).b());
    let r = Rect::new(x, y, (tw + 9.0 + 8.0 + 7.0 + 11.0).min(max_w), 24.0);
    g.round(r, 12.0, bg);
    g.round_stroke(r, 12.0, ln, 1.0);
    g.circle(r.x + 13.0, r.y + 12.0, 4.0, dot);
    g.text(text, Rect::new(r.x + 24.0, r.y, (r.w - 30.0).max(0.0), 24.0), f(12.5).b(), tx);
}

/// What a chain node shows.
struct Node {
    name: &'static str,
    /// None: hollow, not measured (or silent)
    lv: Option<Lv>,
    text: String,
}

fn nodes(v: &nh::Verdict, n: &VNic, h: &nh::Health, pr: &Pr) -> [Node; 4] {
    let a = h.adapters.iter().find(|a| a.luid == n.luid);
    let adapter_v = match a {
        None => "…".to_string(),
        Some(a) if !a.up => "link down".into(),
        Some(a) if a.err_in + a.err_out > 0 => format!("{} err", a.err_in + a.err_out),
        Some(a) => match &a.wifi {
            Some(w) if v.adapter > Lv::Ok => format!("{} dBm", w.rssi),
            _ => "link up".into(),
        },
    };
    let adapter = Node { name: "Adapter", lv: Some(v.adapter), text: adapter_v };
    let hollow = |name, text: &str| Node { name, lv: None, text: text.into() };
    if !pr.active() {
        return [adapter, hollow("Gateway", "not measured"), hollow("Internet", "not measured"), hollow("DNS", "not measured")];
    }
    let (Some(ly), true) = (v.layers, pr.live()) else {
        return [adapter, hollow("Gateway", "…"), hollow("Internet", "…"), hollow("DNS", "…")];
    };
    let view = pr.view;
    let gw = match &view.gw {
        None => hollow("Gateway", "none"),
        Some(_) if ly.gw_silent => hollow("Gateway", "no reply"),
        Some(p) if p.stats.is_dead() => Node { name: "Gateway", lv: Some(ly.gw), text: "timeout".into() },
        Some(p) => Node { name: "Gateway", lv: Some(ly.gw), text: p.stats.now.map_or("lost".into(), |v| format!("{} ms", nh::f1(v))) },
    };
    let inet = if let Some(_) = pr.inet_off() {
        hollow("Internet", if view.inet_state == probe::InetState::Off { "off" } else { "not probed" })
    } else if ly.inet_silent {
        hollow("Internet", "no reply")
    } else if view.inet.stats.is_dead() {
        Node { name: "Internet", lv: Some(ly.inet), text: "timeout".into() }
    } else {
        Node { name: "Internet", lv: Some(ly.inet), text: view.inet.stats.now.map_or("lost".into(), |v| format!("{} ms", nh::f1(v))) }
    };
    let dns = match view.dns {
        probe::Dns::NotProbed => hollow("DNS", "none"),
        probe::Dns::Local => hollow("DNS", "local"),
        probe::Dns::Waiting => hollow("DNS", "…"),
        probe::Dns::Ms(ms) => Node { name: "DNS", lv: Some(ly.dns), text: format!("{} ms", nh::f1(ms)) },
        probe::Dns::Failed => Node { name: "DNS", lv: Some(ly.dns), text: "fail".into() },
    };
    [adapter, gw, inet, dns]
}

/// Layer chain: Adapter is measured from the counters, the rest from the probes (hollow while
/// they are off, waiting, or have nothing to say). A dashed line follows the first failing node.
fn chain(g: &mut Gfx, x: f32, y: f32, v: &nh::Verdict, n: &VNic, h: &nh::Health, pr: &Pr) {
    g.text("THIS PC → ROUTER → INTERNET → NAME LOOKUP", Rect::new(x, y, CHAIN_W, 14.0), f(10.0), TX4);
    let ns = nodes(v, n, h, pr);
    let xs = [48.0, 148.0, 248.0, 348.0];
    let cy = y + 26.0;
    let mut broke = false;
    for (i, nd) in ns.iter().enumerate() {
        let cx = x + xs[i];
        if i > 0 {
            if broke || nd.lv.is_none() {
                g.dash_h(x + xs[i - 1] + 10.0, cx - 10.0, cy.round(), 3.0, 3.0, LN, 1.0);
            } else {
                g.fill(Rect::new(x + xs[i - 1] + 10.0, cy.round(), xs[i] - xs[i - 1] - 20.0, 1.0), LN);
            }
        }
        match nd.lv {
            Some(lv) => {
                let (bg, ln, _, dot) = colors(lv);
                g.circle(cx, cy, 9.0, ln);
                g.circle(cx, cy, 8.0, bg);
                g.circle(cx, cy, 4.0, dot);
                broke |= lv > Lv::Ok;
            }
            None => {
                g.circle(cx, cy, 5.0, TX4);
                g.circle(cx, cy, 4.0, PAN);
            }
        }
        g.text(nd.name, Rect::new(cx - 45.0, y + 38.0, 90.0, 16.0), f(11.0).c(), TX3);
        g.text(&nd.text, Rect::new(cx - 48.0, y + 53.0, 96.0, 14.0), f(10.5).m().c(), if nd.lv.is_some() { TX1 } else { TX4 });
    }
    if pr.live() {
        g.text("Updated every 2 s · the first failing layer is the likely cause", Rect::new(x, y + 72.0, CHAIN_W, 16.0), f(11.0), TX4);
    } else if pr.active() {
        g.text("Probes on: first results in a few seconds", Rect::new(x, y + 72.0, CHAIN_W, 16.0), f(11.0), TX4);
    } else {
        // muted, the switch is in the panel title
        g.text(HINT, Rect::new(x, y + 72.0, CHAIN_W, 16.0), f(11.0), TX4);
    }
}

fn paint_tile(app: &mut App, g: &mut Gfx, r: Rect, t: &Tile) {
    g.round(r, 6.0, ZEBRA);
    g.round_stroke(r, 6.0, SEP, 1.0);
    if t.lv != Lv::Ok {
        g.fill(Rect::new(r.x, r.y + 8.0, 2.0, r.h - 16.0), colors(t.lv).3);
    }
    let tag = t.tag.to_uppercase();
    let tgw = app.vit.wd.get(g, &tag, f(9.5).b()) + 10.0;
    let tr = Rect::new(r.r() - 10.0 - tgw, r.y + 8.0, tgw, 14.0);
    tag_chip(g, tr, &tag, t.tag == "probe");
    g.text(t.label, Rect::new(r.x + 12.0, r.y + 6.0, (tr.x - r.x - 18.0).max(0.0), 16.0), f(11.5), TX3);
    let vw = app.vit.wd.get(g, &t.value, f(17.0).m().b());
    g.text(&t.value, Rect::new(r.x + 12.0, r.y + 24.0, vw + 2.0, 24.0), f(17.0).m().b(), TX1);
    if !t.unit.is_empty() {
        let uw = app.vit.wd.get(g, &t.unit, f(11.5));
        g.text(&t.unit, Rect::new(r.x + 12.0 + vw + 3.0, r.y + 28.0, uw + 2.0, 20.0), f(11.5), TX3);
    }
    let spark_w = if t.spark.is_some() { SPARK_W + 8.0 } else { 0.0 };
    if let Some(sp) = &t.spark {
        spark(g, Rect::new(r.r() - 10.0 - SPARK_W, r.y + 25.0, SPARK_W, SPARK_H), sp, if t.lv == Lv::Ok { C_NET } else { colors(t.lv).3 });
    }
    if t.lv != Lv::Ok {
        let (bg, ln, tx, _) = colors(t.lv);
        let word = if t.lv == Lv::Hot { "HOT" } else { "WARN" };
        let bw = app.vit.wd.get(g, word, f(9.5).b()) + 12.0;
        let br = Rect::new(r.r() - 10.0 - spark_w - bw, r.y + 29.0, bw, 15.0);
        g.round(br, 4.0, bg);
        g.round_stroke(br, 4.0, ln, 1.0);
        g.text(word, br, f(9.5).b().c(), tx);
    }
    g.text(&t.sub, Rect::new(r.x + 12.0, r.b() - 21.0, r.w - 20.0, 14.0), f(10.5).m(), TX3);
}

const SPARK_W: f32 = 72.0;
const SPARK_H: f32 = 24.0;

/// Scale top of a baseline sparkline: the highest sample, never below 1.25 x usual, so a flat quiet
/// history stays flat and a spike stands out.
pub(super) fn spark_top(vals: &[f32], usual: Option<f32>) -> f32 {
    vals.iter().copied().filter(|v| v.is_finite()).fold(usual.unwrap_or(0.0) * 1.25, f32::max).max(0.001)
}

/// 60-minute sparkline with the dashed usual line. Fewer than two samples draw only the line.
fn spark(g: &mut Gfx, r: Rect, sp: &Spark, col: u32) {
    let top = spark_top(&sp.vals, sp.usual);
    g.series(r, &sp.vals, 0.0, top, col, 0.14, 1.25, false);
    if let Some(u) = sp.usual {
        let y = (r.b() - ((u / top) * (r.h - 1.0) + 0.5)).round();
        g.dash_h(r.x, r.r(), y, 3.0, 2.0, TX3, 0.8);
    }
}

/// The small outlined tag of a tile or band; a probe tag is filled with the accent tint.
pub(super) fn tag_chip(g: &mut Gfx, r: Rect, text: &str, probe: bool) {
    if probe {
        g.round(r, 3.0, ACC_BG);
        g.round_stroke(r, 3.0, ACC_LN, 1.0);
        g.text(text, r, f(9.5).b().c(), ACC_TX);
    } else {
        g.round_stroke(r, 3.0, SEP, 1.0);
        g.text(text, r, f(9.5).b().c(), TX3);
    }
}

/// A band across the panel: bold name, muted explanation, tag at the right.
pub(super) fn band(app: &mut App, g: &mut Gfx, r: Rect, y: f32, name: &str, what: &str, tag: &str, probe: bool) {
    let band = Rect::new(r.x + 1.0, y, r.w - 2.0, BAND_H);
    g.fill(band, PAN2);
    g.fill(Rect::new(band.x, band.y, band.w, 1.0), LN);
    g.fill(Rect::new(band.x, band.b() - 1.0, band.w, 1.0), SEP);
    let pw = app.vit.wd.get(g, name, f(12.0).b());
    g.text(name, Rect::new(r.x + 14.0, band.y, pw + 2.0, BAND_H), f(12.0).b(), TX1);
    g.text(what, Rect::new(r.x + 14.0 + pw + 10.0, band.y, (r.w - pw - 120.0).max(0.0), BAND_H), f(11.5), TX3);
    let tgw = app.vit.wd.get(g, tag, f(9.5).b()) + 10.0;
    tag_chip(g, Rect::new(r.r() - 14.0 - tgw, band.y + 7.0, tgw, 14.0), tag, probe);
}

/// The Probes on / off switch at the right of the panel title.
fn switch(app: &mut App, g: &mut Gfx, r: Rect, on: bool) -> f32 {
    let word = if on { "on" } else { "off" };
    let ww = app.vit.wd.get(g, word, f(11.5).b());
    let lw = app.vit.wd.get(g, "Probes", f(11.5));
    let w = lw + 8.0 + 26.0 + 8.0 + ww.max(18.0);
    let sr = Rect::new(r.r() - 14.0 - w, r.y + 5.0, w, 22.0);
    app.hits.push((sr, Hit::VitSeg(11)));
    if app.hover == Some(Hit::VitSeg(11)) {
        g.round(sr.inset(-6.0, 0.0), 6.0, PAN2);
    }
    g.text("Probes", Rect::new(sr.x, sr.y, lw + 2.0, sr.h), f(11.5), TX3);
    let tr = Rect::new(sr.x + lw + 8.0, sr.y + 4.0, 26.0, 14.0);
    if on {
        g.round(tr, 7.0, ACC);
        g.circle(tr.r() - 7.0, tr.y + 7.0, 5.0, BG);
    } else {
        g.round(tr, 7.0, TRACK);
        g.round_stroke(tr, 7.0, LN, 1.0);
        g.circle(tr.x + 7.0, tr.y + 7.0, 5.0, TX4);
    }
    g.text(word, Rect::new(tr.r() + 8.0, sr.y, ww + 4.0, sr.h), f(11.5).b(), if on { TX1 } else { TX3 });
    sr.x
}

/// One chip: tag, then the label and mono parts.
fn paint_chip(app: &mut App, g: &mut Gfx, x: f32, y: f32, w: f32, tag: &str, parts: &[(String, bool)], on: bool) {
    let r = Rect::new(x, y + (ROW_H - CHIP_H) / 2.0 - 1.0, w, CHIP_H);
    g.round(r, 12.0, PAN2);
    g.round_stroke(r, 12.0, LN, 1.0);
    let tw = app.vit.wd.get(g, tag, f(9.5).b());
    g.text(tag, Rect::new(r.x + 10.0, r.y, tw + 2.0, r.h), f(9.5).b(), if on { TX3 } else { TX4 });
    let mut px = r.x + 10.0 + tw + 8.0;
    for (s, mono) in parts {
        let font = if *mono { f(11.5).m() } else { f(11.5) };
        let pw = app.vit.wd.get(g, s, font);
        g.text(s, Rect::new(px, r.y, pw + 2.0, r.h), font, match (on, *mono) {
            (false, _) => TX4,
            (true, true) => TX1,
            (true, false) => TX2,
        });
        px += pw + 5.0;
    }
}

// ---------- RTT graphs ----------

/// y scale of an RTT graph: never below 1.25 x the warn line, so a healthy graph stays flat.
fn rtt_scale(cols: &[probe::Col], warn: f64) -> f32 {
    let top = cols.iter().filter_map(|c| if let probe::Col::Ms(v) = c { Some(*v) } else { None }).fold(0.0f32, f32::max);
    nice(top.max(warn as f32 * 1.25))
}

fn rtt_text(c: probe::Col) -> String {
    match c {
        probe::Col::Ms(v) => format!("{} ms", nh::f1(v)),
        probe::Col::Lost => "lost".into(),
        probe::Col::None => "—".into(),
    }
}

/// One RTT graph in `gr`, on the throughput plot's x range: grid, dashed warn line, series, red
/// ticks for lost probes, and the hover cursor (readout in the hovered graph only).
#[allow(clippy::too_many_arguments)]
fn rtt_graph(app: &mut App, g: &mut Gfx, l: &L, mut p: Plot, gr: Rect, slot: u8, name: &str, color: u32, path: Option<&probe::Path>, warn: f64, both: &[Option<&probe::Path>; 2]) {
    let empty = probe::Path::default();
    let pa = path.unwrap_or(&empty);
    let scale = rtt_scale(&pa.cols, warn);
    ygrid(g, &p, gr, &[(0.0, "0".into()), (1.0, format!("{} ms", num(scale as f64)))]);
    xgrid(g, &p, gr.y, gr.h);
    let ir = inner(&p, gr.y, gr.h);
    if (warn as f32) < scale {
        let y = (ir.b() - ((warn as f32 / scale) * (ir.h - 1.0) + 0.5)).round();
        g.dash_h(p.x, p.x + p.w, y, 4.0, 3.0, WARN, 0.7);
        let t = format!("warn {}", num(warn));
        let tw = app.vit.wd.get(g, &t, f(9.5).m());
        g.text(&t, Rect::new(p.x + p.w - tw - 4.0, y - 13.0, tw + 2.0, 12.0), f(9.5).m(), TX4);
    }
    g.series(ir, &probe::series(&pa.cols), 0.0, scale, color, 0.14, 1.5, false);
    for i in probe::lost_cols(&pa.cols) {
        let x = ir.x + i as f32 / (HIST - 1) as f32 * ir.w;
        g.fill(Rect::new((x - 1.0).round(), gr.b() - 10.0, 2.0, 10.0), HOT);
    }
    // legend: the value at the pointer (or now), the loss over the minute
    let hov = app.vit.hover_col;
    let shown = match hov {
        Some(c) => rtt_text(probe::col_at(&pa.cols, c)),
        None => pa.stats.now.map_or(if pa.stats.sent > 0 { "lost".into() } else { "—".into() }, |v| format!("{} ms", nh::f1(v))),
    };
    p.leg.y = gr.y;
    let y = legend(g, l, &p, gr.y, color, name, &shown);
    note(g, &p, y + 2.0, "loss 60 s", &if pa.stats.sent > 0 { format!("{:.0}%", pa.stats.loss_pct) } else { "—".into() });
    let (gw_c, in_c) = (both[0].map_or(&empty.cols, |p| &p.cols), both[1].map_or(&empty.cols, |p| &p.cols));
    cursor_tip(app, g, &p, slot, gr.y, gr.b(), |c| Tip {
        head: ago(c),
        lines: vec![
            TipLine { swatch: Some(LAN_DN), label: "Gateway".into(), value: rtt_text(probe::col_at(gw_c, c)), hot: false },
            TipLine { swatch: Some(C_MEM), label: "Internet".into(), value: rtt_text(probe::col_at(in_c, c)), hot: false },
        ],
        foot: None,
    });
}

const HINT_SUB: &str = "Hayate then sends a tiny ICMP echo to your gateway and to the internet host, plus one DNS lookup, every 2 s, only while this page is open. Off: nothing is sent and only the counters below are read.";

/// The Network health panel in `r` (height from `layout`).
#[allow(clippy::too_many_arguments)]
pub(super) fn health(app: &mut App, g: &mut Gfx, l: &L, r: Rect, lay: &Lay, v: &nh::Verdict, n: &VNic, h: &nh::Health, tl: &[Tile], ptl: &[Tile], pr: &Pr, s: &Snapshot, ov: &offenders::View) {
    let sw_w = 120.0;
    titled_r(app, g, r, "NETWORK HEALTH", &format!("{} · {}", n.name, if pr.active() { "probes on" } else { "passive checks only" }), sw_w);
    switch(app, g, r, pr.on);
    let vy = r.y + 33.0;
    // verdict: level rail, pill, likely cause, what was checked | layer chain
    let rail = colors(v.lv).3;
    g.fill(Rect::new(r.x, vy + 12.0, 2.0, lay.verdict_h - 24.0), rail);
    let x = r.x + 14.0;
    pill(app, g, x, vy + 12.0, v.lv, &v.label, r.w - 28.0);
    let mut y = vy + 12.0 + 24.0 + 9.0;
    let key = |g: &mut Gfx, y: f32, k: &str| g.text(k, Rect::new(x, y, 78.0, LINE_H), f(10.5).b(), TX4);
    key(g, y, "LIKELY CAUSE");
    let tx = x + 86.0;
    let cw = if lay.side { r.w - 28.0 - CHAIN_W - 18.0 - 86.0 } else { r.w - 28.0 - 86.0 };
    for line in &lay.cause {
        g.text(line, Rect::new(tx, y, cw.max(0.0), LINE_H), f(12.5), TX2);
        y += LINE_H;
    }
    y += 9.0;
    key(g, y, "CHECKED");
    let checked = if pr.live() { if v.lv == Lv::Ok { "All layers pass. Updated every 2 s." } else { "Layers read left to right: the first one that fails is the likely cause. Updated every 2 s." } } else if pr.active() { "Passive counters now; ping, jitter, loss and DNS follow in a few seconds." } else { CHECKED };
    g.text(checked, Rect::new(tx, y, cw.max(0.0), LINE_H), f(11.5), TX3);
    let (cx, cy) = if lay.side { (r.r() - 14.0 - CHAIN_W, vy + 12.0) } else { (x, y + LINE_H + 12.0) };
    chain(g, cx, cy, v, n, h, pr);
    let by = vy + lay.verdict_h;
    g.fill(Rect::new(r.x + 1.0, by, r.w - 2.0, 1.0), SEP);

    // who is failing: apps whose connections fail, from the ETW connection events
    let oy = by + 1.0;
    let otip = offenders::paint(app, g, s, r, oy, ov);
    let pb_y = oy + lay.off_h;
    g.fill(Rect::new(r.x + 1.0, pb_y, r.w - 2.0, 1.0), SEP);
    let pb_y = pb_y + 1.0;
    // probe band, targets, then the graphs + tiles (on) or the hint (off)
    band(app, g, r, pb_y, "Probes", if pr.active() { "active · sends small packets" } else { "off · nothing is sent" }, "PROBE", true);
    let mut y = pb_y + BAND_H + 6.0;
    g.text("TARGETS", Rect::new(r.x + 14.0, y, KEY_W, ROW_H), f(10.5).b(), TX4);
    for row in &lay.rows {
        let mut x = r.x + 14.0 + KEY_W;
        for &i in row {
            let w = item_w(&mut app.vit.wd, g, &lay.items[i]);
            match &lay.items[i] {
                Item::Chip(tag, parts) => paint_chip(app, g, x, y, w, tag, parts, pr.on),
                Item::Note(s, c) => g.text(s, Rect::new(x, y, w + 2.0, ROW_H), f(11.5), *c),
            }
            x += w + 8.0;
        }
        y += ROW_H;
    }
    y += 2.0;
    if pr.on {
        // the RTT graphs share the throughput graph's x range, so one cursor lines up all three
        let p = plot_cols(l, r, if l.narrow { 56.0 } else { 84.0 });
        let both = [pr.view.gw.as_ref(), Some(&pr.view.inet)];
        y += 8.0;
        let gr = Rect::new(p.x, y, p.w, GW_H);
        let gp = Plot { x: p.x, w: p.w, lab_r: p.lab_r, leg: p.leg };
        rtt_graph(app, g, l, gp, gr, 1, "Gateway", LAN_DN, pr.view.gw.as_ref(), probe::TH_GW_RTT[0], &both);
        y += GW_H + 8.0;
        let gr = Rect::new(p.x, y, p.w, IN_H);
        rtt_graph(app, g, l, p, gr, 2, "Internet", C_MEM, Some(&pr.view.inet), probe::TH_IN_RTT[0], &both);
        // the same time axis as the throughput graph
        let p2 = plot_cols(l, r, if l.narrow { 56.0 } else { 84.0 });
        xaxis(g, &p2, gr.b() + 5.0);
        y += IN_H + 22.0;
        let ty = y + 10.0;
        let tw = (r.w - 24.0 - (lay.cols as f32 - 1.0) * TILE_GAP) / lay.cols as f32;
        for (i, t) in ptl.iter().enumerate() {
            let (c, row) = (i % lay.cols, i / lay.cols);
            paint_tile(app, g, Rect::new(r.x + 12.0 + c as f32 * (tw + TILE_GAP), ty + row as f32 * (TILE_H + TILE_GAP), tw, TILE_H), t);
        }
        y += lay.probe_tiles_h;
    } else {
        // the muted hint with the button
        y += 12.0;
        let hr = Rect::new(r.x + 14.0, y, r.w - 28.0, HINT_H);
        g.round(hr, 7.0, PAN);
        g.round_stroke(hr, 7.0, LN, 1.0);
        g.text("Enable probes for ping, jitter, loss, DNS", Rect::new(hr.x + 14.0, hr.y + 8.0, hr.w - 180.0, 20.0), f(12.5).b(), TX2);
        for (k, line) in lay.hint.iter().enumerate() {
            g.text(line, Rect::new(hr.x + 14.0, hr.y + 28.0 + k as f32 * 16.0, hr.w - 180.0, 16.0), f(11.5), TX3);
        }
        let bw = app.vit.wd.get(g, "Turn probes on", f(11.5)) + 26.0;
        let br = Rect::new(hr.r() - 14.0 - bw, hr.y + (hr.h - 24.0) / 2.0, bw, 24.0);
        g.round(br, 12.0, if app.hover == Some(Hit::VitSeg(12)) { PAN3 } else { PAN2 });
        g.round_stroke(br, 12.0, LN, 1.0);
        g.text("Turn probes on", br, f(11.5).c(), TX1);
        app.hits.push((br, Hit::VitSeg(12)));
        y += HINT_H + 14.0;
    }

    // passive band + tiles
    band(app, g, r, y, "Passive", "read from Windows counters · sends nothing", "PASSIVE", false);
    let ty = y + BAND_H + 10.0;
    let tw = (r.w - 24.0 - (lay.cols as f32 - 1.0) * TILE_GAP) / lay.cols as f32;
    for (i, t) in tl.iter().enumerate() {
        let (c, row) = (i % lay.cols, i / lay.cols);
        paint_tile(app, g, Rect::new(r.x + 12.0 + c as f32 * (tw + TILE_GAP), ty + row as f32 * (TILE_H + TILE_GAP), tw, TILE_H), t);
    }
    let mut fy = y + BAND_H + lay.tiles_h;
    for line in &lay.foot {
        g.text(line, Rect::new(r.x + 14.0, fy, r.w - 28.0, 15.0), f(11.0), TX3);
        fy += 15.0;
    }
    if let Some(t) = otip {
        offenders::paint_late(app, g, t);
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::nethealth::{AdapterHealth, Health, TcpHealth, Wifi};

    fn nic(kind: &'static str) -> VNic {
        VNic { luid: 7, name: "Ethernet 3".into(), desc: String::new(), kind, link_bps: 2_500_000_000, rx_bps: 122e6, tx_bps: 5e6, rx_total: 0, tx_total: 0, rx: vec![], tx: vec![] }
    }
    fn off(v: &probe::View) -> Pr<'_> {
        Pr { on: false, can: true, view: v, gw: "10.0.1.1".into(), dns: "10.0.1.1".into(), inet: "1.1.1.1".into(), st: probe::Settings::default() }
    }
    fn ad() -> AdapterHealth {
        AdapterHealth { luid: 7, up: true, pk_in_s: 1240.0, pk_out_s: 380.0, err_total: 1208, ..Default::default() }
    }

    #[test]
    fn link_share_of_the_adapter_speed() {
        let p = link_pct(122.0, 2_500_000_000).unwrap();
        assert!((p - 4.88).abs() < 0.01);
        assert_eq!(link_pct(10.0, 0), None, "no link speed, no ratio");
        assert_eq!(link_pct(f64::NAN, 1_000_000_000), None);
    }

    #[test]
    fn wired_adapter_has_no_wifi_tile_and_clean_counters_are_ok() {
        let h = Health { adapters: vec![ad()], tcp: Some(TcpHealth { retrans_s: 0.2, retr_pct: 0.01, ..Default::default() }), span_s: 60.0, ..Default::default() };
        let t = tiles(&nic("wired"), &h);
        let names: Vec<&str> = t.iter().map(|t| t.label).collect();
        assert_eq!(names, ["Link", "Errors in / out", "Discards in / out", "Packets/s in / out", "TCP retransmits", "Failed connects", "TCP resets"]);
        assert!(t.iter().all(|t| t.lv == Lv::Ok));
        assert_eq!(t[3].value, "1,240 / 380");
        assert_eq!(t[1].sub, "+0 in 60 s · 1,208 since up");
        assert_eq!((t[4].tag, t[5].tag), ("system", "system"), "TCP tiles say they are system-wide");
    }

    #[test]
    fn wifi_tile_only_when_connected_and_warns_on_weak_signal() {
        let mut a = ad();
        let none = tiles(&nic("wireless"), &Health { adapters: vec![a.clone()], tcp: None, span_s: 60.0, ..Default::default() });
        assert!(none.iter().all(|t| t.label != "Wi-Fi signal"), "unavailable WLAN data skips the tile");
        a.wifi = Some(Wifi { ssid: "x".into(), rssi: -78, quality: 31, channel: 6 });
        let t = tiles(&nic("wireless"), &Health { adapters: vec![a], tcp: None, span_s: 60.0, ..Default::default() });
        assert_eq!(t[1].label, "Wi-Fi signal");
        assert_eq!((t[1].value.as_str(), t[1].lv), ("-78", Lv::Hot));
        assert_eq!(t[1].sub, "quality 31% · ch 6 · 2.4 GHz");
    }

    #[test]
    fn counters_light_up_tiles_by_threshold() {
        let mut a = ad();
        a.err_in = 14;
        a.disc_in = 1;
        a.flaps = 1;
        let tcp = TcpHealth { retrans_s: 6.4, retr_pct: 3.4, fail_min: 40.0, rst_min: 9.0, ..Default::default() };
        let t = tiles(&nic("wired"), &Health { adapters: vec![a], tcp: Some(tcp), span_s: 30.0, ..Default::default() });
        let lv: Vec<Lv> = t.iter().map(|t| t.lv).collect();
        assert_eq!(lv, [Lv::Warn, Lv::Hot, Lv::Warn, Lv::Ok, Lv::Hot, Lv::Warn, Lv::Ok]);
        assert!(t[1].sub.starts_with("+14 in 30 s"));
    }

    #[test]
    fn no_data_yet_reads_dashes_and_a_checking_verdict() {
        let pv = probe::View::default();
        let h = Health::default();
        let t = tiles(&nic("wired"), &h);
        assert_eq!(t.len(), 7);
        assert!(t[1..].iter().all(|t| t.value == "—" && t.lv == Lv::Ok));
        let v = verdict(&nic("wired"), &h, &off(&pv), None, false);
        assert_eq!((v.lv, v.label.as_str()), (Lv::Ok, "Checking…"));
    }

    #[test]
    fn verdict_reads_the_adapter_and_system_counters() {
        let pv = probe::View::default();
        let mut a = ad();
        a.up = false;
        let v = verdict(&nic("wired"), &Health { adapters: vec![a], tcp: None, span_s: 10.0, ..Default::default() }, &off(&pv), None, false);
        assert_eq!(v.lv, Lv::Hot);
        let v = verdict(&nic("wired"), &Health { adapters: vec![ad()], tcp: Some(TcpHealth::default()), span_s: 10.0, ..Default::default() }, &off(&pv), None, false);
        assert_eq!(v.label, "Healthy");
    }
}
