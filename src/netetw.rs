//! Per-process network traffic for Vitals > Network "TOP BY NETWORK".
//!
//! Source: ETW real-time session "Hayate-Kernel-Network" on
//! Microsoft-Windows-Kernel-Network ({7dd42a49-5329-4832-8dfd-43d979153a88}),
//! keywords 0x10 (IPv4) | 0x20 (IPv6), event-ID filter in the kernel to the
//! eight data-transfer events:
//!
//! | id | event | direction |
//! |----|-------|-----------|
//! | 10 / 11 | TCPv4 send / receive | up / down |
//! | 26 / 27 | TCPv6 send / receive | up / down |
//! | 42 / 43 | UDPv4 send / receive | up / down |
//! | 58 / 59 | UDPv6 send / receive | up / down |
//!
//! Payload (version 0), every event: `PID u32, size u32, daddr, saddr, dport
//! u16, sport u16, ...` (addresses 4 bytes IPv4, 16 bytes IPv6, network order).
//! `size` is the transport payload byte count ("bytes transmitted/received"
//! in the manifest message), so it runs under NIC counters (no L2/L3/L4 headers, no pure
//! TCP ACKs: Up reads lowest). The Top by Network panel says so (footnote + row tooltip). TCP retransmits (14/30) and TCPCopy (18/34) are
//! different events and are NOT counted: they would double-count bytes.
//! (They feed `netconn`, which counts failures and retransmits per process and remote, never bytes.)
//!
//! The same session also takes the connection events 12/13/14/16/17 (+16 for IPv6): ids and
//! confirmed meanings are documented at the top of `netconn.rs` (15 is the accept side, there is
//! no outbound "completed" event). They add no outbound traffic and are only enabled with the
//! same session: Network detail visible and Hayate elevated.
//! Direction comes from the event ID, never from the addresses. Traffic whose
//! two endpoints are the same host (loopback, own address) never touches a NIC
//! and is skipped.
//!
//! Scope (LAN / WAN / other) is judged on the REMOTE endpoint only. The manifest does
//! not say which of `daddr` / `saddr` is the peer on a receive, so it was MEASURED
//! (`netetw_scopes`, elevated, one TCP and one UDP exchange with a LAN host and with
//! 1.1.1.1): TCP events (10/11/26/27) carry the peer in `daddr` for both directions;
//! UDP events (42/43/58/59) keep the packet order, a send to `daddr`, a receive from
//! `saddr`. The IPv6 TCP roles follow the IPv4 TCP ones (same event class) but were not
//! measured live: this rig has no routable IPv6 peer. `netetw_scopes` re-checks the
//! IPv4 cases against a LAN and a public peer. The
//! address is classified in the callback and dropped: only per-process byte counters
//! are kept, nothing about an endpoint is stored or logged. The scope follows the
//! remote address, not the route: a VPN, tunnel or relay shows its own endpoint,
//! which can differ from the physical path. Ranges follow the IANA IPv4 / IPv6
//! special-purpose registries, see [`classify`].
//!
//! There is no adapter in the payload, so the numbers are
//! all adapters (the UI says so). Only PID + size + addresses are read;
//! no packet contents exist in these events.
//!
//! Needs admin (StartTrace / EnableTraceEx2 return ACCESS_DENIED otherwise).
//! The session exists only while Vitals > Network is open (+ a short grace),
//! so it costs nothing the rest of the time. Memory is bounded: 4-16 x 64 KB
//! ETW buffers, and at most `MAX_PIDS` accumulator entries.
//!
//! Sessions are named per instance (`Hayate-Kernel-Network-<pid>-<create>-<n>`) and
//! stopped only through the handle that started them. A leftover session is
//! stopped only when the process named in it provably no longer exists; one of a
//! live Hayate, or of unknown owner, is never touched.
//!
//! Rates are computed per sampler tick by [`Collector::tick`]: bytes since the
//! last drain divided by the real elapsed time. Bytes of a PID count only when
//! that PID names the same process (same non-zero create time) at BOTH ends of
//! the interval, so a reused PID never inherits the old process's traffic, and
//! a process that started inside the interval waits for the next one. The feed
//! keeps every attributed process (at most `MAX_PIDS`); lists cut it to what
//! they show only after grouping (Apps sums a browser's workers first).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Diagnostics::Etw::*;

use crate::osutil::wide;

const PROVIDER: GUID = GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);
const KEYWORDS: u64 = 0x10 | 0x20;
/// Accumulator cap: distinct PIDs per interval. Beyond it bytes are counted in `overflow`.
pub const MAX_PIDS: usize = 4096;
/// Session stays up this long after Network detail was last wanted (tab flips).
pub const IDLE_STOP_MS: u64 = 20_000;
/// Ticks closer than this are not drained (jitter), longer gaps than `MAX_GAP_MS` restart the baseline.
const MIN_GAP_MS: u64 = 250;
const MAX_GAP_MS: u64 = 10_000;
/// Retry delays after a session failure (not for "needs admin").
const BACKOFF_MS: [u64; 4] = [5_000, 15_000, 60_000, 300_000];
/// Admin does not appear mid-run; try again only rarely (group membership change, etc).
const ADMIN_RETRY_MS: u64 = 600_000;

// ---- decode -------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Down,
    Up,
}

/// Where the remote end of a flow is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// private / on-link: RFC 1918, link-local, ULA, link-scope multicast and broadcast
    Lan,
    /// globally routable unicast
    Wan,
    /// everything else: unspecified, CGNAT / shared, tunnel prefixes, documentation,
    /// benchmarking, reserved, wider-scope multicast. Never counted as LAN or WAN.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Traffic {
    pub pid: u32,
    pub bytes: u32,
    pub dir: Dir,
    pub scope: Scope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoded {
    /// Not a data-transfer event (connect, disconnect, retransmit, ...) or same-host traffic.
    Skip,
    /// A data event whose payload is not the layout we know (version, truncated).
    Bad,
    Traffic(Traffic),
}

/// (direction, address length) for the eight data-transfer event IDs.
fn kind(id: u16) -> Option<(Dir, usize)> {
    Some(match id {
        10 | 42 => (Dir::Up, 4),
        11 | 43 => (Dir::Down, 4),
        26 | 58 => (Dir::Up, 16),
        27 | 59 => (Dir::Down, 16),
        _ => return None,
    })
}

/// TCP data events (v4 send/receive, v6 send/receive). Their `daddr` is always the peer.
fn tcp(id: u16) -> bool {
    matches!(id, 10 | 11 | 26 | 27)
}

/// The data-transfer event IDs (the kernel-side filter).
pub const DATA_EVENTS: [u16; 8] = [10, 11, 26, 27, 42, 43, 58, 59];

fn same_host(d: &[u8], s: &[u8]) -> bool {
    if d == s {
        return true;
    }
    let loop4 = |a: &[u8]| a[0] == 127;
    match d.len() {
        4 => loop4(d) && loop4(s),
        _ => {
            let is_loop6 = |a: &[u8]| a[..15].iter().all(|b| *b == 0) && a[15] == 1;
            // ::ffff:127.x.x.x (v4-mapped loopback)
            let mapped_loop = |a: &[u8]| a[..10].iter().all(|b| *b == 0) && a[10] == 0xff && a[11] == 0xff && a[12] == 127;
            (is_loop6(d) || mapped_loop(d)) && (is_loop6(s) || mapped_loop(s))
        }
    }
}

/// Scope of an IPv4 address: IANA IPv4 Special-Purpose Address Registry (RFC 6890
/// and updates) and RFC 1918.
fn classify4(a: [u8; 4]) -> Scope {
    match (a[0], a[1], a[2], a[3]) {
        // RFC 1918 private use
        (10, ..) | (192, 168, ..) => Scope::Lan,
        (172, b, ..) if (16..=31).contains(&b) => Scope::Lan,
        // RFC 3927 link-local: on-link by definition
        (169, 254, ..) => Scope::Lan,
        // Limited broadcast; all multicast remains Other regardless of scope.
        (255, 255, 255, 255) => Scope::Lan,
        // Globally reachable exceptions inside the protocol-assignment block.
        (192, 0, 0, 9 | 10) => Scope::Wan,
        // 0/8 "this network"; 127/8 loopback (a flow that is not same-host but has a loopback peer is
        // no real LAN/WAN peer); 192.0.0.0/24 IETF protocol assignments; documentation blocks
        // 192.0.2/24, 198.51.100/24, 203.0.113/24; 192.88.99/24 deprecated 6to4 relay anycast
        (0, ..) | (127, ..) | (192, 0, 0, _) | (192, 0, 2, _) | (192, 88, 99, _) | (198, 51, 100, _) | (203, 0, 113, _) => Scope::Other,
        // 100.64.0.0/10 shared address space (carrier-grade NAT, many VPN overlays)
        (100, b, ..) if (64..=127).contains(&b) => Scope::Other,
        // 198.18.0.0/15 benchmarking
        (198, 18 | 19, ..) => Scope::Other,
        // 224/4 other multicast (scope varies), 240/4 reserved
        (224..=255, ..) => Scope::Other,
        _ => Scope::Wan,
    }
}

/// Scope of an IPv6 address: IANA IPv6 Special-Purpose Address Registry, RFC 4193
/// (unique local), RFC 4291 (multicast scopes). Only 2000::/3 minus the special
/// blocks is WAN; unallocated space is Other, never silently public.
fn classify6(a: &[u8]) -> Scope {
    // ::ffff:0:0/96 IPv4-mapped: the IPv4 address decides
    if a[..10].iter().all(|b| *b == 0) && a[10] == 0xff && a[11] == 0xff {
        return classify4([a[12], a[13], a[14], a[15]]);
    }
    // 64:ff9b::/96 NAT64: the embedded IPv4 server decides (64:ff9b:1::/48 local-use falls to Other)
    if a[0] == 0 && a[1] == 0x64 && a[2] == 0xff && a[3] == 0x9b && a[4..12].iter().all(|b| *b == 0) {
        return if classify4([a[12], a[13], a[14], a[15]]) == Scope::Wan { Scope::Wan } else { Scope::Other };
    }
    // fe80::/10 link-local unicast, fc00::/7 unique local
    if (a[0] == 0xfe && a[1] & 0xc0 == 0x80) || a[0] & 0xfe == 0xfc {
        return Scope::Lan;
    }
    // Multicast is Other regardless of scope.
    if a[0] == 0xff {
        return Scope::Other;
    }
    // IANA globally reachable exceptions, before enclosing protocol exclusions.
    let prefix = u32::from_be_bytes(a[..4].try_into().unwrap());
    if (prefix == 0x20010001 && a[4..15].iter().all(|b| *b == 0) && matches!(a[15], 1..=3))
        || prefix == 0x20010003
        || a[..6] == [0x20, 0x01, 0, 4, 1, 0x12]
        || matches!(prefix & 0xfffffff0, 0x20010020 | 0x20010030)
    {
        return Scope::Wan;
    }
    // 2000::/3 global unicast, minus 2001::/23 (protocol assignments, Teredo), 2001:db8::/32
    // (documentation), 2002::/16 (6to4), 3fff::/20 (documentation)
    if a[0] & 0xe0 == 0x20 {
        let special = (a[0] == 0x20 && a[1] == 0x01 && a[2] & 0xfe == 0)
            || (a[0] == 0x20 && a[1] == 0x01 && a[2] == 0x0d && a[3] == 0xb8)
            || (a[0] == 0x20 && a[1] == 0x02)
            || (a[0] == 0x3f && a[1] == 0xff && a[2] & 0xf0 == 0);
        return if special { Scope::Other } else { Scope::Wan };
    }
    // ::, ::1, ::/96 IPv4-compatible, 64::/10 and the rest of unallocated space
    Scope::Other
}

/// Scope of a 4- or 16-byte network-order address. Pure, no allocation.
pub fn classify(a: &[u8]) -> Scope {
    match a.len() {
        4 => classify4([a[0], a[1], a[2], a[3]]),
        16 => classify6(a),
        _ => Scope::Other,
    }
}

/// Decode one event's raw `UserData`. Pure: no Windows calls.
pub fn decode(id: u16, version: u8, data: &[u8]) -> Decoded {
    let Some((dir, alen)) = kind(id) else { return Decoded::Skip };
    // PID, size, daddr, saddr, dport, sport
    let need = 8 + alen * 2 + 4;
    if version != 0 || data.len() < need {
        return Decoded::Bad;
    }
    let pid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let bytes = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let d = &data[8..8 + alen];
    let s = &data[8 + alen..8 + alen * 2];
    if same_host(d, s) {
        return Decoded::Skip;
    }
    // The remote end. TCP events name the peer in `daddr` for BOTH directions (measured
    // live on a send and a receive of one connection); UDP events keep the packet's own
    // order: a send goes to `daddr`, a receive came from `saddr`.
    let remote = if tcp(id) || dir == Dir::Up { d } else { s };
    Decoded::Traffic(Traffic { pid, bytes, dir, scope: classify(remote) })
}

// ---- bounded accumulator -----------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bytes {
    pub down: u64,
    pub up: u64,
    /// the part of `down` / `up` whose remote end is LAN / WAN; the rest is Other
    pub lan_down: u64,
    pub lan_up: u64,
    pub wan_down: u64,
    pub wan_up: u64,
}

#[derive(Debug, Default)]
pub struct Acc {
    pub map: HashMap<u32, Bytes>,
    /// bytes that did not fit (`MAX_PIDS` distinct PIDs in one interval)
    pub overflow: u64,
    /// data events with an unknown payload layout
    pub bad: u64,
}

impl Acc {
    pub fn add(&mut self, t: Traffic) {
        let n = t.bytes as u64;
        if !self.map.contains_key(&t.pid) && self.map.len() >= MAX_PIDS {
            self.overflow = self.overflow.saturating_add(n);
            return;
        }
        let e = self.map.entry(t.pid).or_default();
        let (total, lan, wan) = match t.dir {
            Dir::Down => (&mut e.down, &mut e.lan_down, &mut e.wan_down),
            Dir::Up => (&mut e.up, &mut e.lan_up, &mut e.wan_up),
        };
        *total = total.saturating_add(n);
        match t.scope {
            Scope::Lan => *lan = lan.saturating_add(n),
            Scope::Wan => *wan = wan.saturating_add(n),
            Scope::Other => {}
        }
    }
}

// ---- feed ----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not collecting (Network detail not open, or never asked).
    Off,
    /// Session is up, first interval not complete yet.
    Collecting,
    /// Session is up; `rows` are real rates (an empty list means no process traffic).
    Live,
    /// StartTrace / EnableTraceEx2 refused: needs an elevated Hayate.
    NeedsAdmin,
    /// Session could not start or died; retried with backoff.
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetRow {
    pub pid: u32,
    /// process create time (FILETIME): with `pid` the identity
    pub create: i64,
    /// bytes per second, all scopes (the total every component adds up to)
    pub down: u64,
    pub up: u64,
    /// the part of `down` / `up` with a LAN / WAN remote end; the rest is Other
    pub lan_down: u64,
    pub lan_up: u64,
    pub wan_down: u64,
    pub wan_up: u64,
}

/// Which remote scope a list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    All,
    Lan,
    Wan,
}

impl NetRow {
    pub fn total(&self) -> u64 {
        self.down.saturating_add(self.up)
    }
    /// (down, up) of a view; `All` is the full total.
    pub fn of(&self, v: View) -> (u64, u64) {
        match v {
            View::All => (self.down, self.up),
            View::Lan => (self.lan_down, self.lan_up),
            View::Wan => (self.wan_down, self.wan_up),
        }
    }
    /// (down, up) whose remote end is neither LAN nor WAN.
    pub fn other(&self) -> (u64, u64) {
        (self.down.saturating_sub(self.lan_down.saturating_add(self.wan_down)), self.up.saturating_sub(self.lan_up.saturating_add(self.wan_up)))
    }
}

/// Down / up byte rates per scope: `lan + wan + other == all`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScopeTotals {
    pub all: (u64, u64),
    pub lan: (u64, u64),
    pub wan: (u64, u64),
    pub other: (u64, u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feed {
    pub status: Status,
    /// Sorted by combined rate, highest first. Only processes that moved bytes in the interval.
    pub rows: Vec<NetRow>,
    /// ETW lost events/buffers since the session started (rates under-count when > 0).
    pub lost: u64,
    /// bytes dropped from the last interval because their PID did not name one process at both ends
    pub unattributed: u64,
    /// data events of an unknown layout since the session started: with rows empty
    /// this is "cannot read", not "idle"
    pub bad: u64,
    /// The measured interval these rows average (`Live` only; `None` for every other state).
    /// Cached copies of the same feed carry the same `id`.
    pub iv: Option<Interval>,
    /// Connection health per process + remote (failed connects, retransmits, drops), `netconn`.
    /// Empty unless `Live`. Cached copies carry the same value.
    pub conn: crate::netconn::ConnFeed,
}

/// One completed ETW measurement interval: what a rolling history needs to place it in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    /// grows by one per interval, never reset (also across session restarts)
    pub id: u64,
    /// collector clock (monotonic ms) at the end of the interval
    pub end_ms: u64,
    /// how long the rows average over
    pub span_ms: u64,
}

impl Feed {
    pub const OFF: Feed = Feed { status: Status::Off, rows: Vec::new(), lost: 0, unattributed: 0, bad: 0, iv: None, conn: crate::netconn::ConnFeed::EMPTY };

    /// Sum of every attributed process. Bytes of PIDs that could not be attributed are
    /// counted in `unattributed`, not here.
    pub fn totals(&self) -> ScopeTotals {
        let mut t = ScopeTotals::default();
        let add = |a: &mut (u64, u64), b: (u64, u64)| {
            a.0 = a.0.saturating_add(b.0);
            a.1 = a.1.saturating_add(b.1);
        };
        for r in &self.rows {
            add(&mut t.all, (r.down, r.up));
            add(&mut t.lan, (r.lan_down, r.lan_up));
            add(&mut t.wan, (r.wan_down, r.wan_up));
            add(&mut t.other, r.other());
        }
        t
    }
}

impl Default for Feed {
    fn default() -> Self {
        Feed::OFF
    }
}

/// Pure attribution step (testable): turn one interval of per-PID bytes into rows.
/// `prev`/`cur` map PID to create time at the start / end of the interval.
/// A PID counts only when BOTH maps hold the same non-zero create time for it: a
/// process that was not there at the start (it may have replaced another one that
/// used the PID earlier in the interval) or whose time is unknown is not attributed.
/// Rows are complete (the accumulator already bounds the PIDs); callers cut them
/// after grouping. Returns (rows, unattributed bytes).
fn scope_rates(total: u64, lan: u64, wan: u64, ms: u64) -> (u64, u64, u64) {
    if total == 0 { return (0, 0, 0); }
    let rate = ((total as u128 * 1000) / ms.max(1) as u128).min(u64::MAX as u128) as u64;
    let lan = lan.min(total);
    let wan = wan.min(total - lan);
    let raw = [lan, wan, total - lan - wan];
    let scaled = raw.map(|n| n as u128 * rate as u128);
    let mut parts = scaled.map(|n| (n / total as u128) as u64);
    let mut order = [0, 1, 2];
    order.sort_by_key(|&i| std::cmp::Reverse(scaled[i] % total as u128));
    // Largest-remainder allocation preserves the total without crediting empty scopes.
    for i in order.into_iter().take((rate - parts.iter().sum::<u64>()) as usize) {
        parts[i] += 1;
    }
    (rate, parts[0], parts[1])
}

/// Name the process of each connection pair. The first tick a PID has a pair, its current create
/// time is remembered in `bound`; later ticks keep that identity and DROP the pair when the PID now
/// names a different process (recycled PID), so a new process never inherits another one's failures.
/// A PID that is gone, or has no known start time, is dropped too. `bound` forgets PIDs without pairs.
pub fn bind_pairs(pairs: &mut Vec<crate::netconn::ConnPair>, ids: &HashMap<u32, i64>, bound: &mut HashMap<u32, i64>) {
    pairs.retain_mut(|p| match ids.get(&p.pid) {
        Some(&c) if c != 0 => {
            let first = *bound.entry(p.pid).or_insert(c);
            p.create = first;
            first == c
        }
        _ => false, // a process that is gone cannot be named
    });
    let live: std::collections::HashSet<u32> = pairs.iter().map(|p| p.pid).collect();
    bound.retain(|pid, _| live.contains(pid));
}

pub fn attribute(drained: &HashMap<u32, Bytes>, prev: &HashMap<u32, i64>, cur: &HashMap<u32, i64>, ms: u64) -> (Vec<NetRow>, u64) {
    let ms = ms.max(1);
    let mut rows = Vec::new();
    let mut lost = 0u64;
    for (&pid, b) in drained {
        let ok = match (prev.get(&pid), cur.get(&pid)) {
            (Some(p), Some(c)) if p == c && *c != 0 => Some(*c),
            _ => None, // exited, not seen at the start, replaced, or no known start time
        };
        match ok {
            Some(create) if b.down + b.up > 0 => {
                let (down, lan_down, wan_down) = scope_rates(b.down, b.lan_down, b.wan_down, ms);
                let (up, lan_up, wan_up) = scope_rates(b.up, b.lan_up, b.wan_up, ms);
                rows.push(NetRow { pid, create, down, up, lan_down, lan_up, wan_down, wan_up });
            }
            Some(_) => {}
            None => lost = lost.saturating_add(b.down).saturating_add(b.up),
        }
    }
    rows.sort_by(|a, b| b.total().cmp(&a.total()).then(a.pid.cmp(&b.pid)));
    (rows, lost)
}

// ---- collector state machine --------------------------------------------------------------

pub enum StartErr {
    NeedsAdmin,
    Other(String),
}

/// What the collector needs from the event source; the ETW session in
/// production, a fake in tests.
pub trait Backend {
    fn start(&mut self) -> Result<(), StartErr>;
    /// Session still delivering (false: killed from outside / trace ended).
    fn alive(&mut self) -> bool;
    fn stop(&mut self);
    /// Take everything accumulated since the last call.
    fn take(&mut self) -> Acc;
    /// Cumulative lost events + buffers since `start`.
    fn lost(&mut self) -> u64;
    /// Connection health (failed connects, retransmits) since `start`; empty when unsupported.
    fn conn(&mut self) -> crate::netconn::ConnFeed {
        crate::netconn::ConnFeed::EMPTY
    }
}

pub struct Collector<B: Backend> {
    be: B,
    running: bool,
    /// first drain after start only sets the baseline
    warm: bool,
    status: Status,
    next_try: u64,
    fails: usize,
    idle_since: Option<u64>,
    last: u64,
    prev: HashMap<u32, i64>,
    /// PID -> create time the connection pairs of that PID were first bound to (`bind_pairs`)
    conn_ids: HashMap<u32, i64>,
    feed: Feed,
    /// undecodable data events since the session started
    bad: u64,
    /// intervals produced so far (`Interval::id`)
    ivs: u64,
}

impl<B: Backend> Collector<B> {
    pub fn new(be: B) -> Self {
        Collector {
            be,
            running: false,
            warm: false,
            status: Status::Off,
            next_try: 0,
            fails: 0,
            idle_since: None,
            last: 0,
            prev: HashMap::new(),
            conn_ids: HashMap::new(),
            feed: Feed::OFF,
            bad: 0,
            ivs: 0,
        }
    }

    #[cfg(test)]
    pub fn running(&self) -> bool {
        self.running
    }

    #[cfg(test)]
    pub fn backend(&self) -> &B {
        &self.be
    }

    fn fail(&mut self, now: u64, why: String) {
        if self.running {
            self.be.stop();
        }
        self.running = false;
        self.status = Status::Unavailable(why);
        self.next_try = now + BACKOFF_MS[self.fails.min(BACKOFF_MS.len() - 1)];
        self.fails += 1;
        self.reset_baseline();
    }

    fn reset_baseline(&mut self) {
        self.warm = false;
        self.prev.clear();
        self.conn_ids.clear();
        self.bad = 0;
        self.feed = Feed { status: self.status.clone(), ..Feed::OFF };
    }

    /// Whether `ids` (PID -> create time of the current process list) is needed this tick.
    pub fn wants_ids(&self, want: bool) -> bool {
        want || self.running
    }

    /// One sampler tick. `want`: Vitals > Network is the visible detail.
    /// `ids`: PID -> create time of every process now (only read while running).
    /// `now`: monotonic milliseconds.
    pub fn tick(&mut self, now: u64, want: bool, ids: &HashMap<u32, i64>) -> Feed {
        if want {
            self.idle_since = None;
        } else if self.running {
            let since = *self.idle_since.get_or_insert(now);
            if now.saturating_sub(since) >= IDLE_STOP_MS {
                self.running = false;
                self.be.stop();
                self.status = Status::Off;
                self.reset_baseline();
            }
        }

        if !self.running {
            if !want {
                self.status = match self.status {
                    // keep the reason visible next time the tab opens until a retry succeeds
                    Status::NeedsAdmin | Status::Unavailable(_) => self.status.clone(),
                    _ => Status::Off,
                };
                // the Off feed is what the UI sees while the tab is hidden
                return Feed { status: Status::Off, ..Feed::OFF };
            }
            if now >= self.next_try {
                match self.be.start() {
                    Ok(()) => {
                        self.running = true;
                        self.status = Status::Collecting;
                        self.last = now;
                        self.reset_baseline();
                        self.prev.clone_from(ids);
                    }
                    Err(StartErr::NeedsAdmin) => {
                        self.status = Status::NeedsAdmin;
                        self.next_try = now + ADMIN_RETRY_MS;
                        self.reset_baseline();
                    }
                    Err(StartErr::Other(e)) => self.fail(now, e),
                }
            }
            self.feed.status = self.status.clone();
            return self.feed.clone();
        }

        // running
        if !self.be.alive() {
            self.fail(now, "network trace session ended".into());
            self.feed.status = self.status.clone();
            return self.feed.clone();
        }
        let gap = now.saturating_sub(self.last);
        if gap < MIN_GAP_MS {
            return self.feed.clone();
        }
        let acc = self.be.take();
        self.bad = self.bad.saturating_add(acc.bad);
        let (lost, bad) = (self.be.lost(), self.bad);
        if !self.warm || gap > MAX_GAP_MS {
            // baseline: the first interval is partial, a stall/sleep makes the average meaningless
            self.warm = true;
            self.last = now;
            self.prev.clone_from(ids);
            self.status = Status::Collecting;
            self.feed = Feed { status: Status::Collecting, lost, bad, ..Feed::OFF };
            return self.feed.clone();
        }
        let (rows, unattributed) = attribute(&acc.map, &self.prev, ids, gap);
        self.last = now;
        self.prev.clone_from(ids);
        if self.fails != 0 {
            // a session that stayed up a full interval is healthy again
            self.fails = 0;
        }
        self.status = Status::Live;
        self.ivs += 1;
        let iv = Some(Interval { id: self.ivs, end_ms: now, span_ms: gap });
        let mut conn = self.be.conn();
        bind_pairs(&mut conn.pairs, ids, &mut self.conn_ids);
        self.feed = Feed { status: Status::Live, rows, lost, unattributed: unattributed.saturating_add(acc.overflow), bad, iv, conn };
        self.feed.clone()
    }

    /// Stop on app exit.
    pub fn shutdown(&mut self) {
        if self.running {
            self.running = false;
            self.be.stop();
        }
        self.status = Status::Off;
        self.reset_baseline();
    }
}

// ---- ETW backend ----------------------------------------------------------------------------

/// Every session this program starts is named `Hayate-Kernel-Network-<pid>-<create>-<n>`:
/// the owner's PID and creation time (hex FILETIME) and a per-process counter. Two
/// Hayates (HAYATE_MULTI, two users, a relaunch that overlaps) therefore never share a
/// name, so neither can stop or take over the other's session, and a leftover session
/// names its dead owner.
const SESSION_PREFIX: &str = "Hayate-Kernel-Network-";
/// Sessions left by the pre-rename build carry the same owner fields under the
/// old product name; the dead-owner cleanup recognises both (one-time sweep).
const LEGACY_SESSION_PREFIX: &str = "Blink-Kernel-Network-";

pub fn session_name(pid: u32, create: i64, seq: u32) -> String {
    format!("{SESSION_PREFIX}{pid}-{create:x}-{seq}")
}

/// Owner (pid, creation time) encoded in one of our session names. `None` for any
/// other name, including the shared names of earlier builds: with no owner to
/// check, those are never touched.
pub fn session_owner(name: &str) -> Option<(u32, i64)> {
    let rest = name.strip_prefix(SESSION_PREFIX).or_else(|| name.strip_prefix(LEGACY_SESSION_PREFIX))?;
    let mut it = rest.split('-');
    let pid: u32 = it.next()?.parse().ok()?;
    let create = i64::from_str_radix(it.next()?, 16).ok()?;
    let _seq: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || pid <= 4 || create == 0 {
        return None;
    }
    Some((pid, create))
}

/// Sessions among `names` that may be stopped: ours, whose owner is provably gone
/// (`gone(pid, create)`: that exact process no longer exists). A session of a live
/// owner, one whose owner cannot be checked, and any name that is not ours are never listed.
pub fn orphans(names: &[String], gone: &dyn Fn(u32, i64) -> bool) -> Vec<String> {
    names.iter().filter(|n| session_owner(n).is_some_and(|(pid, create)| gone(pid, create))).cloned().collect()
}

/// Per-session accumulator. The ETW callback reaches it through the trace's
/// context pointer, so a session being torn down can never feed the next one.
struct Shared {
    acc: Mutex<Acc>,
    /// connection-health tracker (events 12/13/14/16/17 and v6), fed by the callback
    conn: Mutex<crate::netconn::Tracker>,
    t0: QpcClock,
}

/// Milliseconds since the session started, on the system clock. The consumer hands
/// `EventHeader.TimeStamp` over as a FILETIME (no RAW_TIMESTAMP flag), so the connection tracker
/// is timed by when an event HAPPENED, not when the batched real-time delivery handed it over:
/// delivery bunches events by about a second, which would blur the 0.5 s SYN spacing that tells
/// a refused connect from a silent one.
struct QpcClock {
    base: i64,
}

fn filetime_now() -> i64 {
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    let f = unsafe { GetSystemTimePreciseAsFileTime() };
    ((f.dwHighDateTime as i64) << 32) | f.dwLowDateTime as i64
}

impl QpcClock {
    fn new() -> Self {
        QpcClock { base: filetime_now() }
    }
    /// An event time (FILETIME, 100 ns units); earlier than the start counts as the start.
    fn at(&self, ft: i64) -> u64 {
        ((ft - self.base).max(0) / 10_000) as u64
    }
    fn now(&self) -> u64 {
        self.at(filetime_now())
    }
}

/// Events seen by the callback (diagnostics, HAYATE_TIMING and tests).
pub static EVENTS: AtomicU64 = AtomicU64::new(0);

struct Props {
    buf: Vec<u64>,
}

/// Room after the properties for the logger and log-file names (1024 chars each).
const NAMES_BYTES: usize = 4096;

impl Props {
    fn new() -> Props {
        let base = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let total = base + NAMES_BYTES;
        let mut buf = vec![0u64; total / 8 + 1];
        let p = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        unsafe {
            (*p).Wnode.BufferSize = total as u32;
            (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*p).Wnode.ClientContext = 1;
            (*p).LoggerNameOffset = base as u32;
            (*p).LogFileNameOffset = (base + NAMES_BYTES / 2) as u32;
        }
        Props { buf }
    }
    fn props(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
        self.buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES
    }
    /// Logger name the system wrote after a query.
    fn logger_name(&self) -> String {
        let p = self.buf.as_ptr() as *const u8;
        let off = unsafe { (*(p as *const EVENT_TRACE_PROPERTIES)).LoggerNameOffset } as usize;
        let max = NAMES_BYTES / 4;
        let w = unsafe { std::slice::from_raw_parts(p.add(off) as *const u16, max) };
        String::from_utf16_lossy(&w[..w.iter().position(|c| *c == 0).unwrap_or(max)])
    }
}

/// Names of all running trace sessions (at most 64 exist).
fn all_session_names() -> Vec<String> {
    let mut bufs: Vec<Props> = (0..64).map(|_| Props::new()).collect();
    let mut ptrs: Vec<*mut EVENT_TRACE_PROPERTIES> = bufs.iter_mut().map(|b| b.props()).collect();
    let mut n = 0u32;
    let r = unsafe { QueryAllTracesW(&mut ptrs, &mut n) };
    if r != ERROR_SUCCESS {
        return Vec::new();
    }
    bufs.iter().take(n as usize).map(|b| b.logger_name()).collect()
}

/// Stop leftover sessions of Hayates that no longer exist (crash, kill). Never a
/// session whose owner is alive or unknown: the owner is the creation-time-checked
/// process in the name, never a guess.
fn stop_orphans() {
    let names = all_session_names();
    let gone = |pid: u32, create: i64| matches!(crate::osutil::open_verified(pid, create, windows::Win32::System::Threading::PROCESS_ACCESS_RIGHTS(0)), Err(crate::osutil::OpenFail::Gone));
    for n in orphans(&names, &gone) {
        let mut p = Props::new();
        let w = wide(&n);
        let r = unsafe { ControlTraceW(CONTROLTRACE_HANDLE { Value: 0 }, PCWSTR(w.as_ptr()), p.props(), EVENT_TRACE_CONTROL_STOP) };
        crate::osutil::log(&format!("netetw: stopped orphan session {n} ({})", r.0));
    }
}

fn next_name() -> String {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let create = crate::osutil::create_time_of(unsafe { windows::Win32::System::Threading::GetCurrentProcess() }).unwrap_or(0);
    session_name(std::process::id(), create, SEQ.fetch_add(1, Ordering::Relaxed))
}

struct Session {
    name: Vec<u16>,
    /// our own controller handle: everything we do to the session goes through it
    ctl: u64,
    /// set by the ProcessTrace thread when it returns
    done: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

/// The real session. One per process.
#[derive(Default)]
pub struct EtwBackend {
    sess: Option<Session>,
}

impl EtwBackend {
    fn query(&self) -> Option<u64> {
        let s = self.sess.as_ref()?;
        let mut p = Props::new();
        let r = unsafe { ControlTraceW(CONTROLTRACE_HANDLE { Value: s.ctl }, PCWSTR(s.name.as_ptr()), p.props(), EVENT_TRACE_CONTROL_QUERY) };
        if r != ERROR_SUCCESS {
            return None;
        }
        let pp = unsafe { &*p.props() };
        Some(pp.EventsLost as u64 + pp.RealTimeBuffersLost as u64 + pp.LogBuffersLost as u64)
    }
}

pub fn map_err(r: WIN32_ERROR, what: &str) -> StartErr {
    match r.0 {
        5 => StartErr::NeedsAdmin,
        // ERROR_ALREADY_EXISTS: that session name is taken. A clash, not a rights problem:
        // the next attempt uses the next name, and a session of someone else stays untouched.
        183 => StartErr::Other(format!("{what}: trace session name already in use (183)")),
        n => StartErr::Other(format!("{what} failed ({n})")),
    }
}

impl Backend for EtwBackend {
    fn start(&mut self) -> Result<(), StartErr> {
        unsafe {
            if self.sess.is_some() {
                return Ok(());
            }
            stop_orphans();
            let name = wide(&next_name());
            let mut p = Props::new();
            let pp = p.props();
            (*pp).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
            (*pp).BufferSize = 64; // KB
            (*pp).MinimumBuffers = 4;
            (*pp).MaximumBuffers = 16; // bounded: 1 MB worst case
            (*pp).FlushTimer = 1;
            let mut h = CONTROLTRACE_HANDLE::default();
            let r = StartTraceW(&mut h, PCWSTR(name.as_ptr()), pp);
            if r != ERROR_SUCCESS {
                return Err(map_err(r, "StartTrace"));
            }
            // from here on the session is ours: every failure stops it through `h`
            let stop_own = |h: CONTROLTRACE_HANDLE| {
                let mut q = Props::new();
                let _ = ControlTraceW(h, PCWSTR::null(), q.props(), EVENT_TRACE_CONTROL_STOP);
            };

            // kernel-side filter: only the eight data-transfer events and the nine connection
            // events (`netconn::CONN_EVENTS`) reach the session
            // EVENT_FILTER_EVENT_ID: FilterIn u8, Reserved u8, Count u16, Events[Count] u16
            let mut raw = vec![1u8, 0];
            raw.extend(((DATA_EVENTS.len() + crate::netconn::CONN_EVENTS.len()) as u16).to_le_bytes());
            for id in DATA_EVENTS.into_iter().chain(crate::netconn::CONN_EVENTS) {
                raw.extend(id.to_le_bytes());
            }
            let mut desc = EVENT_FILTER_DESCRIPTOR { Ptr: raw.as_ptr() as u64, Size: raw.len() as u32, Type: EVENT_FILTER_TYPE_EVENT_ID };
            let mut params = ENABLE_TRACE_PARAMETERS { Version: ENABLE_TRACE_PARAMETERS_VERSION_2, ..Default::default() };
            params.EnableFilterDesc = &mut desc;
            params.FilterDescCount = 1;
            let mut r = EnableTraceEx2(h, &PROVIDER, EVENT_CONTROL_CODE_ENABLE_PROVIDER.0, 4, KEYWORDS, 0, 0, Some(&params));
            if r != ERROR_SUCCESS && r != ERROR_ACCESS_DENIED {
                // no event-ID filtering here: take the whole provider and filter in the callback
                r = EnableTraceEx2(h, &PROVIDER, EVENT_CONTROL_CODE_ENABLE_PROVIDER.0, 4, KEYWORDS, 0, 0, None);
            }
            if r != ERROR_SUCCESS {
                stop_own(h);
                return Err(map_err(r, "EnableTraceEx2"));
            }

            let shared = Arc::new(Shared { acc: Mutex::new(Acc::default()), conn: Mutex::new(crate::netconn::Tracker::default()), t0: QpcClock::new() });
            let ctx = Arc::into_raw(shared.clone()) as usize;
            let mut lname = name.clone();
            let mut lf = EVENT_TRACE_LOGFILEW::default();
            lf.LoggerName = PWSTR(lname.as_mut_ptr());
            lf.Context = ctx as *mut std::ffi::c_void;
            lf.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
            lf.Anonymous2.EventRecordCallback = Some(on_event);
            let th = OpenTraceW(&mut lf);
            if th.Value == u64::MAX || th.Value == 0 {
                let e = GetLastError();
                drop(Arc::from_raw(ctx as *const Shared));
                stop_own(h);
                return Err(StartErr::Other(format!("OpenTrace failed ({})", e.0)));
            }
            let done = Arc::new(AtomicBool::new(false));
            let d2 = done.clone();
            let thv = th.Value;
            match std::thread::Builder::new().name("hayate-netetw".into()).stack_size(128 * 1024).spawn(move || {
                let _keep = lname;
                let th = PROCESSTRACE_HANDLE { Value: thv };
                let _ = ProcessTrace(&[th], None, None);
                let _ = CloseTrace(th);
                // the callback cannot run any more: release the session's accumulator reference
                drop(Arc::from_raw(ctx as *const Shared));
                d2.store(true, Ordering::SeqCst);
            }) {
                Ok(_detached) => {
                    self.sess = Some(Session { name, ctl: h.Value, done, shared });
                    Ok(())
                }
                Err(e) => {
                    let _ = CloseTrace(PROCESSTRACE_HANDLE { Value: thv });
                    drop(Arc::from_raw(ctx as *const Shared));
                    stop_own(h);
                    Err(StartErr::Other(e.to_string()))
                }
            }
        }
    }

    fn alive(&mut self) -> bool {
        self.sess.as_ref().is_some_and(|s| !s.done.load(Ordering::SeqCst))
    }

    /// Stops our own session through our own controller handle, never by name.
    /// Does not wait for the ProcessTrace thread: it ends by itself once the
    /// session is stopped, so the sampler thread (which calls this under the
    /// collector lock) never blocks on ETW's buffer flush.
    fn stop(&mut self) {
        if let Some(s) = self.sess.take() {
            let mut p = Props::new();
            unsafe {
                let _ = ControlTraceW(CONTROLTRACE_HANDLE { Value: s.ctl }, PCWSTR(s.name.as_ptr()), p.props(), EVENT_TRACE_CONTROL_STOP);
            }
        }
    }

    fn take(&mut self) -> Acc {
        match &self.sess {
            Some(s) => std::mem::take(&mut *s.shared.acc.lock().unwrap_or_else(|e| e.into_inner())),
            None => Acc::default(),
        }
    }

    fn lost(&mut self) -> u64 {
        self.query().unwrap_or(0)
    }

    fn conn(&mut self) -> crate::netconn::ConnFeed {
        match &self.sess {
            Some(s) => {
                let ms = s.shared.t0.now();
                s.shared.conn.lock().unwrap_or_else(|e| e.into_inner()).export(ms)
            }
            None => crate::netconn::ConnFeed::EMPTY,
        }
    }
}

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let r = &*rec;
    if r.EventHeader.ProviderId != PROVIDER || r.UserContext.is_null() {
        return;
    }
    let id = r.EventHeader.EventDescriptor.Id;
    let is_data = kind(id).is_some();
    if !is_data && !crate::netconn::is_conn(id) {
        return;
    }
    let sh = &*(r.UserContext as *const Shared);
    EVENTS.fetch_add(1, Ordering::Relaxed);
    let data: &[u8] = if r.UserData.is_null() { &[] } else { std::slice::from_raw_parts(r.UserData as *const u8, r.UserDataLength as usize) };
    if !is_data {
        if let Some(ev) = crate::netconn::decode_conn(id, r.EventHeader.EventDescriptor.Version, data) {
            let ms = sh.t0.at(r.EventHeader.TimeStamp);
            if let Ok(mut t) = sh.conn.lock() {
                t.event(&ev, ms);
            }
        }
        return;
    }
    match decode(id, r.EventHeader.EventDescriptor.Version, data) {
        Decoded::Traffic(t) => {
            if let Ok(mut a) = sh.acc.lock() {
                a.add(t);
            }
        }
        Decoded::Bad => {
            if let Ok(mut a) = sh.acc.lock() {
                a.bad += 1;
            }
        }
        Decoded::Skip => {}
    }
}

/// The process-wide collector, shared between the sampler thread (ticks) and shutdown.
pub fn global() -> &'static Mutex<Collector<EtwBackend>> {
    static C: OnceLock<Mutex<Collector<EtwBackend>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Collector::new(EtwBackend::default())))
}

/// Stop the session at exit.
pub fn shutdown() {
    if let Ok(mut c) = global().lock() {
        c.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(pid: u32, size: u32, d: &[u8], s: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(pid.to_le_bytes());
        v.extend(size.to_le_bytes());
        v.extend(d);
        v.extend(s);
        v.extend(443u16.to_be_bytes());
        v.extend(50000u16.to_be_bytes());
        v.extend([0u8; 24]); // trailing connection fields
        v
    }

    const D4: [u8; 4] = [93, 184, 216, 34];
    const S4: [u8; 4] = [192, 168, 1, 20];

    fn t(pid: u32, bytes: u32, dir: Dir, scope: Scope) -> Decoded {
        Decoded::Traffic(Traffic { pid, bytes, dir, scope })
    }

    fn pair(pid: u32) -> crate::netconn::ConnPair {
        crate::netconn::ConnPair { pid, create: 0, remote: "10.0.0.2".parse().unwrap(), port: 80, c60: Default::default(), c600: Default::default(), kind: None, tl: [[0; 2]; crate::netconn::WIN] }
    }

    #[test]
    fn a_recycled_pid_does_not_inherit_the_old_process_failures() {
        let mut bound = HashMap::new();
        let mut ps = vec![pair(7)];
        bind_pairs(&mut ps, &HashMap::from([(7, 100)]), &mut bound);
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].create, 100);
        // same pid, same process: still kept with its original identity
        bind_pairs(&mut ps, &HashMap::from([(7, 100)]), &mut bound);
        assert_eq!(ps.len(), 1);
        // pid 7 now names another process (create 200): the old pair is dropped, never relabelled
        let mut ps = vec![pair(7)];
        bind_pairs(&mut ps, &HashMap::from([(7, 200)]), &mut bound);
        assert!(ps.is_empty(), "recycled pid drops the pair");
        // gone or unknown start time: dropped, and the binding is forgotten
        let mut ps = vec![pair(7), pair(8)];
        bind_pairs(&mut ps, &HashMap::from([(8, 0)]), &mut bound);
        assert!(ps.is_empty());
        assert!(bound.is_empty());
    }

    #[test]
    fn decode_all_eight_events_direction_from_id() {
        let v4 = ev(7, 1448, &D4, &S4);
        let mut a6 = [0u8; 16];
        a6[0] = 0x26;
        a6[1] = 0x06;
        a6[15] = 9;
        let mut b6 = a6;
        b6[15] = 10;
        let v6 = ev(7, 1448, &a6, &b6);
        // payload order is daddr = D4 (public), saddr = S4 (private). A send goes to D4; a UDP
        // receive came from S4; a TCP receive still names its peer in daddr (D4)
        for (id, dir, scope) in [(10, Dir::Up, Scope::Wan), (11, Dir::Down, Scope::Wan), (42, Dir::Up, Scope::Wan), (43, Dir::Down, Scope::Lan)] {
            assert_eq!(decode(id, 0, &v4), t(7, 1448, dir, scope), "id {id}");
        }
        for (id, dir) in [(26, Dir::Up), (27, Dir::Down), (58, Dir::Up), (59, Dir::Down)] {
            assert_eq!(decode(id, 0, &v6), t(7, 1448, dir, Scope::Wan), "id {id}");
        }
    }

    #[test]
    fn decode_direction_ignores_addresses() {
        // same payload with swapped addresses: direction must not change
        let a = ev(1, 100, &D4, &S4);
        let b = ev(1, 100, &S4, &D4);
        let (Decoded::Traffic(x), Decoded::Traffic(y)) = (decode(11, 0, &a), decode(11, 0, &b)) else { panic!() };
        assert_eq!((x.dir, x.bytes), (y.dir, y.bytes), "direction and size never depend on the addresses");
        assert_eq!(decode(10, 0, &a), t(1, 100, Dir::Up, Scope::Wan));
    }

    #[test]
    fn decode_skips_non_data_events_and_double_counters() {
        let v4 = ev(7, 1448, &D4, &S4);
        // 12 attempt, 13 disconnect, 14 retransmit, 15 connect, 16 accept, 17 reconnect, 18 TCPCopy;
        // 30 retransmit v6, 34 TCPCopy v6
        for id in [12u16, 13, 14, 15, 16, 17, 18, 30, 34, 0, 1, 44, 60] {
            assert_eq!(decode(id, 0, &v4), Decoded::Skip, "id {id}");
        }
    }

    #[test]
    fn decode_skips_same_host_traffic() {
        assert_eq!(decode(10, 0, &ev(1, 5, &[127, 0, 0, 1], &[127, 0, 0, 1])), Decoded::Skip);
        assert_eq!(decode(11, 0, &ev(1, 5, &[127, 0, 0, 1], &[127, 5, 5, 5])), Decoded::Skip);
        // to our own LAN address: never leaves the host
        assert_eq!(decode(11, 0, &ev(1, 5, &S4, &S4)), Decoded::Skip);
        // loopback to a non-loopback address is a real flow
        assert_eq!(decode(11, 0, &ev(1, 5, &[10, 0, 0, 1], &[127, 0, 0, 1])), t(1, 5, Dir::Down, Scope::Lan));
        assert_eq!(decode(43, 0, &ev(1, 5, &[10, 0, 0, 1], &[127, 0, 0, 1])), t(1, 5, Dir::Down, Scope::Other));
        let mut l6 = [0u8; 16];
        l6[15] = 1;
        assert_eq!(decode(26, 0, &ev(1, 5, &l6, &l6)), Decoded::Skip);
        let mut mapped = [0u8; 16];
        mapped[10] = 0xff;
        mapped[11] = 0xff;
        mapped[12] = 127;
        mapped[15] = 1;
        assert_eq!(decode(26, 0, &ev(1, 5, &mapped, &l6)), Decoded::Skip);
    }

    #[test]
    fn decode_rejects_unknown_layouts() {
        let v4 = ev(7, 1448, &D4, &S4);
        assert_eq!(decode(10, 1, &v4), Decoded::Bad, "future version");
        assert_eq!(decode(10, 0, &v4[..19]), Decoded::Bad, "truncated before sport");
        assert_eq!(decode(10, 0, &v4[..20]), t(7, 1448, Dir::Up, Scope::Wan), "exactly the six leading fields");
        assert_eq!(decode(26, 0, &v4[..20]), Decoded::Bad, "v6 event needs 16-byte addresses");
        assert_eq!(decode(10, 0, &[]), Decoded::Bad);
    }

    #[test]
    fn accumulator_is_bounded_and_saturates() {
        let mut a = Acc::default();
        for pid in 0..MAX_PIDS as u32 + 10 {
            a.add(Traffic { pid, bytes: 10, dir: Dir::Down, scope: Scope::Other });
        }
        assert_eq!(a.map.len(), MAX_PIDS);
        assert_eq!(a.overflow, 100);
        // a known pid still accumulates once full
        a.add(Traffic { pid: 3, bytes: 5, dir: Dir::Up, scope: Scope::Other });
        assert_eq!(a.map[&3], Bytes { down: 10, up: 5, ..Default::default() });
        a.map.insert(1, Bytes { down: u64::MAX - 1, ..Default::default() });
        a.add(Traffic { pid: 1, bytes: 100, dir: Dir::Down, scope: Scope::Other });
        assert_eq!(a.map[&1].down, u64::MAX);
    }

    fn s4(a: u8, b: u8, c: u8, d: u8) -> Scope {
        classify(&[a, b, c, d])
    }
    fn s6(text: &str) -> Scope {
        classify(&text.parse::<std::net::Ipv6Addr>().unwrap().octets())
    }

    #[test]
    fn ipv4_ranges_follow_the_iana_registry() {
        use Scope::*;
        for (a, want) in [
            // RFC 1918 and its edges
            ((10, 0, 0, 1), Lan),
            ((10, 255, 255, 255), Lan),
            ((172, 15, 255, 255), Wan),
            ((172, 16, 0, 1), Lan),
            ((172, 31, 255, 255), Lan),
            ((172, 32, 0, 1), Wan),
            ((192, 168, 50, 3), Lan),
            ((192, 169, 0, 1), Wan),
            // link-local, link-scope multicast, limited broadcast
            ((169, 254, 3, 4), Lan),
            ((224, 0, 0, 251), Other),
            ((255, 255, 255, 255), Lan),
            // wider multicast, reserved, unspecified, loopback with a remote peer
            ((224, 0, 1, 1), Other),
            ((239, 255, 255, 250), Other),
            ((240, 0, 0, 1), Other),
            ((0, 0, 0, 0), Other),
            ((0, 1, 2, 3), Other),
            ((127, 0, 0, 1), Other),
            // shared address space (CGNAT, tunnels) is neither LAN nor public WAN
            ((100, 63, 255, 255), Wan),
            ((100, 64, 0, 1), Other),
            ((100, 127, 255, 255), Other),
            ((100, 128, 0, 1), Wan),
            // documentation, benchmarking, protocol assignments, 6to4 relay
            ((192, 0, 2, 9), Other),
            ((198, 51, 100, 9), Other),
            ((203, 0, 113, 9), Other),
            ((198, 17, 255, 255), Wan),
            ((198, 18, 0, 1), Other),
            ((198, 19, 255, 255), Other),
            ((198, 20, 0, 1), Wan),
            ((192, 0, 0, 8), Other),
            ((192, 0, 0, 9), Wan),
            ((192, 0, 0, 10), Wan),
            ((192, 0, 0, 11), Other),
            ((192, 88, 99, 1), Other),
            // ordinary public addresses, and a globally reachable special block
            ((1, 1, 1, 1), Wan),
            ((8, 8, 8, 8), Wan),
            ((93, 184, 216, 34), Wan),
            ((223, 255, 255, 255), Wan),
            ((192, 31, 196, 1), Wan),
        ] {
            assert_eq!(s4(a.0, a.1, a.2, a.3), want, "{a:?}");
        }
    }

    #[test]
    fn ipv6_ranges_follow_the_iana_registry() {
        use Scope::*;
        for (a, want) in [
            ("fe80::1", Lan),
            ("febf::1", Lan),
            ("fec0::1", Other),
            ("fc00::1", Lan),
            ("fd12:3456:789a::1", Lan),
            ("ff02::fb", Other),
            ("ff01::1", Other),
            ("ff05::1", Other),
            ("ff0e::1", Other),
            ("::", Other),
            ("::1", Other),
            ("::10.0.0.1", Other),
            ("100::1", Other),
            ("4000::1", Other),
            ("2606:4700::1111", Wan),
            ("2001:4860:4860::8888", Wan),
            ("2a00:1450:4001::200e", Wan),
            // protocol assignments / Teredo, documentation, 6to4
            ("2001::1", Other),
            ("2001:1::1", Wan),
            ("2001:1::2", Wan),
            ("2001:1::3", Wan),
            ("2001:1::4", Other),
            ("2001:3::1", Wan),
            ("2001:4:112::1", Wan),
            ("2001:4:113::1", Other),
            ("2001:20::1", Wan),
            ("2001:2f:ffff::1", Wan),
            ("2001:30::1", Wan),
            ("2001:40::1", Other),
            ("2001:db8::1", Other),
            ("2002:c000:204::1", Other),
            ("3fff::1", Other),
            ("3fff:fff::1", Other),
            ("3fff:1000::1", Wan),
            // NAT64: the IPv4 server behind it decides
            ("64:ff9b::808:808", Wan),
            ("64:ff9b::a00:1", Other),
            ("64:ff9b:1::1", Other),
        ] {
            assert_eq!(s6(a), want, "{a}");
        }
    }

    #[test]
    fn ipv4_mapped_addresses_use_the_embedded_ipv4_scope() {
        use Scope::*;
        for (a, want) in [
            ("::ffff:10.1.2.3", Lan),
            ("::ffff:192.168.0.9", Lan),
            ("::ffff:169.254.1.1", Lan),
            ("::ffff:8.8.8.8", Wan),
            ("::ffff:100.64.0.1", Other),
            ("::ffff:127.0.0.1", Other),
            ("::ffff:255.255.255.255", Lan),
        ] {
            assert_eq!(s6(a), want, "{a}");
        }
        // a 4-byte slice and the same address in mapped form agree
        assert_eq!(classify(&[10, 0, 0, 1]), s6("::ffff:10.0.0.1"));
        // not an address length: never public
        assert_eq!(classify(&[1, 2, 3]), Scope::Other);
        assert_eq!(classify(&[]), Scope::Other);
    }

    #[test]
    fn scope_follows_the_remote_end_of_each_direction() {
        let lan = [192, 168, 50, 3];
        let wan = [93, 184, 216, 34];
        // send (id 10 / 42): the destination is the remote; payload order is daddr, saddr
        for id in [10u16, 42] {
            assert_eq!(decode(id, 0, &ev(1, 7, &wan, &lan)), t(1, 7, Dir::Up, Scope::Wan), "id {id} to a public host");
            assert_eq!(decode(id, 0, &ev(1, 7, &[192, 168, 50, 9], &lan)), t(1, 7, Dir::Up, Scope::Lan), "id {id} to a LAN host");
        }
        // UDP receive (43): the packet's source (saddr) is the remote
        assert_eq!(decode(43, 0, &ev(1, 7, &lan, &wan)), t(1, 7, Dir::Down, Scope::Wan), "udp from a public host");
        assert_eq!(decode(43, 0, &ev(1, 7, &lan, &[192, 168, 50, 9])), t(1, 7, Dir::Down, Scope::Lan), "udp from a LAN host");
        // TCP receive (11): the peer is in daddr, as on a send
        assert_eq!(decode(11, 0, &ev(1, 7, &wan, &lan)), t(1, 7, Dir::Down, Scope::Wan), "tcp from a public host");
        assert_eq!(decode(11, 0, &ev(1, 7, &[192, 168, 50, 9], &lan)), t(1, 7, Dir::Down, Scope::Lan), "tcp from a LAN host");
        // IPv6 with the same rules, and a v4-mapped peer on a dual-stack socket
        let p6 = "2606:4700::1111".parse::<std::net::Ipv6Addr>().unwrap().octets();
        let l6 = "fe80::1".parse::<std::net::Ipv6Addr>().unwrap().octets();
        let m6 = "::ffff:10.0.0.7".parse::<std::net::Ipv6Addr>().unwrap().octets();
        assert_eq!(decode(26, 0, &ev(1, 7, &p6, &l6)), t(1, 7, Dir::Up, Scope::Wan));
        assert_eq!(decode(27, 0, &ev(1, 7, &p6, &l6)), t(1, 7, Dir::Down, Scope::Wan));
        assert_eq!(decode(58, 0, &ev(1, 7, &m6, &p6)), t(1, 7, Dir::Up, Scope::Lan));
        assert_eq!(decode(59, 0, &ev(1, 7, &p6, &m6)), t(1, 7, Dir::Down, Scope::Lan));
    }

    /// The four events of a live capture (elevated, rig 10.0.0.3 talking to its router
    /// 10.0.0.1 and to 1.1.1.1; payload order daddr, saddr), as the kernel delivered them.
    #[test]
    fn live_captured_roles_classify_by_the_peer() {
        let me = [192, 168, 50, 3];
        let router = [192, 168, 50, 1];
        let cf = [1, 1, 1, 1];
        // UDP: a send names the peer as daddr; a receive has the peer as saddr
        assert_eq!(decode(42, 0, &ev(1, 29, &router, &me)), t(1, 29, Dir::Up, Scope::Lan));
        assert_eq!(decode(43, 0, &ev(1, 61, &me, &router)), t(1, 61, Dir::Down, Scope::Lan));
        assert_eq!(decode(42, 0, &ev(1, 29, &cf, &me)), t(1, 29, Dir::Up, Scope::Wan));
        assert_eq!(decode(43, 0, &ev(1, 61, &me, &cf)), t(1, 61, Dir::Down, Scope::Wan));
        // TCP: the peer is daddr on both the send and the receive of the same connection
        assert_eq!(decode(10, 0, &ev(1, 53, &cf, &me)), t(1, 53, Dir::Up, Scope::Wan));
        assert_eq!(decode(11, 0, &ev(1, 214, &cf, &me)), t(1, 214, Dir::Down, Scope::Wan), "the reply from 1.1.1.1 is WAN, not LAN");
        assert_eq!(decode(11, 0, &ev(1, 269, &router, &me)), t(1, 269, Dir::Down, Scope::Lan));
    }

    #[test]
    fn scope_parts_always_add_up_to_the_total() {
        let mut a = Acc::default();
        let mut add = |pid, bytes, dir, scope| a.add(Traffic { pid, bytes, dir, scope });
        add(1, 1000, Dir::Down, Scope::Lan);
        add(1, 300, Dir::Down, Scope::Wan);
        add(1, 70, Dir::Down, Scope::Other);
        add(1, 50, Dir::Up, Scope::Wan);
        add(1, 9, Dir::Up, Scope::Other);
        add(2, 4000, Dir::Up, Scope::Lan);
        let b = a.map[&1];
        assert_eq!((b.down, b.lan_down, b.wan_down), (1370, 1000, 300));
        assert_eq!((b.up, b.lan_up, b.wan_up), (59, 0, 50));
        // rates over one second: bytes == rate
        let m = ids(&[(1, 10), (2, 20)]);
        let (rows, _) = attribute(&a.map, &m, &m, 1000);
        let r1 = *rows.iter().find(|r| r.pid == 1).unwrap();
        assert_eq!(r1.other(), (70, 9));
        let feed = Feed { status: Status::Live, rows: rows.clone(), lost: 0, unattributed: 0, bad: 0, iv: None, conn: Default::default() };
        let tot = feed.totals();
        assert_eq!(tot.all, (1370, 4059));
        assert_eq!(tot.lan, (1000, 4000));
        assert_eq!(tot.wan, (300, 50));
        assert_eq!(tot.other, (70, 9));
        assert_eq!((tot.lan.0 + tot.wan.0 + tot.other.0, tot.lan.1 + tot.wan.1 + tot.other.1), tot.all, "All is exactly the sum of its parts");
        // rounding of per-second rates never breaks the sum
        let (rows, _) = attribute(&a.map, &m, &m, 3000);
        for r in &rows {
            let o = r.other();
            assert_eq!((r.lan_down + r.wan_down + o.0, r.lan_up + r.wan_up + o.1), (r.down, r.up), "{r:?}");
        }
        assert_eq!(r1.of(View::Lan), (1000, 0));
        assert_eq!(r1.of(View::Wan), (300, 50));
        assert_eq!(r1.of(View::All), (1370, 59));
    }

    #[test]
    fn scope_rounding_never_credits_an_empty_category() {
        for ms in [1, 999, 1000, 1500, 3000, 10000] {
            for raw in [[1, 1, 0], [1, 0, 1], [0, 1, 1], [1, 2, 3]] {
                let (total, lan, wan) = scope_rates(raw.iter().sum(), raw[0], raw[1], ms);
                let parts = [lan, wan, total - lan - wan];
                assert_eq!(parts.iter().sum::<u64>(), total);
                for i in 0..3 { if raw[i] == 0 { assert_eq!(parts[i], 0); } }
            }
        }
        assert_eq!(scope_rates(2, 1, 1, 1500), (1, 1, 0));
    }

    #[test]
    fn a_reused_pid_keeps_no_scope_bytes() {
        let mut a = Acc::default();
        a.add(Traffic { pid: 5, bytes: 900, dir: Dir::Down, scope: Scope::Wan });
        let (rows, lost) = attribute(&a.map, &ids(&[(5, 100)]), &ids(&[(5, 200)]), 1000);
        assert!(rows.is_empty(), "{rows:?}");
        assert_eq!(lost, 900);
        assert_eq!(Feed { status: Status::Live, rows, lost: 0, unattributed: lost, bad: 0, iv: None, conn: Default::default() }.totals(), ScopeTotals::default());
    }

    fn ids(v: &[(u32, i64)]) -> HashMap<u32, i64> {
        v.iter().copied().collect()
    }
    fn bytes(v: &[(u32, u64, u64)]) -> HashMap<u32, Bytes> {
        v.iter().map(|&(p, d, u)| (p, Bytes { down: d, up: u, ..Default::default() })).collect()
    }

    #[test]
    fn rates_use_real_elapsed_time_and_sort_by_combined() {
        let m = ids(&[(1, 10), (2, 20), (3, 30)]);
        let (rows, lost) = attribute(&bytes(&[(1, 1000, 0), (2, 100, 400), (3, 0, 0)]), &m, &m, 2000);
        assert_eq!(lost, 0);
        assert_eq!(rows, vec![NetRow { pid: 1, create: 10, down: 500, up: 0, ..Default::default() }, NetRow { pid: 2, create: 20, down: 50, up: 200, ..Default::default() }], "idle pid 3 absent, 1000 B over 2 s = 500 B/s");
    }

    #[test]
    fn pid_reuse_never_inherits_traffic() {
        // pid 5 was process A (create 100) at the start and B (create 200) at the end
        let (rows, lost) = attribute(&bytes(&[(5, 900, 100)]), &ids(&[(5, 100)]), &ids(&[(5, 200)]), 1000);
        assert!(rows.is_empty());
        assert_eq!(lost, 1000, "counted as unattributed, not shown");
    }

    #[test]
    fn exited_and_new_processes() {
        // pid 6 exited during the interval
        let (rows, lost) = attribute(&bytes(&[(6, 50, 50)]), &ids(&[(6, 1)]), &ids(&[]), 1000);
        assert!(rows.is_empty());
        assert_eq!(lost, 100);
        // pid 7 started during the interval: nothing proves its bytes are not an earlier
        // process's that used the PID, so they are unattributed, not shown
        let (rows, lost) = attribute(&bytes(&[(7, 3000, 0)]), &ids(&[]), &ids(&[(7, 55)]), 1000);
        assert!(rows.is_empty());
        assert_eq!(lost, 3000);
    }

    #[test]
    fn a_replacement_missing_from_the_baseline_is_not_the_old_process() {
        // A (pid 5, create 100) exited and B (pid 5, create 200) started inside the same
        // interval: the start map has no pid 5 at all, the end map names B
        let (rows, lost) = attribute(&bytes(&[(5, 700, 300)]), &ids(&[]), &ids(&[(5, 200)]), 1000);
        assert!(rows.is_empty(), "B must not get A's traffic: {rows:?}");
        assert_eq!(lost, 1000);
        // the same process at both ends is attributed
        let (rows, _) = attribute(&bytes(&[(5, 700, 300)]), &ids(&[(5, 200)]), &ids(&[(5, 200)]), 1000);
        assert_eq!(rows, vec![NetRow { pid: 5, create: 200, down: 700, up: 300, ..Default::default() }]);
    }

    #[test]
    fn an_unknown_start_time_is_never_attributed() {
        let zero = ids(&[(5, 0)]);
        let (rows, lost) = attribute(&bytes(&[(5, 10, 10)]), &zero, &zero, 1000);
        assert!(rows.is_empty());
        assert_eq!(lost, 20);
    }

    #[test]
    fn rows_are_complete_up_to_the_pid_bound() {
        // the cut to what is shown happens after grouping (Apps), not here
        let m: HashMap<u32, i64> = (1..=300).map(|p| (p, p as i64)).collect();
        let b: HashMap<u32, Bytes> = (1..=300).map(|p| (p, Bytes { down: p as u64, ..Default::default() })).collect();
        let (rows, lost) = attribute(&b, &m, &m, 1000);
        assert_eq!(rows.len(), 300);
        assert_eq!(lost, 0);
        assert_eq!(rows[0].pid, 300);
        // and the accumulator holds at most MAX_PIDS of them
        let mut a = Acc::default();
        for pid in 1..=(MAX_PIDS as u32 + 50) {
            a.add(Traffic { pid, bytes: 1, dir: Dir::Down, scope: Scope::Other });
        }
        assert_eq!(a.map.len(), MAX_PIDS);
        assert_eq!(a.overflow, 50);
    }

    #[test]
    fn session_names_are_unique_per_instance_and_name_their_owner() {
        let a = session_name(100, 0xABC, 1);
        let b = session_name(100, 0xABC, 2);
        let other_process = session_name(101, 0xABC, 1);
        let reused_pid = session_name(100, 0xABD, 1);
        let all = [&a, &b, &other_process, &reused_pid];
        for (i, x) in all.iter().enumerate() {
            for y in &all[i + 1..] {
                assert_ne!(x, y);
            }
        }
        assert_eq!(session_owner(&a), Some((100, 0xABC)));
        assert_eq!(session_owner(&reused_pid), Some((100, 0xABD)));
        // the shared names of earlier builds name no owner: never touched
        assert_eq!(session_owner("Hayate-Kernel-Network"), None);
        assert_eq!(session_owner("Hayate-Kernel-Network-Multi"), None);
        assert_eq!(session_owner("Hayate-Kernel-Network-4-ff-1"), None, "system pids are not owners");
        assert_eq!(session_owner("Hayate-Kernel-Network-100-0-1"), None, "no create time, no identity");
        assert_eq!(session_owner("Hayate-Kernel-Network-100-ff-1-x"), None);
        assert_eq!(session_owner("Other-100-ff-1"), None);
        // a real name from this process
        let mine = next_name();
        assert_eq!(session_owner(&mine).map(|o| o.0), Some(std::process::id()));
        assert_ne!(mine, next_name(), "every start gets a fresh name");
        assert!(mine.len() < 100, "fits the logger-name buffer");
    }

    #[test]
    fn orphan_policy_never_stops_another_active_session() {
        let alive = session_name(100, 0xA, 1); // owner running
        let dead = session_name(200, 0xB, 1); // owner exited
        let reused = session_name(300, 0xC, 1); // pid 300 exists, but it is another process now
        let unknown = session_name(400, 0xD, 1); // owner cannot be checked
        let names: Vec<String> = [&alive, &dead, &reused, &unknown].iter().map(|s| s.to_string()).chain(["Hayate-Kernel-Network".into(), "Hayate-Kernel-Network-Multi".into(), "NT Kernel Logger".into(), "EventLog-System".into()]).collect();
        // `gone` is only true when the exact owner (pid AND create time) is provably not running
        let gone = |pid: u32, create: i64| matches!((pid, create), (200, 0xB) | (300, 0xC));
        assert_eq!(orphans(&names, &gone), vec![dead, reused]);
        // nothing is gone: nothing is stopped
        assert!(orphans(&names, &|_, _| false).is_empty());
    }

    #[test]
    fn a_name_clash_is_not_a_rights_problem() {
        assert!(matches!(map_err(WIN32_ERROR(5), "StartTrace"), StartErr::NeedsAdmin));
        match map_err(WIN32_ERROR(183), "StartTrace") {
            StartErr::Other(e) => assert!(e.contains("already in use") && e.contains("183"), "{e}"),
            StartErr::NeedsAdmin => panic!("183 must not look like missing admin rights"),
        }
        assert!(matches!(map_err(WIN32_ERROR(87), "StartTrace"), StartErr::Other(_)));
    }

    #[test]
    fn unreadable_events_are_counted_and_surface_in_the_feed() {
        let mut c = Collector::new(Fake::default());
        let m = ids(&[(1, 1)]);
        c.tick(0, true, &m);
        c.be.pending.bad = 3;
        let f = c.tick(1000, true, &m);
        assert_eq!((f.status.clone(), f.bad), (Status::Collecting, 3), "visible during the baseline");
        c.be.pending.bad = 2;
        let f = c.tick(2000, true, &m);
        assert_eq!(f.status, Status::Live);
        assert!(f.rows.is_empty());
        assert_eq!(f.bad, 5, "cumulative, so an empty list is not read as idle");
        // a new session starts counting afresh
        c.shutdown();
        c.tick(10_000, true, &m);
        assert_eq!(c.tick(11_000, true, &m).bad, 0);
    }

    // ---- state machine -----------------------------------------------------------

    #[derive(Default)]
    struct Fake {
        starts: Vec<Result<(), &'static str>>, // script: Ok, Err("admin"), Err(other)
        started: usize,
        stopped: usize,
        alive: bool,
        pending: Acc,
        lost: u64,
    }
    impl Backend for Fake {
        fn start(&mut self) -> Result<(), StartErr> {
            self.started += 1;
            let r = if self.starts.is_empty() { Ok(()) } else { self.starts.remove(0) };
            match r {
                Ok(()) => {
                    self.alive = true;
                    self.pending = Acc::default();
                    Ok(())
                }
                Err("admin") => Err(StartErr::NeedsAdmin),
                Err(e) => Err(StartErr::Other(e.into())),
            }
        }
        fn alive(&mut self) -> bool {
            self.alive
        }
        fn stop(&mut self) {
            self.stopped += 1;
            self.alive = false;
        }
        fn take(&mut self) -> Acc {
            std::mem::take(&mut self.pending)
        }
        fn lost(&mut self) -> u64 {
            self.lost
        }
    }

    fn feed_bytes(c: &mut Collector<Fake>, pid: u32, down: u64) {
        c.be.pending.add(Traffic { pid, bytes: down as u32, dir: Dir::Down, scope: Scope::Other });
    }

    #[test]
    fn needs_admin_is_reported_and_not_hammered() {
        let mut c = Collector::new(Fake { starts: vec![Err("admin"), Err("admin")], ..Default::default() });
        let m = ids(&[(1, 1)]);
        assert_eq!(c.tick(1000, false, &m).status, Status::Off, "nothing starts while hidden");
        assert_eq!(c.backend().started, 0);
        assert_eq!(c.tick(2000, true, &m).status, Status::NeedsAdmin);
        for t in 3..60 {
            assert_eq!(c.tick(t * 1000, true, &m).status, Status::NeedsAdmin);
        }
        assert_eq!(c.backend().started, 1, "no retry inside the admin retry window");
        assert_eq!(c.tick(2000 + ADMIN_RETRY_MS, true, &m).status, Status::NeedsAdmin);
        assert_eq!(c.backend().started, 2);
        // a failed start never produces rows
        assert!(c.tick(2000 + ADMIN_RETRY_MS + 1000, true, &m).rows.is_empty());
        assert!(!c.running());
    }

    #[test]
    fn first_interval_is_collecting_then_live() {
        let mut c = Collector::new(Fake::default());
        let m = ids(&[(1, 1)]);
        let f = c.tick(0, true, &m);
        assert_eq!(f.status, Status::Collecting);
        feed_bytes(&mut c, 1, 4000);
        let f = c.tick(1000, true, &m);
        assert_eq!(f.status, Status::Collecting, "partial first interval is dropped, not shown");
        assert!(f.rows.is_empty());
        feed_bytes(&mut c, 1, 2000);
        let f = c.tick(2000, true, &m);
        assert_eq!(f.status, Status::Live);
        assert_eq!(f.rows, vec![NetRow { pid: 1, create: 1, down: 2000, up: 0, ..Default::default() }]);
        // quiet second: live and genuinely empty
        let f = c.tick(3000, true, &m);
        assert_eq!((f.status, f.rows.len()), (Status::Live, 0));
    }

    #[test]
    fn scope_history_follows_the_collector_through_baseline_death_and_close() {
        use crate::netscope::ScopeHist;
        use crate::sample::HIST;
        let mut c = Collector::new(Fake::default());
        let mut h = ScopeHist::default();
        let m = ids(&[(1, 1)]);
        let at = |c: &mut Collector<Fake>, h: &mut ScopeHist, t: u64, want: bool| {
            let f = c.tick(t, want, &m);
            h.observe(t, &f);
            f
        };
        let tail = |h: &ScopeHist, n: usize| h.series().all.0[HIST - n..].to_vec();
        // baseline interval: no rows, no history
        assert_eq!(at(&mut c, &mut h, 0, true).iv, None);
        feed_bytes(&mut c, 1, 125_000);
        assert_eq!(at(&mut c, &mut h, 1000, true).iv, None, "baseline is not a measurement");
        assert!(tail(&h, 2).iter().all(|x| x.is_nan()));
        // live seconds; a tick inside the jitter window returns the cached feed and adds no slot
        feed_bytes(&mut c, 1, 250_000);
        let f2 = at(&mut c, &mut h, 2000, true);
        let iv = f2.iv.expect("live interval");
        assert_eq!((iv.end_ms, iv.span_ms), (2000, 1000));
        assert_eq!(at(&mut c, &mut h, 2100, true).iv, Some(iv), "cached copy, same id");
        feed_bytes(&mut c, 1, 375_000);
        let f3 = at(&mut c, &mut h, 3000, true);
        assert!(f3.iv.unwrap().id > iv.id);
        assert_eq!(&tail(&h, 3)[1..], &[2.0, 3.0]);
        // session killed: gap, never joined to what comes after
        c.be.alive = false;
        assert_eq!(at(&mut c, &mut h, 4000, true).iv, None);
        assert_eq!(at(&mut c, &mut h, 5000, true).iv, None);
        let after = at(&mut c, &mut h, 4000 + BACKOFF_MS[0], true);
        assert_eq!(after.status, Status::Collecting, "restart starts at a baseline");
        feed_bytes(&mut c, 1, 125_000);
        let f = at(&mut c, &mut h, 4000 + BACKOFF_MS[0] + 1000, true);
        assert_eq!(f.status, Status::Collecting);
        feed_bytes(&mut c, 1, 500_000);
        let f = at(&mut c, &mut h, 4000 + BACKOFF_MS[0] + 2000, true);
        assert!(f.iv.unwrap().id > f3.iv.unwrap().id, "interval ids never repeat across sessions");
        let t = tail(&h, 12);
        assert_eq!((t[2], t[3], t[11]), (2.0, 3.0, 4.0));
        assert!(t[..2].iter().chain(&t[4..11]).all(|x| x.is_nan()), "baselines, the dead seconds and the restart are gaps: {t:?}");
        // Network detail hidden past the grace period: the session stops and the history goes
        let hide = 4000 + BACKOFF_MS[0] + 2000;
        assert_eq!(at(&mut c, &mut h, hide + 1000, false).status, Status::Live, "still measured inside the grace period");
        assert_eq!(at(&mut c, &mut h, hide + 1000 + IDLE_STOP_MS, false).status, Status::Off);
        assert!(h.series().all.0.iter().all(|x| x.is_nan()));
    }

    #[test]
    fn session_death_restarts_with_backoff_and_fresh_baseline() {
        let mut c = Collector::new(Fake::default());
        let m = ids(&[(1, 1)]);
        c.tick(0, true, &m);
        c.tick(1000, true, &m);
        feed_bytes(&mut c, 1, 1000);
        assert_eq!(c.tick(2000, true, &m).status, Status::Live);
        c.be.alive = false; // logman stop
        let f = c.tick(3000, true, &m);
        assert!(matches!(f.status, Status::Unavailable(_)), "{:?}", f.status);
        assert!(f.rows.is_empty(), "no stale rows after a death");
        // inside the backoff: still unavailable, no start
        assert!(matches!(c.tick(5000, true, &m).status, Status::Unavailable(_)));
        assert_eq!(c.backend().started, 1);
        // after it: restart -> collecting, never inherited old rates
        let f = c.tick(4000 + BACKOFF_MS[0], true, &m);
        assert_eq!(f.status, Status::Collecting);
        assert_eq!(c.backend().started, 2);
        assert!(f.rows.is_empty());
    }

    #[test]
    fn repeated_failures_back_off_longer() {
        let mut c = Collector::new(Fake { starts: vec![Err("boom"), Err("boom"), Err("boom")], ..Default::default() });
        let m = ids(&[]);
        assert!(matches!(c.tick(0, true, &m).status, Status::Unavailable(_)));
        assert_eq!(c.tick(BACKOFF_MS[0] - 1, true, &m).status, Status::Unavailable("boom".into()));
        assert_eq!(c.backend().started, 1);
        c.tick(BACKOFF_MS[0], true, &m);
        assert_eq!(c.backend().started, 2);
        c.tick(BACKOFF_MS[0] + BACKOFF_MS[1] - 1, true, &m);
        assert_eq!(c.backend().started, 2);
        c.tick(BACKOFF_MS[0] + BACKOFF_MS[1], true, &m);
        assert_eq!(c.backend().started, 3);
        // third failure scheduled the longest wait so far
        c.tick(BACKOFF_MS[0] + BACKOFF_MS[1] + BACKOFF_MS[2] - 1, true, &m);
        assert_eq!(c.backend().started, 3);
        let t = BACKOFF_MS[0] + BACKOFF_MS[1] + BACKOFF_MS[2];
        assert_eq!(c.tick(t, true, &m).status, Status::Collecting, "script exhausted: starts");
    }

    #[test]
    fn session_stops_after_idle_grace_and_restart_is_clean() {
        let mut c = Collector::new(Fake::default());
        let m = ids(&[(1, 1)]);
        c.tick(0, true, &m);
        c.tick(1000, true, &m);
        // tab hidden: session stays for the grace window (flipping tabs is cheap)
        c.tick(2000, false, &m);
        c.tick(2000 + IDLE_STOP_MS - 1, false, &m);
        assert!(c.running());
        // back inside the grace: the idle clock resets
        c.tick(2000 + IDLE_STOP_MS, true, &m);
        assert!(c.running());
        c.tick(30_000, false, &m);
        let f = c.tick(30_000 + IDLE_STOP_MS, false, &m);
        assert!(!c.running());
        assert_eq!(c.backend().stopped, 1);
        assert_eq!(f, Feed::OFF);
        // reopen: new session, collecting again, no leftovers
        let f = c.tick(80_000, true, &m);
        assert_eq!(f.status, Status::Collecting);
        assert_eq!(c.backend().started, 2);
    }

    #[test]
    fn identity_and_jitter_and_stalls() {
        let mut c = Collector::new(Fake::default());
        let a = ids(&[(1, 10)]);
        c.tick(0, true, &a);
        c.tick(1000, true, &a);
        // jittery extra tick under MIN_GAP_MS: nothing drained
        feed_bytes(&mut c, 1, 500);
        c.tick(1100, true, &a);
        assert_eq!(c.be.pending.map[&1].down, 500, "not drained yet");
        // pid 1 replaced between ticks: its bytes vanish, not shown for the new process
        let b = ids(&[(1, 99)]);
        let f = c.tick(2000, true, &b);
        assert!(f.rows.is_empty());
        assert_eq!(f.unattributed, 500);
        // a 30 s stall (sleep/hibernate) makes a new baseline instead of a bogus average
        feed_bytes(&mut c, 1, 9000);
        let f = c.tick(32_000, true, &b);
        assert_eq!(f.status, Status::Collecting);
        assert!(f.rows.is_empty());
    }

    #[test]
    fn lost_events_surface_and_overflow_counts() {
        let mut c = Collector::new(Fake::default());
        let m = ids(&[(1, 1)]);
        c.tick(0, true, &m);
        c.tick(1000, true, &m);
        c.be.lost = 7;
        c.be.pending.overflow = 123;
        let f = c.tick(2000, true, &m);
        assert_eq!(f.lost, 7);
        assert_eq!(f.unattributed, 123);
    }

    #[test]
    fn shutdown_stops_the_session() {
        let mut c = Collector::new(Fake::default());
        c.tick(0, true, &ids(&[]));
        c.shutdown();
        assert_eq!(c.backend().stopped, 1);
        assert!(!c.running());
        c.shutdown();
        assert_eq!(c.backend().stopped, 1, "idempotent");
    }

    // ---- live (elevated) ----------------------------------------------------------

    /// From an ADMIN shell: `cargo test netetw_two_sessions --release -- --ignored --nocapture`.
    /// Two sessions of one process (two Hayates share the same code path) run side by side
    /// under different names; stopping one leaves the other delivering, and starting a
    /// third does not evict either.
    #[test]
    #[ignore]
    fn netetw_two_sessions() {
        let mut a = EtwBackend::default();
        let mut b = EtwBackend::default();
        for be in [&mut a, &mut b] {
            match be.start() {
                Ok(()) => {}
                Err(StartErr::NeedsAdmin) => panic!("needs admin: run from an elevated shell"),
                Err(StartErr::Other(e)) => panic!("{e}"),
            }
        }
        let names = all_session_names();
        let mine: Vec<_> = names.iter().filter(|n| session_owner(n).is_some_and(|o| o.0 == std::process::id())).collect();
        assert!(mine.len() >= 2, "both sessions listed under distinct names: {names:?}");
        assert!(a.alive() && b.alive());
        a.stop();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(b.alive(), "stopping A must not touch B");
        let mut c = EtwBackend::default();
        assert!(c.start().is_ok());
        assert!(b.alive() && c.alive(), "a third start evicts nobody");
        // orphan cleanup spares live owners
        stop_orphans();
        assert!(b.alive() && c.alive(), "sessions of a running owner are never orphans");
        b.stop();
        c.stop();
        let left: Vec<_> = all_session_names().into_iter().filter(|n| session_owner(n).is_some_and(|o| o.0 == std::process::id())).collect();
        assert!(left.is_empty(), "own sessions all torn down: {left:?}");
    }

    /// From an ADMIN shell: `cargo test netetw_live --release -- --ignored --nocapture`.
    /// Starts the real session, pulls bytes from a local HTTP-less TCP loop to a
    /// public address is out of scope here: it uses a UDP echo to 1.1.1.1:53 DNS
    /// query traffic plus a TCP connect-and-read, then checks that this process
    /// shows up with plausible bytes in both directions and that the session
    /// stops cleanly. Prints the event cost.
    #[test]
    #[ignore]
    fn netetw_live() {
        use std::io::{Read, Write};
        let mut be = EtwBackend::default();
        match be.start() {
            Ok(()) => {}
            Err(StartErr::NeedsAdmin) => panic!("needs admin: run from an elevated shell"),
            Err(StartErr::Other(e)) => panic!("{e}"),
        }
        let me = std::process::id();
        let t0 = std::time::Instant::now();
        // TCP: GET a small page over plain http (the exact byte counts are not asserted)
        let mut sent = 0usize;
        let mut got = 0usize;
        use std::net::ToSocketAddrs;
        let target = ["example.com:80", "neverssl.com:80"].iter().filter_map(|h| h.to_socket_addrs().ok()?.find(|a| a.is_ipv4())).next();
        eprintln!("workload target {target:?}");
        if let Some(Ok(mut s)) = target.map(|a| std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_secs(5))) {
            let req = b"GET / HTTP/1.0\r\nHost: example.com\r\nConnection: close\r\n\r\n";
            s.write_all(req).unwrap();
            sent += req.len();
            let mut buf = [0u8; 8192];
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got += n;
            }
        }
        // real-time buffers flush on the 1 s FlushTimer: give delivery time
        std::thread::sleep(std::time::Duration::from_millis(4000));
        let acc = be.take();
        let mut top: Vec<_> = acc.map.iter().map(|(p, b)| (*p, b.down, b.up)).collect();
        top.sort_by_key(|x| std::cmp::Reverse(x.1 + x.2));
        eprintln!("top pids (pid, down, up): {:?}", &top[..top.len().min(6)]);
        let mine = acc.map.get(&me).copied().unwrap_or_default();
        eprintln!("events {} bad {} lost {} elapsed {:?}", EVENTS.load(Ordering::Relaxed), acc.bad, be.lost(), t0.elapsed());
        eprintln!("app-level sent {sent} got {got}; etw for pid {me}: down {} up {}", mine.down, mine.up);
        be.stop();
        assert_eq!(acc.bad, 0, "payload layout matches decode");
        if got > 0 {
            assert!(mine.down as usize >= got * 9 / 10 && mine.down as usize <= got * 12 / 10, "down {} vs {}", mine.down, got);
            assert!(mine.up as usize >= sent && mine.up as usize <= sent + 200, "up {} vs {}", mine.up, sent);
        }
    }

    /// From an ADMIN shell: `cargo test netetw_scopes --release -- --ignored --nocapture`
    /// with `HAYATE_TEST_LAN=<ip of a LAN host that answers DNS on 53 and HTTP on 80>`
    /// (the router). Sends two small DNS queries over UDP and two tiny HTTP GETs over
    /// TCP, one pair to the LAN host and one to the public 1.1.1.1, and checks that
    /// this process's bytes land in the LAN / WAN buckets matching the app-level
    /// counts in BOTH directions (down = source is the remote, up = destination).
    /// Total traffic: under 2 KB. Needs no other host.
    #[test]
    #[ignore]
    fn netetw_scopes() {
        use std::io::{Read, Write};
        use std::net::{SocketAddr, TcpStream, UdpSocket};
        use std::time::Duration;
        let lan_ip = crate::osutil::env_var("TEST_LAN").expect("HAYATE_TEST_LAN=<LAN host ip>");
        let wan_ip = "1.1.1.1";
        assert_eq!(classify(&lan_ip.parse::<std::net::Ipv4Addr>().unwrap().octets()), Scope::Lan, "HAYATE_TEST_LAN must be private");
        let mut be = EtwBackend::default();
        match be.start() {
            Ok(()) => {}
            Err(StartErr::NeedsAdmin) => panic!("needs admin: run from an elevated shell"),
            Err(StartErr::Other(e)) => panic!("{e}"),
        }
        let me = std::process::id();
        // DNS query for example.com A
        let q: Vec<u8> = {
            let mut v = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
            for l in ["example", "com"] {
                v.push(l.len() as u8);
                v.extend(l.bytes());
            }
            v.extend([0, 0, 1, 0, 1]);
            v
        };
        let mut sent = [0usize; 2]; // [lan, wan]
        let mut got = [0usize; 2];
        for (i, ip) in [lan_ip.as_str(), wan_ip].into_iter().enumerate() {
            let u = UdpSocket::bind("0.0.0.0:0").unwrap();
            u.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            u.send_to(&q, (ip, 53)).unwrap();
            sent[i] += q.len();
            let mut b = [0u8; 512];
            if let Ok((n, _)) = u.recv_from(&mut b) {
                got[i] += n;
            }
            let a: SocketAddr = format!("{ip}:80").parse().unwrap();
            if let Ok(mut s) = TcpStream::connect_timeout(&a, Duration::from_secs(4)) {
                s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let req = format!("HEAD / HTTP/1.0\r\nHost: {ip}\r\nConnection: close\r\n\r\n");
                s.write_all(req.as_bytes()).unwrap();
                sent[i] += req.len();
                let mut buf = [0u8; 2048];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    got[i] += n;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(4000));
        let acc = be.take();
        let mine = acc.map.get(&me).copied().unwrap_or_default();
        be.stop();
        eprintln!("app-level sent [lan, wan] {sent:?} got {got:?}");
        eprintln!("etw {mine:?} bad {}", acc.bad);
        assert_eq!(acc.bad, 0);
        // own bytes: nothing leaks into the wrong bucket
        assert!(mine.lan_up as usize >= sent[0] && mine.lan_up as usize <= sent[0] + 300, "lan_up {} vs {}", mine.lan_up, sent[0]);
        assert!(mine.wan_up as usize >= sent[1] && mine.wan_up as usize <= sent[1] + 300, "wan_up {} vs {}", mine.wan_up, sent[1]);
        for k in 0..2 {
            let (down, name) = if k == 0 { (mine.lan_down, "lan_down") } else { (mine.wan_down, "wan_down") };
            assert!(down as usize >= got[k] * 9 / 10 && down as usize <= got[k] * 12 / 10 + 100, "{name} {down} vs {}", got[k]);
        }
        assert_eq!(mine.down, mine.lan_down + mine.wan_down, "this workload has no other-scope traffic");
        assert_eq!(mine.up, mine.lan_up + mine.wan_up);
    }

    /// From an ADMIN shell: `cargo test netetw_cost --release -- --ignored --nocapture`.
    /// Runs the real session for 30 s on whatever traffic the machine has and
    /// reports events/s and the CPU this process spent (decode + accumulate
    /// + drain once a second): an upper bound for the steady-state cost.
    #[test]
    #[ignore]
    fn netetw_cost() {
        use windows::Win32::System::Threading::GetCurrentProcess;
        #[link(name = "kernel32")]
        extern "system" {
            fn QueryProcessCycleTime(h: windows::Win32::Foundation::HANDLE, c: *mut u64) -> i32;
        }
        // CPU cycles of the whole process (tick-based process times cannot see ~50 us bursts a second)
        fn cycles() -> u64 {
            let mut c = 0u64;
            unsafe {
                QueryProcessCycleTime(GetCurrentProcess(), &mut c);
            }
            c
        }
        let mut be = EtwBackend::default();
        assert!(be.start().is_ok(), "needs an elevated shell");
        let (ev0, cpu0, t0) = (EVENTS.load(Ordering::Relaxed), cycles(), std::time::Instant::now());
        let mut maxpids = 0;
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            maxpids = maxpids.max(be.take().map.len());
        }
        let (ev, cpu, wall) = (EVENTS.load(Ordering::Relaxed) - ev0, cycles() - cpu0, t0.elapsed().as_secs_f64());
        let lost = be.lost();
        be.stop();
        eprintln!(
            "cost: {ev} events in {wall:.1}s = {:.0}/s; {cpu} cycles = {:.0} cycles/event, {:.0} Mcycles/s (~{:.4}% of one 4 GHz core); pids/interval max {maxpids}; lost {lost}",
            ev as f64 / wall,
            if ev > 0 { cpu as f64 / ev as f64 } else { 0.0 },
            cpu as f64 / wall / 1e6,
            cpu as f64 / wall / 4e9 * 100.0
        );
    }
}
