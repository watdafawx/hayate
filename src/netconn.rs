//! Who is failing on the network: per process + remote address connection health from the
//! Microsoft-Windows-Kernel-Network ETW events that are NOT data transfers.
//!
//! Event ids (confirmed live on Windows 11 build 10.0.26300 with an elevated capture, and
//! against the provider manifest `wevtutil gp Microsoft-Windows-Kernel-Network /ge /gm`;
//! IPv6 is the IPv4 id + 16):
//!
//! | v4 | v6 | meaning in practice |
//! |----|----|---------------------|
//! | 12 | 28 | connection attempted: fires for a connect that went ahead (also loopback); a refused or filtered connect never gets one |
//! | 13 | 29 | connection closed (any end of a connection; also the end of a failed connect) |
//! | 14 | 30 | data retransmitted (a segment of an established connection was resent) |
//! | 15 | 31 | "established": this is the ACCEPT (listener) side, NOT an outbound completion. Not consumed |
//! | 16 | 32 | reconnect attempted: a SYN retransmission. Refused connects emit about 3 of them 0.5 s apart, a silently dropped SYN one at about +1 s and +3 s |
//! | 17 | -  | connection attempt failed with an error code (never seen in captures; consumed, never interpreted) |
//!
//! All of them share the data-event payload prefix: `PID u32, size u32, daddr, saddr, dport u16 BE,
//! sport u16 BE, ...` (4- or 16-byte addresses). For TCP `daddr`/`dport` is always the peer.
//!
//! There is NO outbound "connect completed" event, so a failure is inferred from timing only:
//!
//! - No reply: SYN retransmissions spaced a second or more apart, or still pending after
//!   `PENDING_MS` with no attempt event and no close.
//! - Refused / reset: SYN retransmissions about half a second apart (or one, followed at once
//!   by a close) and a close, with no attempt event. The host answered, but not with an accept.
//! - Dropped mid-stream: an established connection that retransmitted data twice or more
//!   and then closed.
//!
//! These are inferences. They are never presented as a Windows error code.
//!
//! Memory is bounded: 600 one-second buckets per (pid, remote ip:port) pair, 64 pairs, the
//! rest folds into one "more" pair; at most `MAX_FLOWS` open flows.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One-second buckets kept per pair (10 minutes).
pub const RING: usize = 600;
/// Pairs tracked individually.
pub const MAX_PAIRS: usize = 64;
/// Seconds of per-second history exported for the graph marks.
pub const WIN: usize = 60;
const MAX_FLOWS: usize = 512;
/// A connect with no attempt event, no close and this much time since its first SYN retry is "no reply".
pub const PENDING_MS: u64 = 2500;
/// SYN retransmissions closer than this are the quick refused pattern.
pub const FAST_GAP_MS: u64 = 800;
/// One SYN retry followed by a close within this is refused (a filtered connect waits far longer).
pub const FAST_CLOSE_MS: u64 = 1200;
/// Real-time delivery lag allowed for before an open flow is judged on the clock.
const SWEEP_LAG_MS: u64 = 1500;
/// An idle open flow is forgotten after this.
const FLOW_TTL_MS: u64 = 60_000;

/// The connection events consumed (the kernel-side filter list), v4 then v6.
pub const CONN_EVENTS: [u16; 9] = [12, 13, 14, 16, 17, 28, 29, 30, 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ev {
    Attempt,
    Close,
    DataRetr,
    SynRetr,
    Fail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnEvent {
    pub pid: u32,
    pub ev: Ev,
    pub remote: IpAddr,
    pub rport: u16,
    pub lport: u16,
}

fn ev_of(id: u16) -> Option<(Ev, usize)> {
    Some(match id {
        12 => (Ev::Attempt, 4),
        13 => (Ev::Close, 4),
        14 => (Ev::DataRetr, 4),
        16 => (Ev::SynRetr, 4),
        17 => (Ev::Fail, 4),
        28 => (Ev::Attempt, 16),
        29 => (Ev::Close, 16),
        30 => (Ev::DataRetr, 16),
        32 => (Ev::SynRetr, 16),
        _ => return None,
    })
}

/// Whether `id` is one of the connection events.
pub fn is_conn(id: u16) -> bool {
    ev_of(id).is_some()
}

/// Decode one connection event's raw `UserData`. Loopback is kept (unlike data events): a
/// refused connect to a local port is a real failure. `None`: not ours or an unknown layout.
pub fn decode_conn(id: u16, version: u8, data: &[u8]) -> Option<ConnEvent> {
    let (ev, alen) = ev_of(id)?;
    let need = 8 + alen * 2 + 4;
    if version != 0 || data.len() < need {
        return None;
    }
    let pid = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let d = &data[8..8 + alen];
    let o = 8 + alen * 2;
    let rport = u16::from_be_bytes([data[o], data[o + 1]]);
    let lport = u16::from_be_bytes([data[o + 2], data[o + 3]]);
    let remote = if alen == 4 {
        IpAddr::V4(Ipv4Addr::new(d[0], d[1], d[2], d[3]))
    } else {
        let mut a = [0u8; 16];
        a.copy_from_slice(d);
        IpAddr::V6(Ipv6Addr::from(a)).to_canonical()
    };
    Some(ConnEvent { pid, ev, remote, rport, lport })
}

/// Inferred failure kind. Never a Windows error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    NoReply,
    Refused,
    Midstream,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::NoReply => "No reply",
            Kind::Refused => "Refused / reset",
            Kind::Midstream => "Dropped mid-stream",
        }
    }
}

/// What one flow showed; the input of the classifier (pure).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Obs {
    /// SYN retransmission events seen
    pub syn: u8,
    /// widest gap between consecutive SYN retransmissions (ms)
    pub max_syn_gap_ms: u64,
    /// an attempt event was seen: the connect went ahead
    pub established: bool,
    /// data retransmissions on the flow
    pub retr: u8,
    /// ms from the first SYN retransmission to the close (closed) or to now (open)
    pub age_ms: u64,
    pub closed: bool,
}

/// Failure kind of a flow, or `None` when it is clean (or not decided yet).
pub fn classify(o: &Obs) -> Option<Kind> {
    if o.syn > 0 && !o.established {
        if o.closed {
            let fast = if o.syn >= 2 { o.max_syn_gap_ms <= FAST_GAP_MS } else { o.age_ms <= FAST_CLOSE_MS };
            return Some(if fast { Kind::Refused } else { Kind::NoReply });
        }
        return (o.age_ms >= PENDING_MS).then_some(Kind::NoReply);
    }
    if o.closed && o.retr >= 2 {
        return Some(Kind::Midstream);
    }
    None
}

// ---- counts and rings ------------------------------------------------------------------

/// Event counts over a window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// connect attempts: those that went ahead plus the failed ones
    pub att: u32,
    pub noreply: u32,
    pub refused: u32,
    /// connections dropped mid-stream (after data retransmissions)
    pub mid: u32,
    /// data retransmissions
    pub retr: u32,
}

impl Counts {
    /// Failed connects (no reply + refused).
    pub fn failed(&self) -> u32 {
        self.noreply + self.refused
    }
    /// Ranking score: failed + resets + retransmits / 4.
    pub fn score(&self) -> u32 {
        self.failed() + self.mid + self.retr / 4
    }
    pub fn add(&mut self, o: &Counts) {
        self.att += o.att;
        self.noreply += o.noreply;
        self.refused += o.refused;
        self.mid += o.mid;
        self.retr += o.retr;
    }
    pub fn dominant(&self) -> Option<Kind> {
        let m = self.noreply.max(self.refused).max(self.mid);
        if m == 0 {
            None
        } else if self.noreply == m {
            Some(Kind::NoReply)
        } else if self.refused == m {
            Some(Kind::Refused)
        } else {
            Some(Kind::Midstream)
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    sec: u64,
    c: [u16; 5],
}

const A: usize = 0;
const NR: usize = 1;
const RF: usize = 2;
const MD: usize = 3;
const RT: usize = 4;

struct Pair {
    pid: u32,
    remote: IpAddr,
    port: u16,
    ring: Vec<Bucket>,
    last_sec: u64,
}

impl Pair {
    fn new(pid: u32, remote: IpAddr, port: u16) -> Pair {
        Pair { pid, remote, port, ring: vec![Bucket::default(); RING], last_sec: 0 }
    }
    fn bump(&mut self, sec: u64, slot: usize) {
        let b = &mut self.ring[(sec % RING as u64) as usize];
        if b.sec != sec {
            *b = Bucket { sec, c: [0; 5] };
        }
        b.c[slot] = b.c[slot].saturating_add(1);
        self.last_sec = sec;
    }
    /// Counts over the `secs` seconds ending at `now_sec` (inclusive).
    fn counts(&self, now_sec: u64, secs: u64) -> Counts {
        let mut c = Counts::default();
        for b in &self.ring {
            if b.sec != 0 && b.sec <= now_sec && now_sec - b.sec < secs {
                c.att += b.c[A] as u32;
                c.noreply += b.c[NR] as u32;
                c.refused += b.c[RF] as u32;
                c.mid += b.c[MD] as u32;
                c.retr += b.c[RT] as u32;
            }
        }
        c
    }
    /// Per-second (failed, retransmits) for the last `WIN` seconds, index `WIN - 1` = `now_sec`.
    fn timeline(&self, now_sec: u64) -> [[u8; 2]; WIN] {
        let mut t = [[0u8; 2]; WIN];
        for b in &self.ring {
            if b.sec != 0 && b.sec <= now_sec && now_sec - b.sec < WIN as u64 {
                let i = WIN - 1 - (now_sec - b.sec) as usize;
                t[i] = [(b.c[NR] + b.c[RF] + b.c[MD]).min(255) as u8, b.c[RT].min(255) as u8];
            }
        }
        t
    }
}

// ---- flow tracker ------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FlowKey {
    pid: u32,
    remote: IpAddr,
    rport: u16,
    lport: u16,
}

#[derive(Clone, Copy)]
struct Flow {
    first_syn: u64,
    last_syn: u64,
    last_ev: u64,
    obs: Obs,
    /// a failure was already counted for this flow
    decided: bool,
}

pub struct Tracker {
    flows: HashMap<FlowKey, Flow>,
    pairs: Vec<Pair>,
    more: Pair,
}

impl Default for Tracker {
    fn default() -> Self {
        Tracker { flows: HashMap::new(), pairs: Vec::new(), more: Pair::new(0, IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0) }
    }
}

fn sec(ms: u64) -> u64 {
    // seconds are 1-based: 0 marks an empty bucket
    ms / 1000 + 1
}

impl Tracker {
    fn pair(&mut self, pid: u32, remote: IpAddr, port: u16, now_sec: u64) -> &mut Pair {
        if let Some(i) = self.pairs.iter().position(|p| p.pid == pid && p.remote == remote && p.port == port) {
            return &mut self.pairs[i];
        }
        if self.pairs.len() >= MAX_PAIRS {
            // reuse the stalest pair once it has been silent for a whole ring
            let (i, last) = self.pairs.iter().enumerate().map(|(i, p)| (i, p.last_sec)).min_by_key(|x| x.1).unwrap();
            if now_sec.saturating_sub(last) < RING as u64 {
                return &mut self.more;
            }
            self.pairs[i] = Pair::new(pid, remote, port);
            return &mut self.pairs[i];
        }
        self.pairs.push(Pair::new(pid, remote, port));
        self.pairs.last_mut().unwrap()
    }

    fn record(&mut self, k: &FlowKey, kind: Kind, ms: u64) {
        let s = sec(ms);
        let slot = match kind {
            Kind::NoReply => NR,
            Kind::Refused => RF,
            Kind::Midstream => MD,
        };
        let p = self.pair(k.pid, k.remote, k.rport, s);
        p.bump(s, slot);
        if kind != Kind::Midstream {
            // a failed connect never gets an attempt event: count the attempt here
            p.bump(s, A);
        }
    }

    /// Feed one event at monotonic time `ms`.
    pub fn event(&mut self, e: &ConnEvent, ms: u64) {
        let k = FlowKey { pid: e.pid, remote: e.remote, rport: e.rport, lport: e.lport };
        let s = sec(ms);
        match e.ev {
            Ev::Attempt => {
                // healthy connects must not take a slot: only a pair that already failed counts them
                if let Some(p) = self.pairs.iter_mut().find(|p| p.pid == e.pid && p.remote == e.remote && p.port == e.rport) {
                    p.bump(s, A);
                }
                self.touch(k, ms).obs.established = true;
            }
            Ev::SynRetr => {
                let f = self.touch(k, ms);
                if f.obs.established {
                    return;
                }
                if f.obs.syn == 0 {
                    f.first_syn = ms;
                } else {
                    f.obs.max_syn_gap_ms = f.obs.max_syn_gap_ms.max(ms.saturating_sub(f.last_syn));
                }
                f.last_syn = ms;
                f.obs.syn = f.obs.syn.saturating_add(1);
            }
            Ev::DataRetr => {
                self.pair(e.pid, e.remote, e.rport, s).bump(s, RT);
                let f = self.touch(k, ms);
                f.obs.retr = f.obs.retr.saturating_add(1);
            }
            Ev::Close | Ev::Fail => {
                let Some(mut f) = self.flows.remove(&k) else {
                    if e.ev == Ev::Fail {
                        self.record(&k, Kind::NoReply, ms);
                    }
                    return;
                };
                if f.decided {
                    return;
                }
                f.obs.closed = true;
                f.obs.age_ms = ms.saturating_sub(f.first_syn);
                let kind = classify(&f.obs).or((e.ev == Ev::Fail).then_some(Kind::NoReply));
                if let Some(kind) = kind {
                    self.record(&k, kind, ms);
                }
            }
        }
    }

    fn touch(&mut self, k: FlowKey, ms: u64) -> &mut Flow {
        if self.flows.len() >= MAX_FLOWS && !self.flows.contains_key(&k) {
            self.flows.retain(|_, f| ms.saturating_sub(f.last_ev) < FLOW_TTL_MS / 4);
            if self.flows.len() >= MAX_FLOWS {
                // still full: forget the oldest
                if let Some(old) = self.flows.iter().min_by_key(|(_, f)| f.last_ev).map(|(k, _)| *k) {
                    self.flows.remove(&old);
                }
            }
        }
        let f = self.flows.entry(k).or_insert(Flow { first_syn: ms, last_syn: ms, last_ev: ms, obs: Obs::default(), decided: false });
        f.last_ev = ms;
        f
    }

    /// Decide open flows that waited long enough, forget idle ones.
    pub fn sweep(&mut self, ms: u64) {
        let mut hit = Vec::new();
        for (k, f) in self.flows.iter_mut() {
            if f.decided || f.obs.syn == 0 {
                continue;
            }
            let mut o = f.obs;
            o.age_ms = ms.saturating_sub(f.first_syn);
            if let Some(kind) = classify(&o) {
                f.decided = true;
                hit.push((*k, kind));
            }
        }
        for (k, kind) in hit {
            self.record(&k, kind, ms);
        }
        self.flows.retain(|_, f| ms.saturating_sub(f.last_ev) < FLOW_TTL_MS);
    }

    /// Export the offender view at time `ms`: pairs with any failure or retransmit in the last 10 min,
    /// worst first, plus the folded rest.
    pub fn export(&mut self, ms: u64) -> ConnFeed {
        // events are stamped when they happened but reach us up to ~1 s later: do not call a flow
        // silent while its close may still be on the way
        self.sweep(ms.saturating_sub(SWEEP_LAG_MS));
        let now = sec(ms);
        let mut pairs: Vec<ConnPair> = self
            .pairs
            .iter()
            .filter_map(|p| {
                let c600 = p.counts(now, RING as u64);
                if c600.failed() + c600.mid + c600.retr == 0 {
                    return None;
                }
                let c60 = p.counts(now, 60);
                Some(ConnPair { pid: p.pid, create: 0, remote: p.remote, port: p.port, c60, c600, kind: c60.dominant().or(c600.dominant()), tl: p.timeline(now) })
            })
            .collect();
        pairs.sort_by(|a, b| b.c60.score().cmp(&a.c60.score()).then(b.c600.score().cmp(&a.c600.score())).then(a.pid.cmp(&b.pid)));
        ConnFeed { pairs, more60: self.more.counts(now, 60), more_tl: self.more.timeline(now), end_sec: now }
    }
}

// ---- exported view -----------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnPair {
    pub pid: u32,
    /// process create time (filled by the collector from the process list; with `pid` the identity)
    pub create: i64,
    pub remote: IpAddr,
    pub port: u16,
    pub c60: Counts,
    pub c600: Counts,
    pub kind: Option<Kind>,
    /// per second over the last `WIN` s: [failed connects + drops, data retransmits]; last = now
    pub tl: [[u8; 2]; WIN],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnFeed {
    pub pairs: Vec<ConnPair>,
    /// pairs beyond `MAX_PAIRS`, folded
    pub more60: Counts,
    pub more_tl: [[u8; 2]; WIN],
    /// tracker clock second of `tl[WIN - 1]`
    pub end_sec: u64,
}

impl ConnFeed {
    pub const EMPTY: ConnFeed = ConnFeed { pairs: Vec::new(), more60: Counts { att: 0, noreply: 0, refused: 0, mid: 0, retr: 0 }, more_tl: [[0; 2]; WIN], end_sec: 0 };
}

impl Default for ConnFeed {
    fn default() -> Self {
        ConnFeed::EMPTY
    }
}

/// Noticeable offender threshold: failed + resets per minute.
pub const NOTICEABLE: u32 = 10;

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn e(pid: u32, ev: Ev, remote: IpAddr, rport: u16, lport: u16) -> ConnEvent {
        ConnEvent { pid, ev, remote, rport, lport }
    }

    fn payload(pid: u32, d: &[u8], s: &[u8], dport: u16, sport: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(pid.to_le_bytes());
        v.extend(0u32.to_le_bytes());
        v.extend(d);
        v.extend(s);
        v.extend(dport.to_be_bytes());
        v.extend(sport.to_be_bytes());
        v.extend([0u8; 20]);
        v
    }

    #[test]
    fn decode_v4_and_v6_keep_loopback() {
        let p = payload(77, &[127, 0, 0, 1], &[127, 0, 0, 1], 25565, 50000);
        let d = decode_conn(16, 0, &p).unwrap();
        assert_eq!(d, e(77, Ev::SynRetr, ip(127, 0, 0, 1), 25565, 50000));
        let mut a = [0u8; 16];
        a[0] = 0x20;
        a[1] = 0x01;
        a[15] = 9;
        let p6 = payload(5, &a, &[0u8; 16], 443, 4000);
        let d6 = decode_conn(28, 0, &p6).unwrap();
        assert_eq!(d6.ev, Ev::Attempt);
        assert_eq!(d6.remote, IpAddr::V6(Ipv6Addr::from(a)));
        assert_eq!((d6.rport, d6.lport), (443, 4000));
    }

    #[test]
    fn decode_rejects_other_ids_versions_and_short() {
        let p = payload(1, &[1, 2, 3, 4], &[5, 6, 7, 8], 1, 2);
        for id in [10u16, 11, 15, 18, 31, 34, 59] {
            assert!(decode_conn(id, 0, &p).is_none(), "{id}");
        }
        assert!(decode_conn(12, 1, &p).is_none());
        assert!(decode_conn(12, 0, &p[..15]).is_none());
        assert!(decode_conn(13, 0, &p).is_some());
    }

    #[test]
    fn v4_mapped_remote_is_canonical() {
        let mut a = [0u8; 16];
        a[10] = 0xff;
        a[11] = 0xff;
        a[12..].copy_from_slice(&[10, 0, 0, 2]);
        let p6 = payload(5, &a, &[0u8; 16], 80, 4000);
        assert_eq!(decode_conn(28, 0, &p6).unwrap().remote, ip(10, 0, 0, 2));
    }

    fn obs(syn: u8, gap: u64, est: bool, retr: u8, age: u64, closed: bool) -> Obs {
        Obs { syn, max_syn_gap_ms: gap, established: est, retr, age_ms: age, closed }
    }

    #[test]
    fn classify_table() {
        let t: &[(&str, Obs, Option<Kind>)] = &[
            ("refused: 3 quick SYN retries then close", obs(3, 500, false, 0, 1000, true), Some(Kind::Refused)),
            ("refused: one retry then quick close", obs(1, 0, false, 0, 800, true), Some(Kind::Refused)),
            ("filtered: slow retries then give up", obs(2, 2000, false, 0, 6000, true), Some(Kind::NoReply)),
            ("filtered: single retry, late close", obs(1, 0, false, 0, 5000, true), Some(Kind::NoReply)),
            ("pending long enough", obs(2, 2000, false, 0, 2600, false), Some(Kind::NoReply)),
            ("pending but young", obs(1, 0, false, 0, 1000, false), None),
            ("quick refused pattern not yet closed stays pending", obs(3, 500, false, 0, 1400, false), None),
            ("slow handshake that went ahead is clean", obs(1, 0, true, 0, 1500, true), None),
            ("mid-stream: 2 retransmits then close", obs(0, 0, true, 2, 0, true), Some(Kind::Midstream)),
            ("mid-stream without attempt event", obs(0, 0, false, 3, 0, true), Some(Kind::Midstream)),
            ("one retransmit is noise", obs(0, 0, true, 1, 0, true), None),
            ("retransmits but still open", obs(0, 0, true, 5, 0, false), None),
            ("clean close", obs(0, 0, true, 0, 0, true), None),
        ];
        for (name, o, want) in t {
            assert_eq!(classify(o), *want, "{name}");
        }
    }

    #[test]
    fn sequence_refused_connect() {
        // observed on test machine: 3 SYN retransmits 0.5 s apart, then the close, no attempt event
        let mut t = Tracker::default();
        let r = ip(10, 0, 0, 2);
        for (ms, ev) in [(1000, Ev::SynRetr), (1500, Ev::SynRetr), (2000, Ev::SynRetr), (2100, Ev::Close)] {
            t.event(&e(9, ev, r, 25565, 51000), ms);
        }
        let f = t.export(2200);
        assert_eq!(f.pairs.len(), 1);
        let p = &f.pairs[0];
        assert_eq!((p.pid, p.port, p.kind), (9, 25565, Some(Kind::Refused)));
        assert_eq!((p.c60.refused, p.c60.noreply, p.c60.att), (1, 0, 1));
    }

    #[test]
    fn sequence_no_reply_decided_by_sweep_then_close_not_double_counted() {
        let mut t = Tracker::default();
        let r = ip(203, 0, 113, 5);
        t.event(&e(9, Ev::SynRetr, r, 80, 51000), 1000);
        t.event(&e(9, Ev::SynRetr, r, 80, 51000), 3000);
        assert!(t.export(3100).pairs.is_empty(), "too young");
        assert!(t.export(4000).pairs.is_empty(), "the delivery lag is allowed for first");
        let f = t.export(3600 + SWEEP_LAG_MS);
        assert_eq!(f.pairs[0].c60.noreply, 1);
        // the app gives up 5 s later: already counted
        t.event(&e(9, Ev::Close, r, 80, 51000), 8000);
        let f = t.export(8100);
        assert_eq!((f.pairs[0].c60.noreply, f.pairs[0].c60.att), (1, 1));
    }

    #[test]
    fn sequence_success_and_midstream() {
        let mut t = Tracker::default();
        let r = ip(93, 184, 216, 34);
        // success: attempt then close: nothing to report
        t.event(&e(1, Ev::Attempt, r, 443, 50000), 100);
        t.event(&e(1, Ev::Close, r, 443, 50000), 900);
        assert!(t.export(1000).pairs.is_empty());
        // established connection retransmits and drops
        t.event(&e(1, Ev::Attempt, r, 443, 50001), 2000);
        t.event(&e(1, Ev::DataRetr, r, 443, 50001), 2100);
        t.event(&e(1, Ev::DataRetr, r, 443, 50001), 2600);
        t.event(&e(1, Ev::Close, r, 443, 50001), 3000);
        let f = t.export(3100);
        let p = &f.pairs[0];
        assert_eq!((p.c60.mid, p.c60.retr, p.c60.failed(), p.kind), (1, 2, 0, Some(Kind::Midstream)));
        assert_eq!(p.c60.att, 0, "attempts before the first failure do not allocate a pair");
        // once the pair exists, later attempts are counted
        t.event(&e(1, Ev::Attempt, r, 443, 50002), 3500);
        assert_eq!(t.export(3600).pairs[0].c60.att, 1);
    }

    #[test]
    fn healthy_connects_take_no_pair_slot() {
        let mut t = Tracker::default();
        for i in 0..(MAX_PAIRS as u32 * 2) {
            let r = ip(10, 0, (i / 250) as u8, (i % 250) as u8);
            t.event(&e(7, Ev::Attempt, r, 443, 50000), 1000);
            t.event(&e(7, Ev::Close, r, 443, 50000), 1100);
        }
        // a failing pair afterwards still gets its own row, not the folded "more"
        let r = ip(10, 0, 0, 2);
        t.event(&e(8, Ev::SynRetr, r, 25565, 40000), 2000);
        t.event(&e(8, Ev::Close, r, 25565, 40000), 2050);
        let f = t.export(3000);
        assert_eq!(f.pairs.len(), 1);
        assert_eq!(f.more60.failed(), 0);
    }

    #[test]
    fn loopback_connect_failure_counts() {
        let mut t = Tracker::default();
        let r = ip(127, 0, 0, 1);
        for i in 0..12u64 {
            let ms = 1000 + i * 100;
            t.event(&e(4, Ev::SynRetr, r, 1, 40000 + i as u16), ms);
            t.event(&e(4, Ev::Close, r, 1, 40000 + i as u16), ms + 40);
        }
        let f = t.export(3000);
        assert_eq!(f.pairs[0].c60.refused, 12);
        assert!(f.pairs[0].c60.failed() + f.pairs[0].c60.mid >= NOTICEABLE);
    }

    #[test]
    fn windows_and_timeline() {
        let mut t = Tracker::default();
        let r = ip(10, 0, 0, 9);
        // a failure at second 5 and one at second 70
        for ms in [5000u64, 70_000] {
            t.event(&e(2, Ev::SynRetr, r, 22, 50000), ms);
            t.event(&e(2, Ev::Close, r, 22, 50000), ms + 100);
        }
        let f = t.export(75_000);
        let p = &f.pairs[0];
        assert_eq!(p.c60.refused, 1, "only the recent one in 60 s");
        assert_eq!(p.c600.refused, 2);
        assert_eq!(p.tl[WIN - 1 - 5][0], 1, "5 s ago, one failed");
        assert_eq!(p.tl.iter().map(|x| x[0] as u32).sum::<u32>(), 1);
        // past the ring everything is gone
        let f = t.export(75_000 + 700_000);
        assert!(f.pairs.is_empty());
    }

    #[test]
    fn pair_cap_folds_into_more() {
        let mut t = Tracker::default();
        for i in 0..(MAX_PAIRS as u32 + 10) {
            let r = ip(10, 0, (i / 250) as u8, (i % 250) as u8);
            t.event(&e(100 + i, Ev::SynRetr, r, 80, 50000), 1000);
            t.event(&e(100 + i, Ev::Close, r, 80, 50000), 1100);
        }
        let f = t.export(2000);
        assert_eq!(f.pairs.len(), MAX_PAIRS);
        assert_eq!(f.more60.refused, 10);
    }

    #[test]
    fn rank_by_score() {
        let mut t = Tracker::default();
        let r = ip(10, 0, 0, 9);
        for i in 0..3 {
            t.event(&e(1, Ev::SynRetr, r, 80, 100 + i), 1000);
            t.event(&e(1, Ev::Close, r, 80, 100 + i), 1050);
        }
        for i in 0..8 {
            t.event(&e(2, Ev::SynRetr, r, 81, 200 + i), 1000);
            t.event(&e(2, Ev::Close, r, 81, 200 + i), 1050);
        }
        let f = t.export(2000);
        assert_eq!(f.pairs.iter().map(|p| p.pid).collect::<Vec<_>>(), vec![2, 1]);
    }

    #[test]
    fn flow_table_is_bounded() {
        let mut t = Tracker::default();
        let r = ip(10, 0, 0, 9);
        for i in 0..(MAX_FLOWS as u16 * 3) {
            t.event(&e(1, Ev::Attempt, r, 80, i), 1000 + i as u64);
        }
        assert!(t.flows.len() <= MAX_FLOWS);
    }
}
