//! Network health for Vitals > Network: passive interface error / discard / packet counters
//! (MIB_IF_ROW2), system-wide TCP statistics (GetTcpStatisticsEx), Wi-Fi signal (WLAN API) and
//! link flaps. The passive reads are read-only and send nothing. The opt-in probes (ping, loss,
//! DNS) live in `probe`; this file holds only the verdict that combines both.
//!
//! `read` runs on the I/O worker (bgio), once a second, only while the Network detail is
//! visible. `Hist` lives on the sampler thread: it diffs the counters over 60 s and counts link
//! flaps (up/down changes of the adapter list the sampler reads anyway, free of API calls).
//! `diagnose` is the verdict from the design mock, pure and tested.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use super::baseline::{self, Ring, Series, Usual};
use super::probe::{self, Dns, InetState, PathStats};

/// Counter window the error / retransmit tiles diff over.
pub const WINDOW: Duration = Duration::from_secs(60);
/// A gap this long between two reads (detail was hidden) drops the older ones: a diff must never
/// span time nobody sampled.
const GAP: Duration = Duration::from_secs(5);
/// Rates need at least this much span to mean anything.
const MIN_SPAN: f64 = 3.0;

// ---------- thresholds [warn, hot] (the design mock, "WARN / HOT at") ----------

/// errors, or discards, in + out per 60 s
pub const TH_ERR: [f64; 2] = [1.0, 10.0];
pub const TH_DISC: [f64; 2] = [1.0, 10.0];
/// % of sent TCP segments retransmitted
pub const TH_RETR: [f64; 2] = [1.0, 3.0];
/// failed connection attempts per minute (system-wide; busy hosts see ~10/min at idle, so only a flood counts)
pub const TH_FAIL: [f64; 2] = [30.0, 90.0];
/// established connections reset, per minute
pub const TH_RST: [f64; 2] = [60.0, 180.0];
/// Wi-Fi signal, dBm; lower is worse
pub const TH_SIG: [f64; 2] = [-67.0, -75.0];
/// link down / up changes since Blink started
pub const TH_FLAP: [f64; 2] = [1.0, 3.0];
/// below this Mb/s both ways counts as "nothing is moving"
pub const IDLE_MBPS: f64 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Lv {
    #[default]
    Ok,
    Warn,
    Hot,
}

/// Level of a value that is worse when higher.
pub fn lvl(v: f64, t: [f64; 2]) -> Lv {
    if !v.is_finite() {
        Lv::Ok
    } else if v >= t[1] {
        Lv::Hot
    } else if v >= t[0] {
        Lv::Warn
    } else {
        Lv::Ok
    }
}
/// Level of a value that is worse when lower (signal strength).
pub fn lvl_low(v: f64, t: [f64; 2]) -> Lv {
    if !v.is_finite() {
        Lv::Ok
    } else if v <= t[1] {
        Lv::Hot
    } else if v <= t[0] {
        Lv::Warn
    } else {
        Lv::Ok
    }
}

// ---------- raw counters (worker) ----------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct IfCtr {
    pub up: bool,
    pub in_err: u64,
    pub out_err: u64,
    pub in_disc: u64,
    pub out_disc: u64,
    pub in_pkts: u64,
    pub out_pkts: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TcpCtr {
    pub retrans: u64,
    pub out_segs: u64,
    pub fails: u64,
    pub resets: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wifi {
    pub ssid: String,
    /// dBm
    pub rssi: i32,
    /// 0..=100 (WLAN_ASSOCIATION_ATTRIBUTES.wlanSignalQuality)
    pub quality: u32,
    pub channel: u32,
}

impl Wifi {
    /// "2.4 GHz" / "5 GHz" from the channel number; "" when ambiguous (6 GHz reuses 1..=233).
    pub fn band(&self) -> &'static str {
        match self.channel {
            1..=14 => "2.4 GHz",
            36..=177 => "5 GHz",
            _ => "",
        }
    }
}

/// One read of everything, stamped when the worker took it.
#[derive(Clone, Debug)]
pub struct Raw {
    pub at: Instant,
    pub ifs: Vec<(u64, IfCtr)>,
    pub tcp: Option<TcpCtr>,
    pub wifi: Vec<(u64, Wifi)>,
}

/// Read the counters of `luids` (the connected adapters), the system TCP statistics, and the
/// Wi-Fi state of the `wireless` ones. Worker thread only: a handful of IP Helper calls
/// (~0.1 ms each) plus, with a Wi-Fi adapter, one short WLAN session.
pub fn read(luids: &[u64], wireless: &[u64]) -> Option<Raw> {
    use windows::Win32::NetworkManagement::IpHelper::*;
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    let mut ifs = Vec::with_capacity(luids.len());
    for &luid in luids {
        let mut r = MIB_IF_ROW2::default();
        r.InterfaceLuid.Value = luid;
        if unsafe { GetIfEntry2(&mut r) }.is_err() {
            continue;
        }
        ifs.push((
            luid,
            IfCtr {
                up: r.OperStatus == IfOperStatusUp,
                in_err: r.InErrors,
                out_err: r.OutErrors,
                in_disc: r.InDiscards,
                out_disc: r.OutDiscards,
                in_pkts: r.InUcastPkts.saturating_add(r.InNUcastPkts),
                out_pkts: r.OutUcastPkts.saturating_add(r.OutNUcastPkts),
            },
        ));
    }
    let tcp = tcp_counters();
    let wifi = if wireless.is_empty() { Vec::new() } else { wlan(wireless) };
    Some(Raw { at: Instant::now(), ifs, tcp, wifi })
}

/// System-wide TCP counters, IPv4 + IPv6.
fn tcp_counters() -> Option<TcpCtr> {
    use windows::Win32::NetworkManagement::IpHelper::*;
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    let mut c = TcpCtr::default();
    let mut any = false;
    for af in [AF_INET.0 as u32, AF_INET6.0 as u32] {
        let mut s = MIB_TCPSTATS_LH::default();
        if unsafe { GetTcpStatisticsEx(&mut s, af) } == 0 {
            any = true;
            c.retrans += s.dwRetransSegs as u64;
            c.out_segs += s.dwOutSegs as u64;
            c.fails += s.dwAttemptFails as u64;
            c.resets += s.dwEstabResets as u64;
        }
    }
    any.then_some(c)
}

// wlanapi.dll is loaded for the length of one read and freed again: it is absent on Windows
// editions without the Wireless LAN service, and nothing may stay loaded or open while the
// detail is hidden.
type FnOpen = unsafe extern "system" fn(u32, *const core::ffi::c_void, *mut u32, *mut isize) -> u32;
type FnClose = unsafe extern "system" fn(isize, *const core::ffi::c_void) -> u32;
type FnEnum = unsafe extern "system" fn(isize, *const core::ffi::c_void, *mut *mut core::ffi::c_void) -> u32;
type FnQuery = unsafe extern "system" fn(isize, *const windows::core::GUID, i32, *const core::ffi::c_void, *mut u32, *mut *mut core::ffi::c_void, *mut i32) -> u32;
type FnFree = unsafe extern "system" fn(*const core::ffi::c_void);

const OPCODE_CURRENT_CONNECTION: i32 = 7;
const OPCODE_CHANNEL_NUMBER: i32 = 8;
const OPCODE_RSSI: i32 = 0x1000_0102;

/// Signal, quality and channel of every connected Wi-Fi interface whose LUID is in `want`.
fn wlan(want: &[u64]) -> Vec<(u64, Wifi)> {
    use windows::core::{s, w};
    use windows::Win32::NetworkManagement::IpHelper::ConvertInterfaceGuidToLuid;
    use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
    use windows::Win32::NetworkManagement::WiFi::*;
    use windows::Win32::Foundation::FreeLibrary;
    use windows::Win32::System::LibraryLoader::*;
    let mut out = Vec::new();
    unsafe {
        let Ok(lib) = LoadLibraryExW(w!("wlanapi.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) else { return out };
        let sym = |name: windows::core::PCSTR| GetProcAddress(lib, name);
        let (Some(open), Some(close), Some(enum_), Some(query), Some(free)) = (sym(s!("WlanOpenHandle")), sym(s!("WlanCloseHandle")), sym(s!("WlanEnumInterfaces")), sym(s!("WlanQueryInterface")), sym(s!("WlanFreeMemory"))) else {
            let _ = FreeLibrary(lib);
            return out;
        };
        let open: FnOpen = std::mem::transmute(open);
        let close: FnClose = std::mem::transmute(close);
        let enum_: FnEnum = std::mem::transmute(enum_);
        let query: FnQuery = std::mem::transmute(query);
        let free: FnFree = std::mem::transmute(free);
        let (mut ver, mut h) = (0u32, 0isize);
        if open(2, std::ptr::null(), &mut ver, &mut h) == 0 {
            let mut list: *mut core::ffi::c_void = std::ptr::null_mut();
            if enum_(h, std::ptr::null(), &mut list) == 0 && !list.is_null() {
                let l = &*(list as *const WLAN_INTERFACE_INFO_LIST);
                let items = std::slice::from_raw_parts(l.InterfaceInfo.as_ptr(), l.dwNumberOfItems as usize);
                for it in items {
                    let mut luid = NET_LUID_LH::default();
                    if ConvertInterfaceGuidToLuid(&it.InterfaceGuid, &mut luid).0 != 0 || !want.contains(&luid.Value) {
                        continue;
                    }
                    // one query of one opcode; the result buffer is freed here, whatever it held
                    let q = |op: i32| -> Option<Vec<u8>> {
                        let (mut n, mut p, mut ty) = (0u32, std::ptr::null_mut(), 0i32);
                        if query(h, &it.InterfaceGuid, op, std::ptr::null(), &mut n, &mut p, &mut ty) != 0 || p.is_null() {
                            return None;
                        }
                        let v = std::slice::from_raw_parts(p as *const u8, n as usize).to_vec();
                        free(p);
                        Some(v)
                    };
                    let Some(cc) = q(OPCODE_CURRENT_CONNECTION) else { continue }; // not connected
                    if cc.len() < std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
                        continue;
                    }
                    let a = std::ptr::read_unaligned(cc.as_ptr() as *const WLAN_CONNECTION_ATTRIBUTES);
                    let assoc = a.wlanAssociationAttributes;
                    let n = (assoc.dot11Ssid.uSSIDLength as usize).min(32);
                    let rd = |b: Option<Vec<u8>>| b.filter(|b| b.len() >= 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                    out.push((
                        luid.Value,
                        Wifi {
                            ssid: String::from_utf8_lossy(&assoc.dot11Ssid.ucSSID[..n]).into_owned(),
                            rssi: rd(q(OPCODE_RSSI)).unwrap_or(0),
                            quality: assoc.wlanSignalQuality.min(100),
                            channel: rd(q(OPCODE_CHANNEL_NUMBER)).unwrap_or(0).max(0) as u32,
                        },
                    ));
                }
                free(list);
            }
            close(h, std::ptr::null());
        }
        let _ = FreeLibrary(lib);
    }
    out
}

// ---------- history and diffs (sampler thread) ----------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdapterHealth {
    pub luid: u64,
    pub up: bool,
    /// link up/down changes Blink saw since it started
    pub flaps: u32,
    /// counter growth over `span_s` (at most 60 s)
    pub err_in: u64,
    pub err_out: u64,
    pub disc_in: u64,
    pub disc_out: u64,
    /// counters since the adapter came up
    pub err_total: u64,
    pub disc_total: u64,
    pub pk_in_s: f64,
    pub pk_out_s: f64,
    pub wifi: Option<Wifi>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TcpHealth {
    pub retrans_s: f64,
    /// % of sent segments that were retransmits
    pub retr_pct: f64,
    pub fail_min: f64,
    pub rst_min: f64,
    /// this PC's usual level (median of the last hour); None while the baseline is still learning
    pub usual: Option<Usual>,
}

impl TcpHealth {
    /// (retransmit %, failed connects, resets) levels: against this PC's usual level once the
    /// baseline has `baseline::MIN_SAMPLES` minutes, else against the fixed thresholds.
    pub fn levels(&self) -> (Lv, Lv, Lv) {
        match self.usual {
            Some(u) => (lvl(self.retr_pct, baseline::warn_hot(u.retr, true)), lvl(self.fail_min, baseline::warn_hot(u.fail, false)), lvl(self.rst_min, baseline::warn_hot(u.rst, false))),
            None => (lvl(self.retr_pct, TH_RETR), lvl(self.fail_min, TH_FAIL), lvl(self.rst_min, TH_RST)),
        }
    }
}

/// What the UI shows; empty until the first two reads are in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Health {
    pub adapters: Vec<AdapterHealth>,
    pub tcp: Option<TcpHealth>,
    /// seconds the diffs cover
    pub span_s: f64,
    /// the last hour of one-minute samples (oldest first), for the tile sparklines
    pub base: Series,
}

#[derive(Default)]
pub struct Hist {
    samples: VecDeque<Raw>,
    last_up: HashMap<u64, bool>,
    flaps: HashMap<u64, u32>,
    /// one-minute samples of the system TCP counters (see `baseline`); kept while the detail is hidden
    base: Ring,
}

impl Hist {
    /// Every sampler tick, from the adapter list it already has: counts up/down changes.
    pub fn observe_links<I: IntoIterator<Item = (u64, bool)>>(&mut self, nics: I) {
        for (luid, up) in nics {
            if let Some(prev) = self.last_up.insert(luid, up) {
                if prev != up {
                    *self.flaps.entry(luid).or_default() += 1;
                }
            }
        }
    }

    /// Add a read (ignored when it is the one already held).
    pub fn feed(&mut self, raw: Raw) {
        if let Some(l) = self.samples.back() {
            if l.at == raw.at {
                return;
            }
            if raw.at.saturating_duration_since(l.at) > GAP {
                self.samples.clear();
                self.base.gap();
            }
        }
        if let Some(t) = raw.tcp {
            self.base.observe(raw.at, t, baseline::sample_secs());
        }
        while self.samples.front().is_some_and(|f| raw.at.saturating_duration_since(f.at) > WINDOW + Duration::from_secs(1)) {
            self.samples.pop_front();
        }
        self.samples.push_back(raw);
    }

    /// The detail is not visible: forget the reads so a later diff never spans the gap.
    pub fn idle(&mut self) {
        self.samples.clear();
        // the minute being measured is dropped; finished minutes stay (they are real)
        self.base.gap();
    }

    pub fn health(&self) -> Health {
        let (Some(new), Some(old)) = (self.samples.back(), self.samples.front()) else { return Health { base: self.base.series(), ..Default::default() } };
        if self.samples.len() < 2 {
            return Health { base: self.base.series(), ..Default::default() };
        }
        let span = new.at.saturating_duration_since(old.at).as_secs_f64();
        let d = |a: u64, b: u64| a.saturating_sub(b);
        let mut adapters = Vec::with_capacity(new.ifs.len());
        for (luid, n) in &new.ifs {
            let o = old.ifs.iter().find(|(l, _)| l == luid).map(|(_, c)| *c).unwrap_or(*n);
            // packet rate over the last few seconds, not 60: it is a "now" number
            let recent = self.samples.iter().rev().find(|s| new.at.saturating_duration_since(s.at).as_secs_f64() >= 2.0).unwrap_or(old);
            let r = recent.ifs.iter().find(|(l, _)| l == luid).map(|(_, c)| *c).unwrap_or(*n);
            let rs = new.at.saturating_duration_since(recent.at).as_secs_f64();
            let rate = |a: u64, b: u64| if rs >= 0.5 { d(a, b) as f64 / rs } else { 0.0 };
            adapters.push(AdapterHealth {
                luid: *luid,
                up: n.up,
                flaps: self.flaps.get(luid).copied().unwrap_or(0),
                err_in: d(n.in_err, o.in_err),
                err_out: d(n.out_err, o.out_err),
                disc_in: d(n.in_disc, o.in_disc),
                disc_out: d(n.out_disc, o.out_disc),
                err_total: n.in_err.saturating_add(n.out_err),
                disc_total: n.in_disc.saturating_add(n.out_disc),
                pk_in_s: rate(n.in_pkts, r.in_pkts),
                pk_out_s: rate(n.out_pkts, r.out_pkts),
                wifi: new.wifi.iter().find(|(l, _)| l == luid).map(|(_, w)| w.clone()),
            });
        }
        let tcp = match (new.tcp, old.tcp) {
            (Some(n), Some(o)) if span >= MIN_SPAN => {
                let out = d(n.out_segs, o.out_segs) as f64;
                let re = d(n.retrans, o.retrans) as f64;
                Some(TcpHealth {
                    retrans_s: re / span,
                    retr_pct: if out > 0.0 { re / out * 100.0 } else { 0.0 },
                    fail_min: d(n.fails, o.fails) as f64 / span * 60.0,
                    rst_min: d(n.resets, o.resets) as f64 / span * 60.0,
                    usual: self.base.usual(),
                })
            }
            _ => None,
        };
        Health { adapters, tcp, span_s: span, base: self.base.series() }
    }
}

// ---------- verdict ----------

/// "0.8" / "14" / "212": one decimal below 100 ms.
pub fn f1(v: f32) -> String {
    if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") }
}

/// What the probes measured for the selected adapter (borrowed from the probe view).
#[derive(Clone, Copy, Debug)]
pub struct ProbeIn<'a> {
    /// None: the adapter has no gateway
    pub gw: Option<PathStats>,
    pub inet: PathStats,
    /// Off / NotRouted: no internet echo is sent from this adapter, `inet` is empty
    pub inet_state: InetState,
    pub dns: Dns,
    pub gw_addr: &'a str,
    pub inet_addr: &'a str,
    pub dns_addr: &'a str,
    pub dns_name: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub enum ProbeState<'a> {
    /// switch off: nothing is sent, nothing measured
    Off,
    /// switch on, no current result yet
    Waiting,
    On(ProbeIn<'a>),
}

/// Level of each probed layer of the chain.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Layers {
    pub gw: Lv,
    pub inet: Lv,
    pub dns: Lv,
    /// the gateway never answered an echo while the internet did: it ignores ping (common), so
    /// it is not blamed
    pub gw_silent: bool,
    /// the internet host never answered an echo while a DNS lookup did: ICMP to it is filtered
    pub inet_silent: bool,
}

pub fn layers(p: &ProbeIn) -> Layers {
    let probed = p.inet_state == InetState::Probed;
    let inet_ok = probed && p.inet.answered() > 0 && !p.inet.is_dead();
    // something past the gateway answered: the internet echo, or with no echo sent, the lookup
    let beyond_ok = inet_ok || (!probed && matches!(p.dns, Dns::Ms(_)));
    let gw_silent = p.gw.is_some_and(|g| g.never_answered()) && beyond_ok;
    let inet_silent = probed && p.inet.never_answered() && matches!(p.dns, Dns::Ms(_)) && !gw_silent;
    Layers {
        gw: if gw_silent { Lv::Ok } else { p.gw.as_ref().map_or(Lv::Ok, probe::gw_level) },
        // not probed: nothing to judge, never a fault
        inet: if inet_silent || !probed { Lv::Ok } else { probe::inet_level(&p.inet) },
        dns: probe::dns_level(p.dns),
        gw_silent,
        inet_silent,
    }
}

pub struct Inputs<'a> {
    pub adapter: &'a str,
    pub link_up: bool,
    /// Down and Up both under `IDLE_MBPS`
    pub zero_traffic: bool,
    pub errors: u64,
    pub discards: u64,
    /// (dBm, quality %) of a connected Wi-Fi adapter
    pub wifi: Option<(i32, u32)>,
    pub tcp: Option<TcpHealth>,
    pub probe: ProbeState<'a>,
    /// the one app that is a noticeable offender (`netconn::NOTICEABLE`), when ETW knows it
    pub offender: Option<Offender<'a>>,
    /// the ETW feed is live: with no offender named, no single app stands out
    pub etw_live: bool,
}

/// The worst failing app + remote, for the verdict to name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Offender<'a> {
    pub app: &'a str,
    pub ip: &'a str,
    pub port: u16,
    /// remote end is on the LAN (otherwise WAN / other)
    pub lan: bool,
    /// failed connects + drops in the last minute
    pub per_min: u32,
    pub kind: crate::netconn::Kind,
}

/// One sentence: what this failure kind usually means. Always an inference.
pub fn offender_cause(o: &Offender) -> String {
    use crate::netconn::Kind;
    match (o.kind, o.lan) {
        (Kind::NoReply, true) => format!("Nothing answers its connection attempts: the host at {} is off or asleep, a firewall silently drops port {}, or the address is wrong.", o.ip, o.port),
        (Kind::NoReply, false) => "The servers never answer its connection attempts: the service may be down, or a firewall or your ISP is dropping the packets.".into(),
        (Kind::Refused, _) => format!("The host answers at once and nothing accepts the connection: no program is listening on port {} at {}, or something is resetting it.", o.port, o.ip),
        (Kind::Midstream, _) => "Connections start fine, then packets are lost and they drop: a lossy path, a weak Wi-Fi link, or a server cutting the connection.".into(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub lv: Lv,
    pub label: String,
    pub cause: String,
    /// level of the chain's Adapter node (errors, discards, link, signal)
    pub adapter: Lv,
    /// levels of the probed nodes (None with probes off or no result yet)
    pub layers: Option<Layers>,
}

/// " (usual 12 failed/min, 3 resets/min)" once this PC's baseline exists, else empty.
fn usual_text(t: Option<TcpHealth>) -> String {
    t.and_then(|t| t.usual).map_or(String::new(), |u| format!(" (usual {:.0} failed/min, {:.0} resets/min)", u.fail, u.rst))
}

/// Failures above the fixed fallback limit but normal for this PC: said once so a busy machine
/// does not look unchecked. Empty while the baseline learns or when the counters are not high.
fn busy_note(t: Option<TcpHealth>) -> String {
    match t.filter(|t| t.usual.is_some() && t.fail_min >= TH_FAIL[0]) {
        Some(t) => format!(" Connection failures are high ({:.0}/min) but normal for this PC (usual {:.0}/min): nothing changed, so no warning.", t.fail_min, t.usual.map_or(0.0, |u| u.fail)),
        None => String::new(),
    }
}

/// The verdict, in the mock's order: offline, adapter errors, weak Wi-Fi, local latency (gateway),
/// WAN (gateway fine, internet bad), DNS, retransmits, failing connections, else healthy. With
/// probes off or still waiting for the first result, only the passive rules run and the text says so.
pub fn diagnose(i: &Inputs) -> Verdict {
    let l_err = lvl(i.errors as f64, TH_ERR);
    let l_disc = lvl(i.discards as f64, TH_DISC);
    let l_sig = i.wifi.map_or(Lv::Ok, |(r, _)| lvl_low(r as f64, TH_SIG));
    let (l_retr, l_fail, l_rst) = i.tcp.map_or((Lv::Ok, Lv::Ok, Lv::Ok), |t| t.levels());
    let adapter = [l_err, l_disc, l_sig, if i.link_up { Lv::Ok } else { Lv::Hot }].into_iter().max().unwrap_or_default();
    let p = match i.probe {
        ProbeState::On(p) => Some(p),
        _ => None,
    };
    let ly = p.as_ref().map(layers);
    let v = |lv, label: &str, cause: String| Verdict { lv, label: label.into(), cause, adapter, layers: ly };
    let fail = i.tcp.map_or(0.0, |t| t.fail_min);
    let gw_bad = ly.is_some_and(|l| l.gw > Lv::Ok);
    let in_bad = ly.is_some_and(|l| l.inet > Lv::Ok);
    let gw_txt = p.as_ref().and_then(|p| p.gw).map_or(String::new(), |g| format!("gateway {} ms avg, jitter {} ms, {:.0}% loss", g.avg.map_or("—".into(), f1), g.jitter.map_or("—".into(), f1), g.loss_pct));
    if !i.link_up {
        return v(Lv::Hot, "Offline: link down", "The adapter reports no link. Cable unplugged, port off, or Wi-Fi disconnected.".into());
    }
    if let (Some(p), Some(l)) = (&p, &ly) {
        let wifi = i.wifi.is_some();
        let probed = p.inet_state == InetState::Probed;
        // the gateway and the internet host both stopped answering while the link is up (with no
        // internet echo sent from this adapter, a failed lookup is the second witness)
        let beyond_dead = if probed { p.inet.is_dead() && !l.inet_silent } else { p.dns == Dns::Failed };
        if let Some(g) = p.gw.filter(|g| g.is_dead() && !l.gw_silent && beyond_dead) {
            return v(Lv::Hot, "Offline: gateway not reachable", format!("{} has not answered for {:.0} s while the link is up. Router off or restarting, a cable unplugged at its far end, or {}.", p.gw_addr, g.silent_s, if wifi { "Wi-Fi dropped" } else { "a dead switch port" }));
        }
        // no gateway on the adapter: only the internet host says whether anything is reachable
        if p.gw.is_none() && p.inet.is_dead() && !l.inet_silent {
            return v(Lv::Hot, "Offline: internet not reachable", format!("{} has not answered for {:.0} s and this adapter has no gateway to check.", p.inet_addr, p.inet.silent_s));
        }
        if p.inet.is_dead() && !l.inet_silent && !gw_bad {
            let g = p.gw.and_then(|g| g.avg).map_or(String::from("answers"), |a| format!("answers in {} ms", f1(a)));
            return v(Lv::Hot, "Offline: internet not reachable", format!("The gateway {g} but {} has not answered for {:.0} s. The problem is past your router: ISP or WAN outage.", p.inet_addr, p.inet.silent_s));
        }
    } else if i.zero_traffic && l_fail == Lv::Hot {
        let how = if matches!(i.probe, ProbeState::Waiting) { "Probes are on: the first results are a few seconds away." } else { "Turn probes on to see whether the gateway answers." };
        return v(Lv::Hot, "Offline: no traffic, connections failing", format!("The link is up but nothing is moving and {fail:.0} connection attempts per minute fail. {how}"));
    }
    if l_err > Lv::Ok || l_disc > Lv::Ok {
        let loss = p.as_ref().and_then(|p| p.gw).filter(|g| g.loss_pct > 0.0).map_or(String::new(), |g| format!(", and pings lose {:.0}%", g.loss_pct));
        return v(Lv::Warn, "Degraded: adapter errors", format!("Errors and discards are climbing on {} (+{} errors, +{} discards in 60 s){loss}. That points at the cable, port or NIC, not the ISP.", i.adapter, i.errors, i.discards));
    }
    if let (Some((rssi, q)), true) = (i.wifi, l_sig > Lv::Ok && (p.is_none() || gw_bad)) {
        let with = if gw_bad { format!(", {gw_txt}") } else { String::new() };
        return v(Lv::Warn, "Degraded: Wi-Fi signal weak", format!("Signal is {rssi} dBm ({q}% quality){with}. The problem is between this PC and the router: move closer, or use 5 GHz."));
    }
    if let (Some(_), true) = (&p, gw_bad) {
        let hint = if i.wifi.is_some() { "Check Wi-Fi interference." } else { "Check the cable, the switch and router load." };
        return v(Lv::Warn, "Degraded: local latency high", format!("{gw_txt}. The problem is between this PC and your router, not the ISP. {hint}"));
    }
    if let (Some(p), true) = (&p, in_bad) {
        let g = p.gw.and_then(|g| g.avg).map_or(String::from("answers"), |a| format!("is fine ({} ms)", f1(a)));
        let label = if p.inet.loss_pct >= probe::TH_LOSS[0] as f32 { "Degraded: WAN packet loss" } else { "Degraded: WAN latency high" };
        return v(Lv::Warn, label, format!("Gateway {g} but {} shows {} ms avg, {:.0}% loss. The problem is past your router: ISP or WAN.", p.inet_addr, p.inet.avg.map_or("—".into(), f1), p.inet.loss_pct));
    }
    if let (Some(p), Some(l)) = (&p, &ly) {
        if l.dns > Lv::Ok {
            return match p.dns {
                Dns::Ms(ms) => v(Lv::Warn, "Degraded: DNS slow", format!("Pings are fine but a name lookup takes {} ms via {}. The resolver is slow: try 1.1.1.1 or 8.8.8.8 as DNS.", f1(ms), p.dns_addr)),
                _ => v(Lv::Warn, "Degraded: DNS not answering", format!("Pings are fine but the lookup of {} through {} failed. The resolver is down or filtering: try 1.1.1.1 or 8.8.8.8 as DNS.", p.dns_name, p.dns_addr)),
            };
        }
    }
    if l_retr > Lv::Ok {
        let pc = i.tcp.map_or(0.0, |t| t.retr_pct);
        let how = match i.probe {
            ProbeState::On(_) => "The probes look clean, so it may be one server or short bursts.",
            _ => "Turn probes on to find where.",
        };
        return v(Lv::Warn, "Degraded: TCP retransmits high", format!("{pc:.1}% of sent TCP segments are resent: packets are lost somewhere on the path. {how}"));
    }
    if l_fail > Lv::Ok || l_rst > Lv::Ok {
        let r = i.tcp.map_or(0.0, |t| t.rst_min);
        if let Some(o) = i.offender {
            let ip = if o.ip.contains(':') { format!("[{}]:{}", o.ip, o.port) } else { format!("{}:{}", o.ip, o.port) };
            let what = if o.kind == crate::netconn::Kind::Midstream { "dropping connections to" } else { "failing to reach" };
            return v(Lv::Warn, "Degraded: connections failing", format!("{} {what} {ip}, {} {}/min, {} (inferred). {} System-wide: {fail:.0} failed attempts and {r:.0} resets per minute{usual}.", o.app, o.per_min, if o.kind == crate::netconn::Kind::Midstream { "dropped" } else { "failed" }, o.kind.label().to_lowercase(), offender_cause(&o), usual = usual_text(i.tcp)));
        }
        if i.etw_live {
            return v(Lv::Warn, "Degraded: connections failing", format!("{fail:.0} failed attempts and {r:.0} resets per minute{usual}, spread over many small sources: no single app stands out.", usual = usual_text(i.tcp)));
        }
        return v(Lv::Warn, "Degraded: connections failing", format!("{fail:.0} failed attempts and {r:.0} resets per minute{usual}, system-wide. A remote service or a firewall rule may be refusing connections.", usual = usual_text(i.tcp)));
    }
    let busy = busy_note(i.tcp);
    match (&p, &ly) {
        (Some(p), Some(l)) => {
            let gw = match p.gw {
                _ if l.gw_silent => "Gateway ignores ping".to_string(),
                Some(g) => format!("Gateway {} ms", g.avg.map_or("—".into(), f1)),
                None => "No gateway on this adapter".to_string(),
            };
            let inet = match p.inet_state {
                InetState::Off => "internet not probed (internet = none)".to_string(),
                InetState::NotRouted => "internet not probed on this adapter".to_string(),
                InetState::Probed if l.inet_silent => "internet host ignores ping".to_string(),
                InetState::Probed => format!("internet {} ms", p.inet.avg.map_or("—".into(), f1)),
            };
            let dns = match p.dns {
                Dns::Ms(ms) => format!("DNS {} ms", f1(ms)),
                Dns::Local => "DNS is a local resolver (not probed)".into(),
                _ => "no DNS server to ask".into(),
            };
            v(Lv::Ok, "Healthy", format!("{gw}, {inet}, {dns}, no loss, no adapter errors.{busy}"))
        }
        _ if matches!(i.probe, ProbeState::Waiting) => v(Lv::Ok, "Healthy", format!("No errors, link up, no retransmits. Probes are on: latency, loss and DNS results arrive in a few seconds.{busy}")),
        _ => v(Lv::Ok, "Healthy", format!("No errors, link up, no retransmits. Passive checks only: latency, loss and DNS are not tested while probes are off.{busy}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Inputs<'static> {
        Inputs { adapter: "Ethernet 3", link_up: true, zero_traffic: false, errors: 0, discards: 0, wifi: None, tcp: Some(TcpHealth::default()), probe: ProbeState::Off, offender: None, etw_live: false }
    }

    #[test]
    fn levels_use_the_mock_thresholds() {
        assert_eq!(lvl(0.0, TH_ERR), Lv::Ok);
        assert_eq!(lvl(1.0, TH_ERR), Lv::Warn);
        assert_eq!(lvl(10.0, TH_ERR), Lv::Hot);
        assert_eq!(lvl(0.9, TH_RETR), Lv::Ok);
        assert_eq!(lvl(3.0, TH_RETR), Lv::Hot);
        assert_eq!(lvl(f64::NAN, TH_RETR), Lv::Ok);
        assert_eq!(lvl_low(-60.0, TH_SIG), Lv::Ok);
        assert_eq!(lvl_low(-67.0, TH_SIG), Lv::Warn);
        assert_eq!(lvl_low(-75.0, TH_SIG), Lv::Hot);
        assert_eq!(lvl(2.0, TH_FLAP), Lv::Warn);
    }

    #[test]
    fn clean_passive_data_is_healthy() {
        let d = diagnose(&ok());
        assert_eq!((d.lv, d.label.as_str()), (Lv::Ok, "Healthy"));
        assert!(d.cause.contains("not tested"));
    }

    #[test]
    fn no_link_is_offline_before_anything_else() {
        let d = diagnose(&Inputs { link_up: false, errors: 50, ..ok() });
        assert_eq!((d.lv, d.adapter), (Lv::Hot, Lv::Hot));
        assert!(d.label.starts_with("Offline"));
    }

    #[test]
    fn idle_link_with_failing_connects_is_offline() {
        let t = TcpHealth { fail_min: 100.0, ..Default::default() };
        let d = diagnose(&Inputs { zero_traffic: true, tcp: Some(t), ..ok() });
        assert_eq!(d.lv, Lv::Hot);
        assert!(d.cause.contains("100"));
        // failing connects with traffic flowing is only degraded
        let d = diagnose(&Inputs { tcp: Some(t), ..ok() });
        assert_eq!((d.lv, d.label.as_str()), (Lv::Warn, "Degraded: connections failing"));
    }

    #[test]
    fn adapter_errors_beat_weak_wifi_and_retransmits() {
        let t = TcpHealth { retr_pct: 5.0, ..Default::default() };
        let d = diagnose(&Inputs { errors: 14, discards: 6, wifi: Some((-80, 20)), tcp: Some(t), ..ok() });
        assert_eq!(d.label, "Degraded: adapter errors");
        assert!(d.cause.contains("+14 errors, +6 discards"));
        assert_eq!(d.adapter, Lv::Hot);
    }

    #[test]
    fn weak_wifi_beats_retransmits() {
        let t = TcpHealth { retr_pct: 5.0, ..Default::default() };
        let d = diagnose(&Inputs { wifi: Some((-78, 31)), tcp: Some(t), ..ok() });
        assert_eq!(d.label, "Degraded: Wi-Fi signal weak");
        assert!(d.cause.contains("-78 dBm (31% quality)"));
        assert_eq!(d.adapter, Lv::Hot);
        // a strong signal is not a cause
        assert_eq!(diagnose(&Inputs { wifi: Some((-50, 90)), ..ok() }).lv, Lv::Ok);
    }

    #[test]
    fn retransmits_and_resets_are_degraded_not_offline() {
        let t = TcpHealth { retr_pct: 1.6, ..Default::default() };
        assert_eq!(diagnose(&Inputs { tcp: Some(t), ..ok() }).label, "Degraded: TCP retransmits high");
        let t = TcpHealth { rst_min: 70.0, ..Default::default() };
        assert_eq!(diagnose(&Inputs { tcp: Some(t), ..ok() }).label, "Degraded: connections failing");
        // no TCP numbers yet: nothing to blame
        assert_eq!(diagnose(&Inputs { tcp: None, ..ok() }).lv, Lv::Ok);
    }

    #[test]
    fn verdict_names_the_offender_only_when_the_system_counter_is_high() {
        use crate::netconn::Kind;
        let o = |kind, lan| Offender { app: "OpenJDK Platform binary", ip: "10.0.0.2", port: 25565, lan, per_min: 61, kind };
        let hot = TcpHealth { fail_min: 100.0, ..Default::default() };
        let d = diagnose(&Inputs { tcp: Some(hot), offender: Some(o(Kind::NoReply, true)), ..ok() });
        assert_eq!((d.lv, d.label.as_str()), (Lv::Warn, "Degraded: connections failing"));
        assert!(d.cause.starts_with("OpenJDK Platform binary failing to reach 10.0.0.2:25565, 61 failed/min, no reply (inferred)."), "{}", d.cause);
        assert!(d.cause.contains("host at 10.0.0.2 is off or asleep"));
        let d = diagnose(&Inputs { tcp: Some(hot), offender: Some(o(Kind::Refused, true)), ..ok() });
        assert!(d.cause.contains("refused / reset (inferred)") && d.cause.contains("no program is listening on port 25565"));
        let d = diagnose(&Inputs { tcp: Some(hot), offender: Some(o(Kind::NoReply, false)), ..ok() });
        assert!(d.cause.contains("servers never answer"));
        let d = diagnose(&Inputs { tcp: Some(hot), offender: Some(o(Kind::Midstream, false)), ..ok() });
        assert!(d.cause.contains("dropping connections to") && d.cause.contains("61 dropped/min") && d.cause.contains("lossy path"));
        // system counters low: an offender alone does not change a healthy verdict
        assert_eq!(diagnose(&Inputs { offender: Some(o(Kind::NoReply, true)), ..ok() }).lv, Lv::Ok);
        // v6 remote is bracketed
        let v6 = Offender { ip: "2001:db8::1", ..o(Kind::NoReply, false) };
        assert!(diagnose(&Inputs { tcp: Some(hot), offender: Some(v6), ..ok() }).cause.contains("[2001:db8::1]:25565"));
        // no offender: today's text
        assert!(diagnose(&Inputs { tcp: Some(hot), ..ok() }).cause.contains("A remote service or a firewall rule"));
    }

    fn tcp_with(fail: f64, rst: f64, retr: f64, usual: Option<(f64, f64, f64)>) -> TcpHealth {
        TcpHealth { fail_min: fail, rst_min: rst, retr_pct: retr, usual: usual.map(|(fail, rst, retr)| Usual { fail, rst, retr }), ..Default::default() }
    }

    /// (failed, resets, retransmit %) levels for each case: a busy-normal machine, a spike above
    /// usual, a quiet machine, and the learning fallback to the fixed thresholds.
    #[test]
    fn levels_are_judged_against_the_usual_level_once_it_exists() {
        // busy machine: 89/min against a usual 84 is normal (WARN would need 252)
        assert_eq!(tcp_with(89.0, 30.0, 0.5, Some((84.0, 28.0, 0.4))).levels(), (Lv::Ok, Lv::Ok, Lv::Ok));
        // ...and the same numbers on the fixed thresholds would have been HOT
        assert_eq!(tcp_with(89.0, 30.0, 0.5, None).levels().1, Lv::Warn);
        // spike above usual: 92/min against a usual 12 (WARN at 36, HOT at 96)
        assert_eq!(tcp_with(92.0, 3.0, 0.1, Some((12.0, 3.0, 0.1))).levels().1, Lv::Warn);
        assert_eq!(tcp_with(96.0, 3.0, 0.1, Some((12.0, 3.0, 0.1))).levels().1, Lv::Hot);
        // quiet machine: the absolute margin (+10 / +40) decides, not 3 x 0
        assert_eq!(tcp_with(9.0, 0.0, 0.0, Some((0.0, 0.0, 0.0))).levels().1, Lv::Ok);
        assert_eq!(tcp_with(10.0, 0.0, 0.0, Some((0.0, 0.0, 0.0))).levels().1, Lv::Warn);
        assert_eq!(tcp_with(40.0, 0.0, 0.0, Some((0.0, 0.0, 0.0))).levels().1, Lv::Hot);
        // resets and retransmit % use the same rule (points for the %)
        assert_eq!(tcp_with(0.0, 12.0, 0.0, Some((0.0, 0.0, 0.0))).levels().2, Lv::Warn);
        assert_eq!(tcp_with(0.0, 0.0, 1.5, Some((0.0, 0.0, 0.0))).levels().0, Lv::Warn);
        assert_eq!(tcp_with(0.0, 0.0, 6.0, Some((0.0, 0.0, 0.0))).levels().0, Lv::Hot);
        // learning: today's fixed thresholds [30, 90] / [60, 180] / [1, 3]
        assert_eq!(tcp_with(29.0, 59.0, 0.9, None).levels(), (Lv::Ok, Lv::Ok, Lv::Ok));
        assert_eq!(tcp_with(30.0, 60.0, 1.0, None).levels(), (Lv::Warn, Lv::Warn, Lv::Warn));
        assert_eq!(tcp_with(90.0, 180.0, 3.0, None).levels(), (Lv::Hot, Lv::Hot, Lv::Hot));
    }

    #[test]
    fn a_busy_but_normal_machine_stays_healthy_even_with_a_noticeable_offender() {
        use crate::netconn::Kind;
        let o = Offender { app: "OpenJDK Platform binary", ip: "10.0.0.2", port: 25565, lan: true, per_min: 61, kind: Kind::NoReply };
        let normal = tcp_with(89.0, 30.0, 0.5, Some((84.0, 28.0, 0.4)));
        let d = diagnose(&Inputs { tcp: Some(normal), offender: Some(o), etw_live: true, ..ok() });
        assert_eq!((d.lv, d.label.as_str()), (Lv::Ok, "Healthy"));
        assert!(d.cause.contains("high (89/min) but normal for this PC (usual 84/min)"), "{}", d.cause);
        assert!(!d.cause.contains("OpenJDK"), "no culprit is named when nothing is above usual");
        // the same offender with a real spike is named, and the usual level is quoted
        let spike = tcp_with(92.0, 4.0, 0.4, Some((12.0, 3.0, 0.4)));
        let d = diagnose(&Inputs { tcp: Some(spike), offender: Some(o), etw_live: true, ..ok() });
        assert_eq!((d.lv, d.label.as_str()), (Lv::Warn, "Degraded: connections failing"));
        assert!(d.cause.starts_with("OpenJDK Platform binary failing to reach 10.0.0.2:25565"), "{}", d.cause);
        assert!(d.cause.contains("(usual 12 failed/min, 3 resets/min)"), "{}", d.cause);
    }

    #[test]
    fn a_spike_with_no_single_offender_says_spread_or_system_wide() {
        let spike = tcp_with(92.0, 4.0, 0.4, Some((12.0, 3.0, 0.4)));
        // ETW live, nobody noticeable: spread over many small sources
        let d = diagnose(&Inputs { tcp: Some(spike), etw_live: true, ..ok() });
        assert!(d.cause.contains("spread over many small sources") && d.cause.contains("usual 12 failed/min"), "{}", d.cause);
        // no ETW (not admin): the original system-wide sentence
        let d = diagnose(&Inputs { tcp: Some(spike), ..ok() });
        assert!(d.cause.contains("system-wide. A remote service or a firewall rule"), "{}", d.cause);
    }

    #[test]
    fn the_learning_fallback_keeps_todays_behaviour() {
        use crate::netconn::Kind;
        let o = Offender { app: "x.exe", ip: "10.0.0.2", port: 80, lan: true, per_min: 40, kind: Kind::Refused };
        // 89/min with no baseline yet: the fixed threshold applies (30 / 90), so it warns and names the app
        let d = diagnose(&Inputs { tcp: Some(tcp_with(89.0, 0.0, 0.0, None)), offender: Some(o), etw_live: true, ..ok() });
        assert_eq!(d.lv, Lv::Warn);
        assert!(d.cause.starts_with("x.exe failing to reach 10.0.0.2:80") && !d.cause.contains("usual"), "{}", d.cause);
        // 25/min is below the fixed limit and stays Healthy
        assert_eq!(diagnose(&Inputs { tcp: Some(tcp_with(25.0, 0.0, 0.0, None)), offender: Some(o), etw_live: true, ..ok() }).lv, Lv::Ok);
    }

    #[test]
    fn hist_feeds_the_baseline_one_sample_a_minute_while_reads_arrive() {
        let t0 = Instant::now();
        let mut h = Hist::default();
        // reads every second for 61 s: 60 failed connects per minute
        for s in 0..=61u64 {
            h.feed(raw(t0 + Duration::from_secs(s), 0, 0, s, 100 * s));
        }
        let x = h.health();
        assert_eq!(x.base.len(), 1, "one finished minute");
        assert!(x.tcp.unwrap().usual.is_none(), "1 of 15: still learning");
        // the detail is hidden: the minute being measured is dropped, the finished one stays
        h.idle();
        assert_eq!(h.health().base.len(), 1);
        assert_eq!(h.health().adapters.len(), 0);
    }

    fn raw(at: Instant, e: u64, pk: u64, retr: u64, out: u64) -> Raw {
        Raw {
            at,
            ifs: vec![(7, IfCtr { up: true, in_err: e, in_pkts: pk, out_pkts: pk / 2, ..Default::default() })],
            tcp: Some(TcpCtr { retrans: retr, out_segs: out, fails: retr, resets: 0 }),
            wifi: vec![],
        }
    }

    #[test]
    fn hist_diffs_counters_over_the_window() {
        let t0 = Instant::now();
        let mut h = Hist::default();
        assert_eq!(h.health(), Health::default());
        for s in 0..=4u64 {
            h.feed(raw(t0 + Duration::from_secs(s), 10 + s, 1000 * s, 2 * s, 100 * s));
        }
        let x = h.health();
        assert_eq!(x.span_s, 4.0);
        let a = &x.adapters[0];
        assert_eq!((a.err_in, a.err_total), (4, 14));
        assert_eq!(a.pk_in_s, 1000.0);
        let t = x.tcp.unwrap();
        assert!((t.retr_pct - 2.0).abs() < 1e-9);
        assert!((t.retrans_s - 2.0).abs() < 1e-9);
        assert!((t.fail_min - 120.0).abs() < 1e-9);
    }

    #[test]
    fn hist_drops_reads_across_a_gap_and_a_counter_reset() {
        let t0 = Instant::now();
        let mut h = Hist::default();
        h.feed(raw(t0, 0, 0, 0, 0));
        h.feed(raw(t0 + Duration::from_secs(1), 0, 0, 0, 0));
        // the detail was hidden for a minute: start over
        h.feed(raw(t0 + Duration::from_secs(70), 5, 0, 0, 0));
        assert_eq!(h.health(), Health::default());
        h.feed(raw(t0 + Duration::from_secs(71), 5, 0, 0, 0));
        assert_eq!(h.health().span_s, 1.0);
        // counters that went back (adapter reset) never underflow
        h.feed(raw(t0 + Duration::from_secs(72), 1, 0, 0, 0));
        assert_eq!(h.health().adapters[0].err_in, 0);
        // the same read twice changes nothing
        let n = h.samples.len();
        h.feed(raw(t0 + Duration::from_secs(72), 1, 0, 0, 0));
        assert_eq!(h.samples.len(), n);
        h.idle();
        assert_eq!(h.health(), Health::default());
    }

    #[test]
    fn hist_keeps_only_the_window() {
        let t0 = Instant::now();
        let mut h = Hist::default();
        for s in 0..120u64 {
            h.feed(raw(t0 + Duration::from_secs(s), s, 0, 0, 0));
        }
        let x = h.health();
        assert!(x.span_s <= 61.0 && x.span_s >= 59.0);
        assert!(h.samples.len() <= 62);
    }

    #[test]
    fn flaps_count_up_down_changes_only() {
        let mut h = Hist::default();
        h.observe_links([(1, true), (2, true)]);
        h.observe_links([(1, true), (2, false)]);
        h.observe_links([(1, true), (2, true)]);
        assert_eq!(h.flaps.get(&2), Some(&2));
        assert_eq!(h.flaps.get(&1), None);
    }

    #[test]
    fn wifi_band_from_channel() {
        let w = |c| Wifi { channel: c, ..Default::default() }.band();
        assert_eq!((w(6), w(36), w(149), w(0)), ("2.4 GHz", "5 GHz", "5 GHz", ""));
    }

    // ---- verdict with probes ----

    /// 30 rounds: `avg` ms with `jit` ms of alternation, `lost_tail` unanswered at the end.
    fn path(avg: f32, jit: f32, loss: u32, dead: u32) -> PathStats {
        let n = 30u32;
        PathStats {
            sent: n,
            lost: loss.max(dead),
            now: (dead == 0).then_some(avg),
            avg: Some(avg),
            min: Some(avg - jit),
            max: Some(avg + jit),
            jitter: Some(jit),
            loss_pct: loss.max(dead) as f32 / n as f32 * 100.0,
            dead,
            silent_s: dead as f32 * 2.0,
        }
    }
    fn dead_path(rounds: u32) -> PathStats {
        PathStats { sent: rounds, lost: rounds, loss_pct: 100.0, dead: rounds, silent_s: rounds as f32 * 2.0, ..Default::default() }
    }
    fn good() -> ProbeIn<'static> {
        ProbeIn { gw: Some(path(0.8, 0.3, 0, 0)), inet: path(12.0, 2.0, 0, 0), inet_state: InetState::Probed, dns: Dns::Ms(14.0), gw_addr: "10.0.0.1", inet_addr: "1.1.1.1", dns_addr: "10.0.0.1", dns_name: "example.com" }
    }
    fn with(p: ProbeIn<'static>) -> Inputs<'static> {
        Inputs { probe: ProbeState::On(p), ..ok() }
    }

    #[test]
    fn probes_off_or_waiting_say_so_and_run_only_passive_rules() {
        let d = diagnose(&ok());
        assert!(d.layers.is_none() && d.cause.contains("not tested while probes are off"));
        let d = diagnose(&Inputs { probe: ProbeState::Waiting, ..ok() });
        assert_eq!((d.lv, d.label.as_str()), (Lv::Ok, "Healthy"));
        assert!(d.layers.is_none() && d.cause.contains("Probes are on"), "{}", d.cause);
        // idle + failing connects with probes on but no result yet still reads offline, and says why it waits
        let t = TcpHealth { fail_min: 100.0, ..Default::default() };
        let d = diagnose(&Inputs { zero_traffic: true, tcp: Some(t), probe: ProbeState::Waiting, ..ok() });
        assert!(d.label.starts_with("Offline") && d.cause.contains("first results"));
        assert!(diagnose(&Inputs { zero_traffic: true, tcp: Some(t), ..ok() }).cause.contains("Turn probes on"));
    }

    #[test]
    fn healthy_with_probes_lists_all_three_layers() {
        let d = diagnose(&with(good()));
        assert_eq!((d.lv, d.label.as_str()), (Lv::Ok, "Healthy"));
        assert_eq!(d.cause, "Gateway 0.8 ms, internet 12.0 ms, DNS 14.0 ms, no loss, no adapter errors.");
        assert_eq!(d.layers, Some(Layers::default()));
    }

    #[test]
    fn gateway_silent_with_the_internet_dead_is_offline_gateway_unreachable() {
        let p = ProbeIn { gw: Some(dead_path(5)), inet: dead_path(5), dns: Dns::Failed, ..good() };
        let d = diagnose(&with(p));
        assert_eq!((d.lv, d.label.as_str()), (Lv::Hot, "Offline: gateway not reachable"));
        assert!(d.cause.contains("10.0.0.1 has not answered for 10 s"), "{}", d.cause);
        assert!(d.cause.contains("dead switch port"));
        let wifi = Inputs { wifi: Some((-60, 80)), ..with(p) };
        assert!(diagnose(&wifi).cause.contains("Wi-Fi dropped"));
        assert_eq!(d.layers.unwrap().gw, Lv::Hot);
        // two missed rounds are not "dead" yet
        let p = ProbeIn { gw: Some(path(0.8, 0.3, 2, 2)), inet: path(12.0, 2.0, 2, 2), ..good() };
        assert_ne!(diagnose(&with(p)).label, "Offline: gateway not reachable");
    }

    #[test]
    fn a_gateway_that_ignores_ping_is_not_blamed() {
        let p = ProbeIn { gw: Some(dead_path(30)), ..good() };
        let d = diagnose(&with(p));
        assert_eq!((d.lv, d.label.as_str()), (Lv::Ok, "Healthy"));
        assert!(d.cause.starts_with("Gateway ignores ping"), "{}", d.cause);
        let l = d.layers.unwrap();
        assert!(l.gw_silent && l.gw == Lv::Ok);
    }

    #[test]
    fn a_dead_internet_behind_a_working_gateway_is_offline_internet() {
        let p = ProbeIn { inet: dead_path(8), dns: Dns::Failed, ..good() };
        let d = diagnose(&with(p));
        assert_eq!((d.lv, d.label.as_str()), (Lv::Hot, "Offline: internet not reachable"));
        assert!(d.cause.contains("past your router") && d.cause.contains("1.1.1.1 has not answered for 16 s"), "{}", d.cause);
        // ICMP to the host is filtered but DNS works: not an outage
        let p = ProbeIn { inet: dead_path(8), dns: Dns::Ms(20.0), ..good() };
        let d = diagnose(&with(p));
        assert_eq!(d.lv, Lv::Ok);
        assert!(d.layers.unwrap().inet_silent && d.cause.contains("internet host ignores ping"));
        // no gateway on the adapter: the internet host alone decides
        let p = ProbeIn { gw: None, inet: dead_path(8), dns: Dns::NotProbed, ..good() };
        assert_eq!(diagnose(&with(p)).label, "Offline: internet not reachable");
        assert!(diagnose(&with(ProbeIn { gw: None, dns: Dns::NotProbed, ..good() })).cause.contains("No gateway on this adapter"));
    }

    #[test]
    fn an_internet_that_is_not_probed_is_never_offline() {
        let np = |state, gw: Option<PathStats>, dns| ProbeIn { gw, inet: PathStats::default(), inet_state: state, dns, ..good() };
        for state in [InetState::NotRouted, InetState::Off] {
            // VPN / wrong adapter: the gateway answers, no internet echo is sent
            let d = diagnose(&with(np(state, Some(path(0.8, 0.3, 0, 0)), Dns::Ms(14.0))));
            assert_eq!((d.lv, d.label.as_str()), (Lv::Ok, "Healthy"), "{state:?}");
            assert_eq!(d.layers.unwrap().inet, Lv::Ok);
            // even with no DNS server at all the missing internet echo is not a fault
            assert_eq!(diagnose(&with(np(state, Some(path(0.8, 0.3, 0, 0)), Dns::NotProbed))).lv, Lv::Ok);
            assert_eq!(diagnose(&with(np(state, None, Dns::NotProbed))).lv, Lv::Ok);
        }
        let d = diagnose(&with(np(InetState::NotRouted, Some(path(0.8, 0.3, 0, 0)), Dns::Ms(14.0))));
        assert!(d.cause.contains("internet not probed on this adapter"), "{}", d.cause);
        let d = diagnose(&with(np(InetState::Off, Some(path(0.8, 0.3, 0, 0)), Dns::Ms(14.0))));
        assert!(d.cause.contains("internet not probed (internet = none)"), "{}", d.cause);
        // a dead gateway whose DNS lookup fails too is still an outage, with no internet echo
        let d = diagnose(&with(np(InetState::NotRouted, Some(dead_path(5)), Dns::Failed)));
        assert_eq!(d.label, "Offline: gateway not reachable");
        // a gateway that ignores ping while DNS answers is not blamed
        let d = diagnose(&with(np(InetState::Off, Some(dead_path(30)), Dns::Ms(14.0))));
        assert_eq!((d.lv, d.label.as_str()), (Lv::Ok, "Healthy"));
        assert!(d.layers.unwrap().gw_silent);
        // the old rule still holds when the internet IS probed
        let p = ProbeIn { inet: dead_path(8), dns: Dns::Failed, ..good() };
        assert_eq!(diagnose(&with(p)).label, "Offline: internet not reachable");
    }

    #[test]
    fn a_local_resolver_is_not_called_missing() {
        let d = diagnose(&with(ProbeIn { dns: Dns::Local, ..good() }));
        assert_eq!((d.lv, d.label.as_str()), (Lv::Ok, "Healthy"));
        assert!(d.cause.contains("DNS is a local resolver (not probed)") && !d.cause.contains("no DNS server"), "{}", d.cause);
        assert_eq!(d.layers.unwrap().dns, Lv::Ok);
    }

    #[test]
    fn link_down_beats_everything() {
        let p = ProbeIn { gw: Some(dead_path(9)), inet: dead_path(9), ..good() };
        let d = diagnose(&Inputs { link_up: false, ..with(p) });
        assert_eq!(d.label, "Offline: link down");
    }

    #[test]
    fn adapter_errors_mention_ping_loss() {
        let p = ProbeIn { gw: Some(path(1.0, 0.5, 3, 0)), ..good() };
        let d = diagnose(&Inputs { errors: 14, discards: 6, ..with(p) });
        assert_eq!(d.label, "Degraded: adapter errors");
        assert!(d.cause.contains("+14 errors, +6 discards in 60 s), and pings lose 10%"), "{}", d.cause);
        // without loss the text is the passive one
        assert!(!diagnose(&Inputs { errors: 14, ..with(good()) }).cause.contains("pings lose"));
    }

    #[test]
    fn weak_wifi_is_blamed_only_when_the_gateway_is_bad() {
        let weak = Some((-78, 31));
        // gateway fine: the signal number alone is not the cause
        assert_eq!(diagnose(&Inputs { wifi: weak, ..with(good()) }).label, "Healthy");
        // gateway slow too: Wi-Fi it is, with the numbers
        let p = ProbeIn { gw: Some(path(42.0, 9.0, 2, 0)), ..good() };
        let d = diagnose(&Inputs { wifi: weak, ..with(p) });
        assert_eq!(d.label, "Degraded: Wi-Fi signal weak");
        assert!(d.cause.contains("-78 dBm (31% quality), gateway 42.0 ms avg, jitter 9.0 ms, 7% loss"), "{}", d.cause);
    }

    #[test]
    fn slow_gateway_is_local_latency() {
        let p = ProbeIn { gw: Some(path(22.0, 1.0, 0, 0)), ..good() };
        let d = diagnose(&with(p));
        assert_eq!((d.lv, d.label.as_str()), (Lv::Warn, "Degraded: local latency high"));
        assert!(d.cause.contains("gateway 22.0 ms avg") && d.cause.contains("not the ISP") && d.cause.contains("cable"), "{}", d.cause);
        assert_eq!(d.layers.unwrap().gw, Lv::Warn);
        // jitter alone, loss alone
        assert_eq!(diagnose(&with(ProbeIn { gw: Some(path(1.0, 6.0, 0, 0)), ..good() })).label, "Degraded: local latency high");
        assert_eq!(diagnose(&with(ProbeIn { gw: Some(path(1.0, 0.5, 2, 0)), ..good() })).label, "Degraded: local latency high");
        assert!(diagnose(&Inputs { wifi: Some((-50, 90)), ..with(ProbeIn { gw: Some(path(60.0, 1.0, 0, 0)), ..good() }) }).cause.contains("Wi-Fi interference"));
    }

    #[test]
    fn good_gateway_but_bad_internet_is_the_isp() {
        let p = ProbeIn { inet: path(240.0, 5.0, 0, 0), ..good() };
        let d = diagnose(&with(p));
        assert_eq!((d.lv, d.label.as_str()), (Lv::Warn, "Degraded: WAN latency high"));
        assert!(d.cause.contains("Gateway is fine (0.8 ms)") && d.cause.contains("1.1.1.1 shows 240 ms avg") && d.cause.contains("ISP or WAN"), "{}", d.cause);
        assert_eq!(d.layers.unwrap(), Layers { inet: Lv::Hot, ..Default::default() });
        let p = ProbeIn { inet: path(15.0, 2.0, 6, 0), ..good() };
        let d = diagnose(&with(p));
        assert_eq!(d.label, "Degraded: WAN packet loss");
        assert!(d.cause.contains("20% loss"));
        // a bad gateway comes first: the internet numbers are then its consequence
        let p = ProbeIn { gw: Some(path(60.0, 1.0, 0, 0)), inet: path(240.0, 5.0, 0, 0), ..good() };
        assert_eq!(diagnose(&with(p)).label, "Degraded: local latency high");
    }

    #[test]
    fn slow_or_failing_dns_with_good_pings_is_the_resolver() {
        let d = diagnose(&with(ProbeIn { dns: Dns::Ms(212.0), ..good() }));
        assert_eq!((d.lv, d.label.as_str()), (Lv::Warn, "Degraded: DNS slow"));
        assert!(d.cause.contains("212 ms via 10.0.0.1") && d.cause.contains("1.1.1.1 or 8.8.8.8"), "{}", d.cause);
        assert_eq!(d.layers.unwrap().dns, Lv::Warn);
        assert_eq!(diagnose(&with(ProbeIn { dns: Dns::Ms(640.0), ..good() })).layers.unwrap().dns, Lv::Hot);
        let d = diagnose(&with(ProbeIn { dns: Dns::Failed, ..good() }));
        assert_eq!(d.label, "Degraded: DNS not answering");
        assert!(d.cause.contains("example.com") && d.cause.contains("10.0.0.1"));
        assert_eq!(d.layers.unwrap().dns, Lv::Hot);
        // an adapter without a DNS server has nothing to blame
        assert_eq!(diagnose(&with(ProbeIn { dns: Dns::NotProbed, ..good() })).lv, Lv::Ok);
    }

    #[test]
    fn retransmits_with_clean_probes_point_at_one_server() {
        let t = TcpHealth { retr_pct: 4.0, ..Default::default() };
        let d = diagnose(&Inputs { tcp: Some(t), ..with(good()) });
        assert_eq!(d.label, "Degraded: TCP retransmits high");
        assert!(d.cause.contains("probes look clean"));
        assert!(diagnose(&Inputs { tcp: Some(t), ..ok() }).cause.contains("Turn probes on"));
        // probe findings outrank the retransmit note
        let d = diagnose(&Inputs { tcp: Some(t), ..with(ProbeIn { dns: Dns::Ms(500.0), ..good() }) });
        assert_eq!(d.label, "Degraded: DNS slow");
    }

    #[test]
    fn failing_connects_still_degrade_with_clean_probes() {
        let t = TcpHealth { fail_min: 100.0, ..Default::default() };
        // with probes the gateway answers, so heavy failures with no traffic are not "offline"
        let d = diagnose(&Inputs { zero_traffic: true, tcp: Some(t), ..with(good()) });
        assert_eq!((d.lv, d.label.as_str()), (Lv::Warn, "Degraded: connections failing"));
    }
}
