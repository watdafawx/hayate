//! Opt-in network probes for Vitals > Network (phase 2 of the design mock).
//!
//! OFF by default and persisted in `%LOCALAPPDATA%\Blink\vitals.ini`. When ON, and only while the
//! Network detail is on screen with the window shown, one dedicated thread sends every ~2 s:
//!   * one ICMP echo (IcmpSendEcho2Ex, 32-byte payload, 1 s timeout) to the selected adapter's
//!     gateway, from that adapter's IPv4 address,
//!   * one ICMP echo to a public host (default 1.1.1.1), same source address,
//!   * one DNS A lookup of a fixed name (default example.com) sent to that adapter's own DNS
//!     server (DnsQueryEx, cache bypassed, 1.5 s timeout, cancelled when it overruns).
//! Nothing else is ever sent: the targets come from `Cfg`, which holds exactly those three
//! destinations; there is no sweep, no raw socket, no packet crafting, no admin right.
//!
//! The sampler leases the thread each tick (`lease`); a lease lasts `LEASE`, so when the detail
//! is hidden, the window is hidden, the adapter changes, or the sampler stalls, the thread stops
//! sending at once and exits after `IDLE_EXIT` (nothing stays resident or open). The thread
//! never holds the shared lock across a network call, so the sampler and the UI can never block
//! on it. `view` is the pure read model the UI draws; history simply has gaps.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::nethealth::{lvl, Lv};
use super::HIST;

// ---------- constants ----------

/// One round (gateway echo, internet echo, DNS lookup) per this.
pub const PERIOD: Duration = Duration::from_secs(2);
/// Echo timeout.
const ICMP_TIMEOUT_MS: u32 = 1000;
/// DNS lookup timeout; the query is cancelled after it.
const DNS_TIMEOUT_MS: u32 = 1500;
/// After DnsCancelQuery, how long to wait for the completion callback before parking the job.
const DNS_CANCEL_GRACE_MS: u32 = 1000;
/// How long one sampler tick keeps probing allowed.
const LEASE: Duration = Duration::from_secs(3);
/// The thread exits after this long without a valid lease.
const IDLE_EXIT: Duration = Duration::from_secs(4);
/// Stats window.
const WINDOW_S: f32 = 60.0;
/// Rounds kept (60 s at 2 s, with slack for a round that overran).
const KEEP: usize = 40;
/// Pings the jitter is taken over.
const JITTER_N: usize = 30;
/// Newest round older than this = no current data (detail was hidden, or the adapter changed).
const FRESH_S: f32 = 6.0;
/// After this long of every probe answering healthy, the round slows to `IDLE_PERIOD`.
pub const IDLE_AFTER: Duration = Duration::from_secs(600);
/// The round while everything has been healthy for `IDLE_AFTER` (back to `PERIOD` on any loss,
/// high RTT, or when the user changes tab / adapter / the window is hidden).
pub const IDLE_PERIOD: Duration = Duration::from_secs(10);
/// Payload of every echo.
const PAYLOAD: &[u8; 32] = b"abcdefghijklmnopqrstuvwabcdefghi";
/// Link-layer bytes of one echo request + reply on Ethernet (14 + 20 + 8 + 32, twice).
const ICMP_BYTES: f32 = 148.0;
/// One A query + reply, about.
const DNS_BYTES: f32 = 175.0;

pub const DEFAULT_INTERNET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
pub const DEFAULT_DNS_NAME: &str = "example.com";

// thresholds [warn, hot] (the design mock, "WARN / HOT at")
pub const TH_GW_RTT: [f64; 2] = [15.0, 50.0];
pub const TH_GW_JIT: [f64; 2] = [5.0, 15.0];
pub const TH_IN_RTT: [f64; 2] = [80.0, 200.0];
pub const TH_IN_JIT: [f64; 2] = [15.0, 40.0];
pub const TH_LOSS: [f64; 2] = [1.0, 5.0];
pub const TH_DNS: [f64; 2] = [100.0, 400.0];
/// Consecutive unanswered rounds that mean "not reachable".
pub const DEAD_ROUNDS: u32 = 3;

// ---------- settings (vitals.ini) ----------

/// The switch and the editable targets. The targets are read once at start; edit the file while
/// Blink is closed. There is no free-text control in the UI, so they show as read-only chips.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    /// None: `internet = none` in vitals.ini, gateway (and DNS) only, no external destination
    pub internet: Option<Ipv4Addr>,
    pub dns_name: String,
    /// why a value of the file was ignored (shown next to the chips)
    pub warn: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { enabled: false, internet: Some(DEFAULT_INTERNET), dns_name: DEFAULT_DNS_NAME.into(), warn: None }
    }
}

/// A literal IPv4 address Blink may send an echo to: one host, no range, no wildcard, and none of
/// the addresses that are never a host (0.x, loopback, multicast, reserved, broadcast).
pub fn valid_ipv4(s: &str) -> Option<Ipv4Addr> {
    let ip: Ipv4Addr = s.trim().parse().ok()?;
    let o = ip.octets();
    (!(o[0] == 0 || o[0] == 127 || o[0] >= 224)).then_some(ip)
}

/// A DNS host name Blink may look up: letters, digits and hyphens in dot-separated labels, at
/// most 253 characters, not an IP literal, no wildcard, no scheme, no path, no port.
pub fn valid_hostname(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches('.');
    if s.is_empty() || s.len() > 253 || !s.contains('.') || s.parse::<Ipv4Addr>().is_ok() {
        return None;
    }
    let ok = s.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    // a name that is all digits and dots is an (invalid) address, not a host
    (ok && s.bytes().any(|b| b.is_ascii_alphabetic())).then(|| s.to_ascii_lowercase())
}

pub fn parse_ini(text: &str) -> Settings {
    let mut s = Settings::default();
    let mut sect = String::new();
    let mut warn = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(h) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sect = h.trim().to_ascii_lowercase();
            continue;
        }
        if sect != "net_probes" {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "enabled" => s.enabled = matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes" | "on"),
            // no external destination: only the gateway (and DNS) are probed
            "internet" if matches!(v.to_ascii_lowercase().as_str(), "none" | "off") => s.internet = None,
            "internet" => match valid_ipv4(v) {
                Some(ip) => s.internet = Some(ip),
                None => warn.push(format!("internet = {v} is not a single IPv4 address (or none)")),
            },
            "dns_name" => match valid_hostname(v) {
                Some(n) => s.dns_name = n,
                None => warn.push(format!("dns_name = {v} is not a host name")),
            },
            _ => {}
        }
    }
    if !warn.is_empty() {
        s.warn = Some(format!("vitals.ini: {}; using the defaults", warn.join(", ")));
    }
    s
}

pub fn render_ini(s: &Settings) -> String {
    format!(
        "; Hayate Vitals network probes. Opt-in: OFF unless enabled = true (the switch in Vitals > Network).\n\
         ; Edit the targets while Hayate is closed. With probes on, and only while Vitals > Network is open,\n\
         ; Hayate sends every 2 s: one ICMP echo to the adapter's gateway, one to `internet` (a single IPv4\n\
         ; address, or `none` for no external destination) and one DNS lookup of `dns_name` to the\n\
         ; adapter's own DNS server. See the Defender review notes.\n\
         [net_probes]\nenabled = {}\ninternet = {}\ndns_name = {}\n",
        s.enabled,
        s.internet.map_or("none".to_string(), |i| i.to_string()),
        s.dns_name
    )
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static SETTINGS: OnceLock<Settings> = OnceLock::new();

fn ini_path() -> std::path::PathBuf {
    crate::osutil::data_dir().join("vitals.ini")
}

/// Load vitals.ini once (sampler thread, at start). Until then probes read as off.
pub fn init() {
    let s = SETTINGS.get_or_init(|| parse_ini(&std::fs::read_to_string(ini_path()).unwrap_or_default()));
    ENABLED.store(s.enabled, Ordering::Relaxed);
}

/// The targets (defaults until `init` ran).
pub fn settings() -> Settings {
    SETTINGS.get().cloned().unwrap_or_default()
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// The switch: flips it, saves it (background write) and returns the new state. Turning it off
/// stops the sending at once (the lease is dropped here, not at the next tick).
pub fn toggle() -> bool {
    let on = !ENABLED.fetch_xor(true, Ordering::Relaxed);
    let mut s = settings();
    s.enabled = on;
    crate::bgio::write_file(ini_path(), render_ini(&s).into_bytes());
    if !on {
        release();
    }
    on
}

// ---------- what is probed ----------

/// What the internet echo does on this adapter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Inet {
    Target(Ipv4Addr),
    /// `internet = none`
    Off,
    /// a virtual / tunnel adapter, or the OS routes the target through another adapter: an echo
    /// sourced from here would test the wrong path, so none is sent
    NotRouted,
}

/// How the internet probe reads in the UI and the verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum InetState {
    #[default]
    Probed,
    Off,
    NotRouted,
}

impl Inet {
    pub fn target(self) -> Option<Ipv4Addr> {
        match self {
            Inet::Target(ip) => Some(ip),
            _ => None,
        }
    }
    pub fn state(self) -> InetState {
        match self {
            Inet::Target(_) => InetState::Probed,
            Inet::Off => InetState::Off,
            Inet::NotRouted => InetState::NotRouted,
        }
    }
}

/// Exactly the destinations of one adapter; nothing outside it is ever sent to.
#[derive(Clone, Debug, PartialEq)]
pub struct Cfg {
    pub luid: u64,
    pub src: Ipv4Addr,
    pub gw: Option<Ipv4Addr>,
    pub inet: Inet,
    /// (the adapter's DNS server, the name to look up)
    pub dns: Option<(Ipv4Addr, String)>,
    /// the adapter's DNS server is a loopback stub resolver: not probed, and said so
    pub dns_local: bool,
}

/// The interface the OS would send a packet to `dst` through (GetBestInterfaceEx), as a LUID.
/// None when there is no route or the lookup failed.
pub fn route_luid(dst: Ipv4Addr) -> Option<u64> {
    use windows::Win32::NetworkManagement::IpHelper::{ConvertInterfaceIndexToLuid, GetBestInterfaceEx};
    use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR, SOCKADDR_IN};
    let mut sa = SOCKADDR_IN::default();
    sa.sin_family = AF_INET;
    sa.sin_addr.S_un.S_addr = u32::from_ne_bytes(dst.octets());
    let mut idx = 0u32;
    unsafe {
        if GetBestInterfaceEx(&sa as *const SOCKADDR_IN as *const SOCKADDR, &mut idx) != 0 {
            return None;
        }
        let mut l = NET_LUID_LH::default();
        (ConvertInterfaceIndexToLuid(idx, &mut l).0 == 0).then_some(l.Value)
    }
}

/// What the internet echo does: off, a target, or "not routed here". `routed_via` is the adapter
/// the OS sends the target through (`route_luid`); when it is known, it alone decides (a VPN
/// adapter that is the route is probed, the physical one behind it is not). Without it, a
/// virtual adapter is not probed.
pub fn inet_for(luid: u64, kind: &str, target: Option<Ipv4Addr>, routed_via: Option<u64>) -> Inet {
    match (target, routed_via) {
        (None, _) => Inet::Off,
        (Some(ip), Some(l)) if l == luid => Inet::Target(ip),
        (Some(_), Some(_)) => Inet::NotRouted,
        (Some(_), None) if kind == "virtual" => Inet::NotRouted,
        (Some(ip), None) => Inet::Target(ip),
    }
}

/// The probe targets of one adapter from its addresses (`Addr` text forms) and the settings.
/// None without an IPv4 address of its own: there is nothing to send from.
pub fn cfg_for(luid: u64, ipv4: &str, gateway: &str, dns: &str, st: &Settings, kind: &str, routed_via: Option<u64>) -> Option<Cfg> {
    let src: Ipv4Addr = ipv4.split_whitespace().next()?.parse().ok()?;
    // a gateway or DNS server that is not a plain unicast address is not probed
    let host = |s: &str| s.trim().parse::<Ipv4Addr>().ok().filter(|a| !a.is_unspecified() && !a.is_loopback() && !a.is_multicast() && !a.is_broadcast());
    let dns_local = dns.trim().parse::<Ipv4Addr>().is_ok_and(|a| a.is_loopback());
    Some(Cfg { luid, src, gw: host(gateway), inet: inet_for(luid, kind, st.internet, routed_via), dns: host(dns).map(|d| (d, st.dns_name.clone())), dns_local })
}

// ---------- rounds and shared state ----------

/// One probe of a round.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Probed {
    /// not part of this adapter's targets (no gateway, no DNS server)
    Skipped,
    Lost,
    /// answered in this many ms
    Ms(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Round {
    pub at: Instant,
    pub gw: Probed,
    pub inet: Probed,
    pub dns: Probed,
}

struct Shared {
    cfg: Option<Cfg>,
    lease: Option<Instant>,
    rounds: VecDeque<Round>,
    running: bool,
    /// the gap between rounds right now (`PERIOD`, or `IDLE_PERIOD` when calm)
    period: Duration,
}

static S: Mutex<Shared> = Mutex::new(Shared { cfg: None, lease: None, rounds: VecDeque::new(), running: false, period: PERIOD });

/// Idle backoff: tracks how long every probe has been healthy.
#[derive(Default)]
struct Calm {
    since: Option<Instant>,
}

impl Calm {
    fn reset(&mut self) {
        self.since = None;
    }
    /// A finished round at `now`; returns the gap before the next one.
    fn next_period(&mut self, now: Instant, clean: bool) -> Duration {
        if !clean {
            self.since = None;
            return PERIOD;
        }
        let since = *self.since.get_or_insert(now);
        if now.saturating_duration_since(since) >= IDLE_AFTER { IDLE_PERIOD } else { PERIOD }
    }
}

/// Every probe of the round answered under its warn threshold. A target that has never answered
/// (a gateway or host that ignores ping: at least `DEAD_ROUNDS` lost rounds, no answer) is steady,
/// not a fault, so it does not keep the round at the fast pace.
fn is_clean(hist: &VecDeque<Round>, r: &Round) -> bool {
    let ok = |f: fn(&Round) -> Probed, warn: f64| match f(r) {
        Probed::Skipped => true,
        Probed::Ms(v) => (v as f64) < warn,
        Probed::Lost => !hist.iter().any(|h| matches!(f(h), Probed::Ms(_))) && hist.iter().filter(|h| f(h) == Probed::Lost).count() >= DEAD_ROUNDS as usize,
    };
    ok(|r| r.gw, TH_GW_RTT[0]) && ok(|r| r.inet, TH_IN_RTT[0]) && ok(|r| r.dns, TH_DNS[0])
}
static CV: Condvar = Condvar::new();

fn st() -> MutexGuard<'static, Shared> {
    S.lock().unwrap_or_else(|e| e.into_inner())
}

/// The config probing is allowed for right now: set, and its lease has not run out.
fn allowed(g: &Shared, now: Instant) -> Option<Cfg> {
    match (&g.cfg, g.lease) {
        (Some(c), Some(l)) if now < l => Some(c.clone()),
        _ => None,
    }
}

/// Sampler tick, Network detail shown with probes on: keep probing `cfg` for `LEASE` more.
/// A different adapter / target starts a fresh history.
pub fn lease(cfg: Cfg) {
    let now = Instant::now();
    let spawn = {
        let mut g = st();
        if g.cfg.as_ref() != Some(&cfg) {
            g.rounds.clear();
            g.period = PERIOD;
            g.cfg = Some(cfg);
        }
        g.lease = Some(now + LEASE);
        let spawn = !g.running;
        g.running = true;
        spawn
    };
    if spawn && std::thread::Builder::new().name("blink-probe".into()).stack_size(256 * 1024).spawn(run).is_err() {
        st().running = false;
    }
    CV.notify_all();
}

/// Probing is not wanted now (detail or window hidden, probes off): stop sending at once.
pub fn release() {
    let mut g = st();
    g.cfg = None;
    g.lease = None;
    drop(g);
    CV.notify_all();
}

fn run() {
    let mut net = Net::default();
    let mut next = Instant::now();
    let mut calm = Calm::default();
    let mut last_cfg: Option<Cfg> = None;
    loop {
        // wait for a round that is due and allowed; leave when nothing was allowed for IDLE_EXIT
        let cfg = {
            let mut g = st();
            let mut idle_since = Instant::now();
            loop {
                let now = Instant::now();
                match allowed(&g, now) {
                    Some(c) if now >= next => break c,
                    Some(_) => {
                        idle_since = now;
                        let wait = next.saturating_duration_since(now);
                        g = CV.wait_timeout(g, wait).unwrap_or_else(|e| e.into_inner()).0;
                    }
                    None => {
                        // probing paused (tab, adapter or window changed): back to the fast pace
                        calm.reset();
                        g.period = PERIOD;
                        next = now;
                        let left = IDLE_EXIT.saturating_sub(now.saturating_duration_since(idle_since));
                        if left.is_zero() {
                            g.running = false;
                            drop(g);
                            net.close();
                            return;
                        }
                        g = CV.wait_timeout(g, left).unwrap_or_else(|e| e.into_inner()).0;
                    }
                }
            }
        };
        if last_cfg.as_ref() != Some(&cfg) {
            calm.reset();
            last_cfg = Some(cfg.clone());
        }
        let t0 = Instant::now();
        // each send re-checks the lease: hiding the detail mid-round sends nothing more, and a
        // round that was cut short is dropped
        let still = || allowed(&st(), Instant::now()).as_ref() == Some(&cfg);
        let mut cut = false;
        let gw = match cfg.gw {
            Some(ip) if still() => net.echo(cfg.src, ip),
            Some(_) => {
                cut = true;
                Probed::Skipped
            }
            None => Probed::Skipped,
        };
        let inet = match cfg.inet {
            Inet::Target(ip) if !cut && still() => net.echo(cfg.src, ip),
            Inet::Target(_) => {
                cut = true;
                Probed::Skipped
            }
            // internet = none, or not routed through this adapter: nothing is sent
            _ => Probed::Skipped,
        };
        let dns = match &cfg.dns {
            Some((srv, name)) if !cut && still() => dns_lookup(&mut net, *srv, name),
            Some(_) => {
                cut = true;
                Probed::Skipped
            }
            None => Probed::Skipped,
        };
        let done = Instant::now();
        let period = {
            let mut g = st();
            // only a complete round of the config still in force is kept
            if !cut && allowed(&g, done).as_ref() == Some(&cfg) {
                let round = Round { at: t0, gw, inet, dns };
                let clean = is_clean(&g.rounds, &round);
                g.rounds.push_back(round);
                while g.rounds.len() > KEEP {
                    g.rounds.pop_front();
                }
                g.period = calm.next_period(done, clean);
            } else {
                calm.reset();
                g.period = PERIOD;
            }
            g.period
        };
        next = (t0 + period).max(done);
    }
}

// ---------- ICMP (IcmpSendEcho2Ex) and DNS (DnsQueryEx) ----------

/// How many cancelled lookups that never called back may be parked; past it DNS probing stops.
const MAX_PARKED: usize = 8;
/// How long the thread waits at exit for parked lookups to signal before it leaks what is left.
const PARKED_EXIT_WAIT: Duration = Duration::from_millis(1000);

/// Jobs the OS may still write to: parked until their completion callback has signalled, and
/// only then freed. Generic so the bookkeeping is testable with a fake job.
struct Parked<J> {
    jobs: Vec<J>,
}

impl<J> Default for Parked<J> {
    fn default() -> Self {
        Parked { jobs: Vec::new() }
    }
}

impl<J> Parked<J> {
    fn len(&self) -> usize {
        self.jobs.len()
    }
    fn is_full(&self) -> bool {
        self.jobs.len() >= MAX_PARKED
    }
    fn park(&mut self, j: J) {
        self.jobs.push(j);
    }
    /// Free (via `free`) exactly the jobs `done` says have signalled; the rest stay parked.
    fn reap(&mut self, mut done: impl FnMut(&J) -> bool, mut free: impl FnMut(J)) {
        let mut keep = Vec::new();
        for j in self.jobs.drain(..) {
            if done(&j) {
                free(j);
            } else {
                keep.push(j);
            }
        }
        self.jobs = keep;
    }
}

/// A cancelled lookup whose callback has not signalled yet: the boxed job and its event.
struct Abandoned {
    job: *mut DnsJob,
    ev: windows::Win32::Foundation::HANDLE,
}

impl Abandoned {
    fn signalled(&self) -> bool {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        unsafe { windows::Win32::System::Threading::WaitForSingleObject(self.ev, 0) == WAIT_OBJECT_0 }
    }
    /// Only after `signalled`: the callback ran, nothing writes to the job any more.
    fn free(self) {
        unsafe { free_dns_job(self.job, self.ev) }
    }
}

#[derive(Default)]
struct Net {
    icmp: Option<windows::Win32::Foundation::HANDLE>,
    parked: Parked<Abandoned>,
    /// the resolver left too many lookups hanging: no more lookups in this session
    dns_off: bool,
}

impl Net {
    /// One echo request, 32-byte payload, from `src` to `dst`, answered or not within 1 s.
    fn echo(&mut self, src: Ipv4Addr, dst: Ipv4Addr) -> Probed {
        use windows::Win32::NetworkManagement::IpHelper::*;
        if !sendable(dst) {
            return Probed::Lost;
        }
        if self.icmp.is_none() {
            self.icmp = unsafe { IcmpCreateFile() }.ok();
        }
        let Some(h) = self.icmp else { return Probed::Lost };
        let ip = |a: Ipv4Addr| u32::from_ne_bytes(a.octets());
        // room for the reply record, the echoed payload and an ICMP error (docs: payload + 8 + records)
        let mut buf = [0u64; 32];
        let t0 = Instant::now();
        let n = unsafe { IcmpSendEcho2Ex(h, None, None, None, ip(src), ip(dst), PAYLOAD.as_ptr() as *const _, PAYLOAD.len() as u16, None, buf.as_mut_ptr() as *mut _, std::mem::size_of_val(&buf) as u32, ICMP_TIMEOUT_MS) };
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        if n == 0 {
            return Probed::Lost;
        }
        let r = unsafe { &*(buf.as_ptr() as *const ICMP_ECHO_REPLY) };
        // IP_SUCCESS from the host asked (a router's "unreachable" is a reply, not an answer)
        if r.Status == 0 && r.Address == ip(dst) {
            Probed::Ms(ms.max(0.01))
        } else {
            Probed::Lost
        }
    }

    /// Free the parked lookups that have signalled by now.
    fn reap(&mut self) {
        self.parked.reap(Abandoned::signalled, Abandoned::free);
    }

    fn close(&mut self) {
        // give parked lookups a moment to call back; what never does is leaked, not freed live
        let end = Instant::now() + PARKED_EXIT_WAIT;
        loop {
            self.reap();
            if self.parked.len() == 0 || Instant::now() >= end {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Some(h) = self.icmp.take() {
            let _ = unsafe { windows::Win32::NetworkManagement::IpHelper::IcmpCloseHandle(h) };
        }
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.close();
    }
}

/// The "Test this host" click: exactly one echo (`IcmpSendEcho`, 32-byte payload, 1 s) to `dst`, on
/// its own handle that is closed again. Only to a `sendable` unicast host (never loopback). True
/// when that host itself answered. Blocks up to 1 s: call from a worker thread only.
pub fn echo_once(dst: Ipv4Addr) -> bool {
    use windows::Win32::NetworkManagement::IpHelper::*;
    if !sendable(dst) {
        return false;
    }
    let Ok(h) = (unsafe { IcmpCreateFile() }) else { return false };
    let ip = u32::from_ne_bytes(dst.octets());
    let mut buf = [0u64; 32];
    let n = unsafe { IcmpSendEcho(h, ip, PAYLOAD.as_ptr() as *const _, PAYLOAD.len() as u16, None, buf.as_mut_ptr() as *mut _, std::mem::size_of_val(&buf) as u32, ICMP_TIMEOUT_MS) };
    let ok = n != 0 && {
        let r = unsafe { &*(buf.as_ptr() as *const ICMP_ECHO_REPLY) };
        r.Status == 0 && r.Address == ip
    };
    let _ = unsafe { IcmpCloseHandle(h) };
    ok
}

/// Defence in depth: the config only holds unicast hosts, and so does what is sent.
fn sendable(a: Ipv4Addr) -> bool {
    !(a.is_unspecified() || a.is_loopback() || a.is_multicast() || a.is_broadcast() || a.octets()[0] >= 240)
}

/// The device a DnsQueryEx completion signals (the event handle rides in the query context).
unsafe extern "system" fn dns_done(ctx: *const core::ffi::c_void, _res: *mut windows::Win32::NetworkManagement::Dns::DNS_QUERY_RESULT) {
    let _ = windows::Win32::System::Threading::SetEvent(windows::Win32::Foundation::HANDLE(ctx as *mut _));
}

/// Everything DnsQueryEx points at: it must stay alive until the completion callback ran.
struct DnsJob {
    req: windows::Win32::NetworkManagement::Dns::DNS_QUERY_REQUEST,
    res: windows::Win32::NetworkManagement::Dns::DNS_QUERY_RESULT,
    cancel: windows::Win32::NetworkManagement::Dns::DNS_QUERY_CANCEL,
    servers: windows::Win32::NetworkManagement::Dns::DNS_ADDR_ARRAY,
    name: Vec<u16>,
}

/// Free a finished (or signalled-after-cancel) job: its records, the box, the event.
unsafe fn free_dns_job(job: *mut DnsJob, ev: windows::Win32::Foundation::HANDLE) {
    use windows::Win32::NetworkManagement::Dns::*;
    if !(*job).res.pQueryRecords.is_null() {
        DnsFree(Some((*job).res.pQueryRecords as *const _), DnsFreeRecordList);
    }
    drop(Box::from_raw(job));
    let _ = windows::Win32::Foundation::CloseHandle(ev);
}

/// One A lookup of `name` sent to `server` only, cache bypassed, timed. A lookup that runs past
/// `DNS_TIMEOUT_MS` is cancelled and counts as lost (worst case 1.5 s + 1 s cancel grace). A
/// cancelled lookup that never calls back is parked (never freed while live); at `MAX_PARKED`
/// of them the resolver is written off and no further lookup is sent in this session.
fn dns_lookup(net: &mut Net, server: Ipv4Addr, name: &str) -> Probed {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::NetworkManagement::Dns::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    if !sendable(server) {
        return Probed::Lost;
    }
    net.reap();
    if net.dns_off || net.parked.is_full() {
        net.dns_off = true;
        return Probed::Lost;
    }
    unsafe {
        let Ok(ev) = CreateEventW(None, true, false, None) else { return Probed::Lost };
        let job = Box::into_raw(Box::new(DnsJob {
            req: DNS_QUERY_REQUEST::default(),
            res: DNS_QUERY_RESULT::default(),
            cancel: DNS_QUERY_CANCEL::default(),
            servers: DNS_ADDR_ARRAY::default(),
            name: name.encode_utf16().chain(std::iter::once(0)).collect(),
        }));
        // one server: AF_INET sockaddr (family, port 0, address) in the first DNS_ADDR
        let mut a = DNS_ADDR::default();
        a.MaxSa[0] = 2;
        for (i, b) in server.octets().iter().enumerate() {
            a.MaxSa[4 + i] = *b as i8;
        }
        (*job).servers.MaxCount = 1;
        (*job).servers.AddrCount = 1;
        (*job).servers.Family = 2;
        (*job).servers.AddrArray = [a];
        (*job).req.Version = DNS_QUERY_REQUEST_VERSION1.0;
        (*job).req.QueryName = windows::core::PCWSTR((*job).name.as_ptr());
        (*job).req.QueryType = DNS_TYPE_A.0;
        // bypass the cache; no hosts file, no multicast / NetBT fallbacks (a failing name stays a failure)
        (*job).req.QueryOptions = (DNS_QUERY_BYPASS_CACHE.0 | DNS_QUERY_NO_HOSTS_FILE.0 | DNS_QUERY_NO_MULTICAST.0 | DNS_QUERY_NO_NETBT.0) as u64;
        (*job).req.pDnsServerList = &mut (*job).servers;
        (*job).req.pQueryCompletionCallback = Some(dns_done);
        (*job).req.pQueryContext = ev.0;
        (*job).res.Version = DNS_QUERY_RESULTS_VERSION1.0 as u32;
        let t0 = Instant::now();
        let rc = DnsQueryEx(&(*job).req, &mut (*job).res, Some(&mut (*job).cancel));
        let mut done = rc != windows::Win32::Foundation::DNS_REQUEST_PENDING;
        if !done {
            done = WaitForSingleObject(ev, DNS_TIMEOUT_MS) == WAIT_OBJECT_0;
            if !done {
                DnsCancelQuery(&(*job).cancel);
                // the cancelled query still calls back; if it does not within the grace, park the
                // job: the callback may still write to it, so it is freed only once signalled
                if WaitForSingleObject(ev, DNS_CANCEL_GRACE_MS) != WAIT_OBJECT_0 {
                    net.parked.park(Abandoned { job, ev });
                    if net.parked.is_full() {
                        net.dns_off = true;
                    }
                    return Probed::Lost;
                }
            }
        }
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        let ok = done && (rc == 0 || rc == windows::Win32::Foundation::DNS_REQUEST_PENDING) && (*job).res.QueryStatus == 0 && !(*job).res.pQueryRecords.is_null();
        free_dns_job(job, ev);
        if ok { Probed::Ms(ms.max(0.01)) } else { Probed::Lost }
    }
}

// ---------- statistics (pure) ----------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PathStats {
    /// rounds in the last 60 s that probed this target
    pub sent: u32,
    pub lost: u32,
    /// newest answer, None when the newest round was lost
    pub now: Option<f32>,
    pub avg: Option<f32>,
    pub min: Option<f32>,
    pub max: Option<f32>,
    /// mean |Δ| of consecutive answered pings over the last 30
    pub jitter: Option<f32>,
    pub loss_pct: f32,
    /// unanswered rounds in a row at the newest end
    pub dead: u32,
    /// seconds since the last answer (or since the first round when there was none)
    pub silent_s: f32,
}

impl PathStats {
    pub fn answered(&self) -> u32 {
        self.sent - self.lost
    }
    /// Not answering at all for `DEAD_ROUNDS` rounds.
    pub fn is_dead(&self) -> bool {
        self.dead >= DEAD_ROUNDS
    }
    /// Every probe of the window went unanswered (with at least `DEAD_ROUNDS` of them).
    pub fn never_answered(&self) -> bool {
        self.sent >= DEAD_ROUNDS && self.lost == self.sent
    }
}

/// Stats of one target from its samples, oldest first: (seconds ago, RTT ms or None when lost).
/// Samples older than 60 s are ignored.
pub fn path_stats(samples: &[(f32, Option<f32>)]) -> PathStats {
    let s: Vec<&(f32, Option<f32>)> = samples.iter().filter(|(age, _)| *age <= WINDOW_S).collect();
    let mut p = PathStats { sent: s.len() as u32, ..Default::default() };
    if s.is_empty() {
        return p;
    }
    let ok: Vec<f32> = s.iter().filter_map(|x| x.1).collect();
    p.lost = p.sent - ok.len() as u32;
    p.loss_pct = p.lost as f32 / p.sent as f32 * 100.0;
    p.now = s.last().and_then(|x| x.1);
    p.dead = s.iter().rev().take_while(|x| x.1.is_none()).count() as u32;
    if !ok.is_empty() {
        p.avg = Some(ok.iter().sum::<f32>() / ok.len() as f32);
        p.min = ok.iter().copied().reduce(f32::min);
        p.max = ok.iter().copied().reduce(f32::max);
    }
    // jitter: consecutive answered pairs among the last 30 pings
    let tail = &s[s.len().saturating_sub(JITTER_N)..];
    let (mut sum, mut n) = (0.0f32, 0u32);
    for w in tail.windows(2) {
        if let (Some(a), Some(b)) = (w[0].1, w[1].1) {
            sum += (b - a).abs();
            n += 1;
        }
    }
    p.jitter = (n > 0).then(|| sum / n as f32);
    p.silent_s = match s.iter().rev().find(|x| x.1.is_some()) {
        Some(x) => x.0,
        None => s[0].0,
    };
    p
}

/// What one graph column holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Col {
    None,
    Ms(f32),
    Lost,
}

/// The 60 graph columns (one per second, newest last) of one target. A round lands in the column
/// of its age; rounds are 2 s apart, so every other column is empty and is drawn by `series`.
pub fn columns(samples: &[(f32, Option<f32>)]) -> Vec<Col> {
    let mut c = vec![Col::None; HIST];
    for (age, v) in samples {
        if *age < 0.0 || *age > (HIST - 1) as f32 + 0.5 {
            continue;
        }
        let i = HIST - 1 - (age.round() as usize).min(HIST - 1);
        c[i] = v.map_or(Col::Lost, Col::Ms);
    }
    c
}

/// Line values of `columns`: lost and never-sampled columns are gaps (NaN), except the single
/// empty column between two answered neighbours, which is the straight line between them.
pub fn series(cols: &[Col]) -> Vec<f32> {
    let mut v: Vec<f32> = cols.iter().map(|c| if let Col::Ms(x) = c { *x } else { f32::NAN }).collect();
    for i in 1..cols.len().saturating_sub(1) {
        if cols[i] == Col::None {
            if let (Col::Ms(a), Col::Ms(b)) = (cols[i - 1], cols[i + 1]) {
                v[i] = (a + b) / 2.0;
            }
        }
    }
    v
}

/// Columns where a probe was lost (the red ticks).
pub fn lost_cols(cols: &[Col]) -> Vec<usize> {
    cols.iter().enumerate().filter(|(_, c)| **c == Col::Lost).map(|(i, _)| i).collect()
}

/// The reading at a hovered column: the round in it, else the nearest one a column away.
pub fn col_at(cols: &[Col], c: usize) -> Col {
    [c, c + 1, c.wrapping_sub(1)].into_iter().filter_map(|i| cols.get(i).copied()).find(|x| *x != Col::None).unwrap_or(Col::None)
}

// ---------- levels (pure) ----------

fn lv_opt(v: Option<f32>, t: [f64; 2]) -> Lv {
    v.map_or(Lv::Ok, |v| lvl(v as f64, t))
}

/// Gateway layer: worst of average RTT, jitter and loss; Hot when it stopped answering.
pub fn gw_level(s: &PathStats) -> Lv {
    if s.is_dead() {
        return Lv::Hot;
    }
    [lv_opt(s.avg, TH_GW_RTT), lv_opt(s.jitter, TH_GW_JIT), if s.sent > 0 { lvl(s.loss_pct as f64, TH_LOSS) } else { Lv::Ok }].into_iter().max().unwrap_or_default()
}

pub fn inet_level(s: &PathStats) -> Lv {
    if s.is_dead() {
        return Lv::Hot;
    }
    [lv_opt(s.avg, TH_IN_RTT), lv_opt(s.jitter, TH_IN_JIT), if s.sent > 0 { lvl(s.loss_pct as f64, TH_LOSS) } else { Lv::Ok }].into_iter().max().unwrap_or_default()
}

/// DNS layer: a failed lookup is Hot.
pub fn dns_level(d: Dns) -> Lv {
    match d {
        Dns::Ms(v) => lvl(v as f64, TH_DNS),
        Dns::Failed => Lv::Hot,
        Dns::NotProbed | Dns::Local | Dns::Waiting => Lv::Ok,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Dns {
    /// the adapter has no DNS server
    #[default]
    NotProbed,
    /// the adapter's DNS server is a loopback stub resolver: not probed
    Local,
    Waiting,
    Ms(f32),
    Failed,
}

// ---------- the read model ----------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    pub stats: PathStats,
    pub cols: Vec<Col>,
}

/// What the UI draws, for the adapter being probed. `Default` = nothing probed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    /// something is being probed and the newest round is recent
    pub live: bool,
    /// rounds of the last 60 s
    pub rounds: u32,
    /// no gateway on the adapter: only the internet echo (and DNS) are probed
    pub gw: Option<Path>,
    pub inet: Path,
    /// the internet echo is off (`internet = none`) or not sent from this adapter
    pub inet_state: InetState,
    pub dns: Dns,
    /// estimated probe traffic, KB per minute
    pub kb_min: f32,
}

/// What a config probes, for the view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    pub gw: bool,
    pub dns: bool,
    pub dns_local: bool,
    pub inet: InetState,
}

impl Shape {
    pub fn of(c: &Cfg) -> Shape {
        Shape { gw: c.gw.is_some(), dns: c.dns.is_some(), dns_local: c.dns_local, inet: c.inet.state() }
    }
}

/// Build the view from the stored rounds at `now`. `fresh_s`: how old the newest round may be
/// and still count as current (it grows with the round period).
pub fn view_from(rounds: &[Round], now: Instant, sh: Shape, fresh_s: f32) -> View {
    let age = |r: &Round| now.saturating_duration_since(r.at).as_secs_f32();
    let recent: Vec<&Round> = rounds.iter().filter(|r| age(r) <= WINDOW_S).collect();
    let pick = |f: fn(&Round) -> Probed| -> Vec<(f32, Option<f32>)> {
        recent
            .iter()
            .filter_map(|r| match f(r) {
                Probed::Skipped => None,
                Probed::Lost => Some((age(r), None)),
                Probed::Ms(v) => Some((age(r), Some(v))),
            })
            .collect()
    };
    let path = |s: Vec<(f32, Option<f32>)>| Path { stats: path_stats(&s), cols: columns(&s) };
    let live = recent.last().is_some_and(|r| age(r) <= fresh_s);
    let dns = match recent.last().map(|r| r.dns) {
        _ if sh.dns_local && !sh.dns => Dns::Local,
        _ if !sh.dns => Dns::NotProbed,
        // a lookup that failed once is shown as the newest result; the older ones are history
        Some(Probed::Ms(v)) if live => Dns::Ms(v),
        Some(Probed::Lost) if live => Dns::Failed,
        _ => Dns::Waiting,
    };
    let echoes = sh.gw as u32 + (sh.inet == InetState::Probed) as u32;
    let per_round = ICMP_BYTES * echoes as f32 + if sh.dns { DNS_BYTES } else { 0.0 };
    View { live, rounds: recent.len() as u32, gw: sh.gw.then(|| path(pick(|r| r.gw))), inet: path(pick(|r| r.inet)), inet_state: sh.inet, dns, kb_min: if live { recent.len() as f32 * per_round / 1000.0 } else { 0.0 } }
}

/// The view of the probes now running (empty when none, or for another adapter).
pub fn view(luid: u64) -> View {
    let g = st();
    match &g.cfg {
        Some(c) if c.luid == luid => {
            let rounds: Vec<Round> = g.rounds.iter().copied().collect();
            let sh = Shape::of(c);
            // two missed periods and a little slack: the pace slows to 10 s when calm
            let fresh = (g.period.as_secs_f32() * 2.0 + 2.0).max(FRESH_S);
            drop(g);
            view_from(&rounds, Instant::now(), sh, fresh)
        }
        _ => View::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[Option<f32>]) -> Vec<(f32, Option<f32>)> {
        // 2 s apart, the last one is 1 s old
        let n = v.len();
        v.iter().enumerate().map(|(i, x)| (1.0 + 2.0 * (n - 1 - i) as f32, *x)).collect()
    }

    #[test]
    fn targets_must_be_one_host() {
        assert_eq!(valid_ipv4("1.1.1.1"), Some(Ipv4Addr::new(1, 1, 1, 1)));
        assert_eq!(valid_ipv4(" 8.8.8.8 "), Some(Ipv4Addr::new(8, 8, 8, 8)));
        for bad in ["1.1.1.0/24", "1.1.1.1-5", "1.1.1.*", "1.1.1", "0.0.0.0", "127.0.0.1", "224.0.0.1", "255.255.255.255", "example.com", "", "1.1.1.1:53"] {
            assert_eq!(valid_ipv4(bad), None, "{bad}");
        }
        assert_eq!(valid_hostname("Example.COM"), Some("example.com".into()));
        assert_eq!(valid_hostname("example.com."), Some("example.com".into()));
        for bad in ["1.1.1.1", "localhost", "*.example.com", "exa mple.com", "http://example.com", "example.com/x", "example.com:53", "-a.com", "a..com", "", "999.999.999.999", "a_b.com"] {
            assert_eq!(valid_hostname(bad), None, "{bad}");
        }
        assert_eq!(valid_hostname(&format!("{}.com", "a".repeat(64))), None);
    }

    #[test]
    fn ini_round_trips_and_ignores_bad_targets() {
        assert_eq!(parse_ini(""), Settings::default());
        let on = Settings { enabled: true, internet: Some(Ipv4Addr::new(9, 9, 9, 9)), dns_name: "one.one".into(), warn: None };
        assert_eq!(parse_ini(&render_ini(&on)), on);
        let d = parse_ini(&render_ini(&Settings::default()));
        assert!(!d.enabled, "off unless the file says on");
        let bad = parse_ini("[net_probes]\nenabled = true\ninternet = 10.0.0.0/8\ndns_name = *\n[other]\nenabled=false");
        assert!(bad.enabled);
        assert_eq!((bad.internet, bad.dns_name.as_str()), (Some(DEFAULT_INTERNET), DEFAULT_DNS_NAME));
        assert!(bad.warn.unwrap().contains("10.0.0.0/8"));
        // a key outside [net_probes] never counts
        assert!(!parse_ini("enabled = true").enabled);
    }

    #[test]
    fn config_holds_only_the_three_destinations() {
        let st = Settings::default();
        let c = cfg_for(7, "10.0.0.10 / 24", "10.0.0.1", "10.0.0.1", &st, "wired", Some(7)).unwrap();
        assert_eq!((c.src, c.gw, c.inet), (Ipv4Addr::new(10, 0, 0, 10), Some(Ipv4Addr::new(10, 0, 0, 1)), Inet::Target(DEFAULT_INTERNET)));
        assert_eq!(c.dns, Some((Ipv4Addr::new(10, 0, 0, 1), "example.com".into())));
        // no gateway / DNS server: those are skipped, never guessed
        let c = cfg_for(7, "10.0.0.2 / 8", "", "", &st, "wired", None).unwrap();
        assert_eq!((c.gw, c.dns), (None, None));
        // an adapter without an IPv4 address of its own sends nothing
        assert_eq!(cfg_for(7, "", "10.0.0.1", "10.0.0.1", &st, "wired", None), None);
        assert_eq!(cfg_for(7, "not an ip / 24", "", "", &st, "wired", None), None);
        // a gateway that is not a unicast host is dropped
        assert_eq!(cfg_for(7, "10.0.0.2 / 8", "255.255.255.255", "224.0.0.1", &st, "wired", None).map(|c| (c.gw, c.dns)), Some((None, None)));
        assert!(!sendable(Ipv4Addr::new(255, 255, 255, 255)) && !sendable(Ipv4Addr::new(0, 0, 0, 0)) && !sendable(Ipv4Addr::new(239, 1, 1, 1)) && sendable(Ipv4Addr::new(1, 1, 1, 1)));
    }

    #[test]
    fn lease_gates_probing() {
        let now = Instant::now();
        let cfg = Cfg { luid: 1, src: Ipv4Addr::new(10, 0, 0, 2), gw: None, inet: Inet::Target(DEFAULT_INTERNET), dns: None, dns_local: false };
        let mut g = Shared { cfg: None, lease: None, rounds: VecDeque::new(), running: false, period: PERIOD };
        assert_eq!(allowed(&g, now), None, "no config, no probes");
        g.cfg = Some(cfg.clone());
        assert_eq!(allowed(&g, now), None, "no lease, no probes");
        g.lease = Some(now + LEASE);
        assert_eq!(allowed(&g, now), Some(cfg));
        assert_eq!(allowed(&g, now + LEASE), None, "an expired lease (sampler stalled, detail hidden) stops the sends");
    }

    #[test]
    fn parked_jobs_are_freed_only_after_they_signal() {
        use std::cell::RefCell;
        // (id, signalled)
        let mut p: Parked<(u32, bool)> = Parked::default();
        for i in 0..3 {
            p.park((i, false));
        }
        let freed = RefCell::new(Vec::new());
        p.reap(|j| j.1, |j| freed.borrow_mut().push(j.0));
        assert!(freed.borrow().is_empty() && p.len() == 3, "nothing signalled: nothing freed");
        p.jobs[1].1 = true;
        p.reap(|j| j.1, |j| freed.borrow_mut().push(j.0));
        assert_eq!((freed.borrow().clone(), p.len()), (vec![1], 2), "only the signalled job goes");
        p.jobs.iter_mut().for_each(|j| j.1 = true);
        p.reap(|j| j.1, |j| freed.borrow_mut().push(j.0));
        assert_eq!((freed.borrow().len(), p.len()), (3, 0));
    }

    #[test]
    fn parked_list_is_capped() {
        let mut p: Parked<u8> = Parked::default();
        for i in 0..MAX_PARKED - 1 {
            p.park(i as u8);
            assert!(!p.is_full());
        }
        p.park(0);
        assert!(p.is_full(), "at the cap the DNS probe stops for the session");
        p.reap(|_| true, |_| {});
        assert!(!p.is_full() && p.len() == 0);
    }

    #[test]
    fn stats_of_a_clean_path() {
        let p = path_stats(&s(&[Some(1.0), Some(2.0), Some(1.5), Some(1.5)]));
        assert_eq!((p.sent, p.lost, p.dead), (4, 0, 0));
        assert_eq!((p.now, p.min, p.max), (Some(1.5), Some(1.0), Some(2.0)));
        assert!((p.avg.unwrap() - 1.5).abs() < 1e-6);
        // |2-1| + |1.5-2| + |1.5-1.5| over 3 pairs
        assert!((p.jitter.unwrap() - 0.5).abs() < 1e-6);
        assert_eq!(p.loss_pct, 0.0);
        assert_eq!(p.silent_s, 1.0);
    }

    #[test]
    fn loss_dead_and_silence() {
        let p = path_stats(&s(&[Some(1.0), None, Some(3.0), None, None]));
        assert_eq!((p.sent, p.lost, p.dead), (5, 3, 2));
        assert!((p.loss_pct - 60.0).abs() < 1e-4);
        assert_eq!(p.now, None);
        // jitter needs two consecutive answers: none here
        assert_eq!(p.jitter, None);
        assert_eq!(p.silent_s, 5.0, "the last answer was 5 s ago");
        assert!(!p.is_dead());
        let d = path_stats(&s(&[Some(1.0), None, None, None]));
        assert!(d.is_dead() && !d.never_answered());
        let n = path_stats(&s(&[None, None, None]));
        assert!(n.is_dead() && n.never_answered());
        assert_eq!((n.avg, n.min, n.max), (None, None, None));
        assert_eq!(n.silent_s, 5.0);
        assert_eq!(path_stats(&[]), PathStats::default());
    }

    #[test]
    fn stats_use_the_last_minute_only() {
        let mut v = s(&[Some(500.0), Some(1.0), Some(1.0)]);
        v[0].0 = 61.0;
        let p = path_stats(&v);
        assert_eq!((p.sent, p.max), (2, Some(1.0)));
    }

    #[test]
    fn jitter_covers_the_last_30_pings() {
        // 40 pings: the first 10 alternate wildly, the last 30 are steady
        let mut v: Vec<Option<f32>> = (0..10).map(|i| Some(if i % 2 == 0 { 1.0 } else { 100.0 })).collect();
        v.extend((0..30).map(|_| Some(2.0)));
        let p = path_stats(&s(&v));
        assert_eq!(p.jitter, Some(0.0));
    }

    #[test]
    fn columns_place_rounds_by_age_and_series_bridges_one_gap() {
        let v = [(1.0, Some(2.0)), (3.0, Some(4.0)), (5.0, None), (7.0, Some(8.0))];
        // oldest first, as the ring hands them over
        let v: Vec<_> = v.into_iter().rev().collect();
        let c = columns(&v);
        assert_eq!(c.len(), HIST);
        assert_eq!((c[HIST - 2], c[HIST - 4], c[HIST - 6], c[HIST - 8]), (Col::Ms(2.0), Col::Ms(4.0), Col::Lost, Col::Ms(8.0)));
        assert_eq!(c[HIST - 1], Col::None);
        let sr = series(&c);
        assert!((sr[HIST - 3] - 3.0).abs() < 1e-6, "one empty column between two answers is the line between them");
        assert!(sr[HIST - 5].is_nan() && sr[HIST - 6].is_nan(), "a loss is a gap, never an interpolation");
        assert_eq!(lost_cols(&c), vec![HIST - 6]);
        assert_eq!(col_at(&c, HIST - 1), Col::Ms(2.0), "an empty column reads the neighbour round");
        assert_eq!(col_at(&c, 3), Col::None);
    }

    #[test]
    fn levels_follow_the_mock_thresholds() {
        let mk = |avg, jit, loss| PathStats { sent: 30, now: Some(avg), avg: Some(avg), jitter: Some(jit), loss_pct: loss, ..Default::default() };
        assert_eq!(gw_level(&mk(1.0, 0.5, 0.0)), Lv::Ok);
        assert_eq!(gw_level(&mk(15.0, 0.5, 0.0)), Lv::Warn);
        assert_eq!(gw_level(&mk(1.0, 15.0, 0.0)), Lv::Hot);
        assert_eq!(gw_level(&mk(1.0, 0.5, 1.0)), Lv::Warn);
        assert_eq!(gw_level(&mk(1.0, 0.5, 5.0)), Lv::Hot);
        assert_eq!(inet_level(&mk(79.0, 14.0, 0.0)), Lv::Ok);
        assert_eq!(inet_level(&mk(80.0, 1.0, 0.0)), Lv::Warn);
        assert_eq!(inet_level(&mk(200.0, 1.0, 0.0)), Lv::Hot);
        assert_eq!(gw_level(&PathStats { sent: 3, lost: 3, dead: 3, loss_pct: 100.0, ..Default::default() }), Lv::Hot);
        assert_eq!((dns_level(Dns::Ms(99.0)), dns_level(Dns::Ms(100.0)), dns_level(Dns::Ms(400.0)), dns_level(Dns::Failed)), (Lv::Ok, Lv::Warn, Lv::Hot, Lv::Hot));
        assert_eq!((dns_level(Dns::Waiting), dns_level(Dns::NotProbed)), (Lv::Ok, Lv::Ok));
    }

    const ALL: Shape = Shape { gw: true, dns: true, dns_local: false, inet: InetState::Probed };

    #[test]
    fn view_is_stale_without_a_recent_round_and_counts_traffic() {
        let t0 = Instant::now();
        let r = |sec: u64, gw: Probed| Round { at: t0 + Duration::from_secs(sec), gw, inet: Probed::Ms(10.0), dns: Probed::Ms(14.0) };
        let rounds: Vec<Round> = (0..30).map(|i| r(i * 2, Probed::Ms(0.8))).collect();
        let v = view_from(&rounds, t0 + Duration::from_secs(60), ALL, FRESH_S);
        assert!(v.live);
        assert_eq!(v.rounds, 30);
        assert!((v.kb_min - 30.0 * (296.0 + 175.0) / 1000.0).abs() < 0.01, "about 14 KB/min with three targets: {}", v.kb_min);
        assert_eq!(v.dns, Dns::Ms(14.0));
        assert_eq!(v.gw.as_ref().unwrap().stats.sent, 30);
        // the detail was hidden for a while: no current data, history is a gap
        let old = view_from(&rounds, t0 + Duration::from_secs(70), ALL, FRESH_S);
        assert!(!old.live && old.kb_min == 0.0 && old.dns == Dns::Waiting);
        // no gateway on the adapter: no gateway path, two echoes' worth of bytes less
        let none = view_from(&rounds, t0 + Duration::from_secs(60), Shape { gw: false, dns: false, dns_local: false, inet: InetState::Probed }, FRESH_S);
        assert!(none.gw.is_none() && none.dns == Dns::NotProbed);
        assert!((none.kb_min - 30.0 * 148.0 / 1000.0).abs() < 0.01);
        // a failed lookup is the newest result
        let mut lost = rounds.clone();
        lost.last_mut().unwrap().dns = Probed::Lost;
        assert_eq!(view_from(&lost, t0 + Duration::from_secs(60), ALL, FRESH_S).dns, Dns::Failed);
        assert_eq!(view_from(&[], t0, ALL, FRESH_S).rounds, 0);
    }

    // ---- internet = none, routing, local resolver, idle backoff ----

    #[test]
    fn internet_none_is_accepted_and_round_trips() {
        for v in ["none", "None", "NONE", "off"] {
            let s = parse_ini(&format!("[net_probes]\nenabled = true\ninternet = {v}\n"));
            assert_eq!(s.internet, None, "{v}");
            assert!(s.warn.is_none() && s.enabled, "none is a choice, not a mistake: {v}");
        }
        let s = Settings { enabled: true, internet: None, ..Settings::default() };
        let text = render_ini(&s);
        assert!(text.contains("internet = none"));
        assert_eq!(parse_ini(&text), s);
        // a bad address still falls back to the default and warns
        let bad = parse_ini("[net_probes]\ninternet = nope\n");
        assert_eq!(bad.internet, Some(DEFAULT_INTERNET));
        assert!(bad.warn.is_some());
        // no external destination: the config sends no internet echo and the view says so
        let c = cfg_for(7, "10.0.0.2 / 24", "10.0.0.1", "10.0.0.1", &s, "wired", Some(7)).unwrap();
        assert_eq!((c.inet, c.inet.target()), (Inet::Off, None));
        let t0 = Instant::now();
        let r = Round { at: t0, gw: Probed::Ms(1.0), inet: Probed::Skipped, dns: Probed::Ms(9.0) };
        let v = view_from(&[r], t0, Shape::of(&c), FRESH_S);
        assert_eq!((v.inet_state, v.inet.stats.sent, v.live), (InetState::Off, 0, true));
        assert!((v.kb_min - (148.0 + 175.0) / 1000.0).abs() < 0.01, "one echo and one lookup per round: {}", v.kb_min);
    }

    #[test]
    fn internet_probe_follows_the_route_not_the_adapter() {
        let st = Settings::default();
        let go = |luid, kind, via| cfg_for(luid, "10.0.0.2 / 24", "10.0.0.1", "10.0.0.1", &st, kind, via).unwrap().inet;
        // the OS routes the target through this adapter: probed
        assert_eq!(go(7, "wired", Some(7)), Inet::Target(DEFAULT_INTERNET));
        // through another one (a VPN default route): the physical adapter does not test the internet
        assert_eq!(go(7, "wired", Some(9)), Inet::NotRouted);
        // the VPN adapter that is the route is probed even though it is "virtual"
        assert_eq!(go(9, "virtual", Some(9)), Inet::Target(DEFAULT_INTERNET));
        // route unknown: a virtual adapter is not probed, a physical one is
        assert_eq!(go(9, "virtual", None), Inet::NotRouted);
        assert_eq!(go(7, "wireless", None), Inet::Target(DEFAULT_INTERNET));
        assert_eq!(Inet::NotRouted.state(), InetState::NotRouted);
        // gateway and DNS of a not-routed adapter are still probed
        let c = cfg_for(7, "10.0.0.2 / 24", "10.0.0.1", "10.0.0.1", &st, "wired", Some(9)).unwrap();
        assert!(c.gw.is_some() && c.dns.is_some() && c.inet.target().is_none());
    }

    #[test]
    fn a_loopback_stub_resolver_is_local_not_missing() {
        let st = Settings::default();
        let c = cfg_for(7, "10.0.0.2 / 24", "10.0.0.1", "127.0.0.53", &st, "wired", None).unwrap();
        assert!(c.dns.is_none() && c.dns_local);
        let t0 = Instant::now();
        let r = Round { at: t0, gw: Probed::Ms(1.0), inet: Probed::Ms(9.0), dns: Probed::Skipped };
        assert_eq!(view_from(&[r], t0, Shape::of(&c), FRESH_S).dns, Dns::Local);
        // no DNS server at all stays "none"
        let c = cfg_for(7, "10.0.0.2 / 24", "10.0.0.1", "", &st, "wired", None).unwrap();
        assert!(c.dns.is_none() && !c.dns_local);
        assert_eq!(view_from(&[r], t0, Shape::of(&c), FRESH_S).dns, Dns::NotProbed);
        assert_eq!(dns_level(Dns::Local), Lv::Ok);
    }

    fn ms_round(t0: Instant, gw: Probed, inet: Probed, dns: Probed) -> Round {
        Round { at: t0, gw, inet, dns }
    }

    #[test]
    fn idle_backoff_after_ten_calm_minutes_and_back_on_any_trouble() {
        let t0 = Instant::now();
        let mut c = Calm::default();
        assert_eq!(c.next_period(t0, true), PERIOD, "the first calm round starts the clock");
        assert_eq!(c.next_period(t0 + IDLE_AFTER - Duration::from_secs(1), true), PERIOD);
        assert_eq!(c.next_period(t0 + IDLE_AFTER, true), IDLE_PERIOD, "10 minutes of calm: slow down");
        assert_eq!(c.next_period(t0 + IDLE_AFTER + IDLE_PERIOD, true), IDLE_PERIOD);
        // one bad round: fast again, and the clock restarts
        assert_eq!(c.next_period(t0 + IDLE_AFTER + Duration::from_secs(20), false), PERIOD);
        assert_eq!(c.next_period(t0 + IDLE_AFTER + Duration::from_secs(22), true), PERIOD);
        assert_eq!(c.next_period(t0 + IDLE_AFTER * 2, true), PERIOD, "less than 10 minutes since the trouble");
        // a tab / adapter change resets it
        let mut c = Calm::default();
        c.next_period(t0, true);
        c.reset();
        assert_eq!(c.next_period(t0 + IDLE_AFTER, true), PERIOD);
    }

    #[test]
    fn a_round_is_clean_only_when_every_probe_is_fast() {
        let t0 = Instant::now();
        let h: VecDeque<Round> = VecDeque::new();
        let good = ms_round(t0, Probed::Ms(1.0), Probed::Ms(12.0), Probed::Ms(14.0));
        assert!(is_clean(&h, &good));
        assert!(is_clean(&h, &ms_round(t0, Probed::Skipped, Probed::Skipped, Probed::Skipped)), "nothing probed is quiet");
        assert!(!is_clean(&h, &ms_round(t0, Probed::Ms(20.0), Probed::Ms(12.0), Probed::Ms(14.0))), "gateway over its warn level");
        assert!(!is_clean(&h, &ms_round(t0, Probed::Ms(1.0), Probed::Ms(90.0), Probed::Ms(14.0))), "internet over its warn level");
        assert!(!is_clean(&h, &ms_round(t0, Probed::Ms(1.0), Probed::Ms(12.0), Probed::Ms(150.0))), "slow lookup");
        assert!(!is_clean(&h, &ms_round(t0, Probed::Ms(1.0), Probed::Lost, Probed::Ms(14.0))), "a loss is trouble");
        // a gateway that has never answered (ignores ping) is steady, not trouble
        let silent: VecDeque<Round> = (0..3).map(|_| ms_round(t0, Probed::Lost, Probed::Ms(12.0), Probed::Ms(14.0))).collect();
        assert!(is_clean(&silent, &ms_round(t0, Probed::Lost, Probed::Ms(12.0), Probed::Ms(14.0))));
        // but one that answered before and now does not is
        let mut was: VecDeque<Round> = silent.clone();
        was.push_front(good);
        assert!(!is_clean(&was, &ms_round(t0, Probed::Lost, Probed::Ms(12.0), Probed::Ms(14.0))));
    }

    #[test]
    fn freshness_scales_with_the_period() {
        let t0 = Instant::now();
        let r = ms_round(t0, Probed::Ms(1.0), Probed::Ms(12.0), Probed::Ms(14.0));
        let at = t0 + Duration::from_secs(9);
        assert!(!view_from(&[r], at, ALL, FRESH_S).live, "9 s old at the 2 s pace: stale");
        assert!(view_from(&[r], at, ALL, IDLE_PERIOD.as_secs_f32() * 2.0 + 2.0).live, "9 s old at the 10 s pace: current");
    }
}
