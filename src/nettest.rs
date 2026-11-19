//! "Test this host": the button on a Who-is-failing row. One click, never automatic:
//!
//! - one ICMP echo (`IcmpSendEcho`, 1 s timeout, see `sample::probe::echo_once`), IPv4 only, never
//!   to a loopback / unspecified / multicast / broadcast address, and
//! - one TCP connect (non-blocking Winsock socket, waited on 1.5 s) to the row's IP:port, closed at
//!   once without sending a byte.
//!
//! The destination is only the address of the clicked row: a literal IP from the table as last drawn
//! (no hostnames, no name lookups), checked by `validate`. A row cools down for 5 s after a test,
//! and only one test runs at a time. The work runs on its own short-lived thread, never the UI
//! thread, and its result is read back by the next paint.
//!
//! Everything that decides something is pure and unit-tested here: `classify`, `tcp_kind`,
//! `parse_literal`, `validate` and the `Registry` gate.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// TCP connect timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
/// A row cannot be tested again for this long after its last test finished.
pub const COOLDOWN: Duration = Duration::from_secs(5);
/// Results kept for the hover card, then forgotten.
pub const KEEP: Duration = Duration::from_secs(600);
/// A slot still `Running` after this long is stuck (worker died or hung): it stops blocking tests.
pub const RUN_LIMIT: Duration = Duration::from_secs(10);
/// Rows remembered at most (the table shows 6).
const MAX_ENTRIES: usize = 16;

/// What the single echo said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echo {
    Reply,
    NoReply,
    /// not sent: IPv6, or an address an echo is never sent to
    NotSent,
}

/// What the single connect said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tcp {
    /// the handshake completed
    Open,
    /// the host answered with a reset: it is up and nothing listens there
    Refused,
    /// no answer in time, or no route
    Silent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    UpPortOpen,
    UpPortClosed,
    /// no echo reply and no TCP answer
    HostDown,
    /// the host (or something in front of it) did not answer the connect
    PortFiltered,
}

impl Outcome {
    /// The sentence shown in the hover card.
    pub fn text(self) -> &'static str {
        match self {
            Outcome::UpPortOpen => "host is up, port open",
            Outcome::UpPortClosed => "host is up, port closed",
            Outcome::HostDown => "host not answering (down, asleep or filtered)",
            Outcome::PortFiltered => "port filtered/no reply",
        }
    }
    /// The few words on the button after a test.
    pub fn short(self) -> &'static str {
        match self {
            Outcome::UpPortOpen => "up · open",
            Outcome::UpPortClosed => "up · closed",
            Outcome::HostDown => "no answer",
            Outcome::PortFiltered => "filtered",
        }
    }
}

/// Combine the two answers. A completed or refused connect proves the host is up whatever the
/// echo said (ICMP is often filtered); with neither answering, the echo tells a dead host from a
/// filtered port, and with no echo sent the connect alone cannot tell, so it reads as filtered.
pub fn classify(echo: Echo, tcp: Tcp) -> Outcome {
    match (tcp, echo) {
        (Tcp::Open, _) => Outcome::UpPortOpen,
        (Tcp::Refused, _) => Outcome::UpPortClosed,
        (Tcp::Silent, Echo::NoReply) => Outcome::HostDown,
        (Tcp::Silent, Echo::Reply | Echo::NotSent) => Outcome::PortFiltered,
    }
}

/// A failed connect (Winsock error code) as a `Tcp` answer: only a refusal / reset proves a live
/// host (10061 WSAECONNREFUSED, 10054 WSAECONNRESET, 10053 WSAECONNABORTED).
pub fn tcp_kind(code: i32) -> Tcp {
    match code {
        10061 | 10054 | 10053 => Tcp::Refused,
        _ => Tcp::Silent,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// not a literal IP address (a hostname, a scoped or malformed address)
    NotLiteral,
    /// unspecified, multicast, broadcast, reserved, or port 0
    NotUnicast,
    /// not one of the rows of the table
    NotInTable,
    /// that row was tested less than `COOLDOWN` ago
    Cooldown,
    /// a test is already running
    Busy,
}

/// A literal IPv4 / IPv6 address, nothing else: no name, no scope id, no port, no spaces.
pub fn parse_literal(s: &str) -> Option<IpAddr> {
    if s.is_empty() || s.len() > 45 || s.contains(['%', ' ', '/', '[', ']']) {
        return None;
    }
    s.parse::<IpAddr>().ok()
}

/// Whether an address is a unicast host a connect or echo may go to.
pub fn is_unicast(ip: IpAddr) -> bool {
    match ip {
        // 0.0.0.0/8 is "this network", never a host to reach (not just 0.0.0.0)
        IpAddr::V4(a) => !(a.is_unspecified() || a.is_multicast() || a.is_broadcast() || a.octets()[0] == 0 || a.octets()[0] >= 240),
        IpAddr::V6(a) => !(a.is_unspecified() || a.is_multicast()),
    }
}

/// An echo is sent only to a non-loopback unicast IPv4 host.
pub fn echo_allowed(ip: IpAddr) -> Option<Ipv4Addr> {
    match ip {
        IpAddr::V4(a) if is_unicast(ip) && !a.is_loopback() => Some(a),
        _ => None,
    }
}

/// The destination must be a unicast address and port of a row of `table` (the rows as drawn).
pub fn validate(ip: IpAddr, port: u16, table: &[(IpAddr, u16)]) -> Result<(), Reject> {
    if port == 0 || !is_unicast(ip) {
        return Err(Reject::NotUnicast);
    }
    if !table.contains(&(ip, port)) {
        return Err(Reject::NotInTable);
    }
    Ok(())
}

/// Same, from text: a literal IP is required first.
pub fn validate_text(ip: &str, port: u16, table: &[(IpAddr, u16)]) -> Result<IpAddr, Reject> {
    let ip = parse_literal(ip).ok_or(Reject::NotLiteral)?;
    validate(ip, port, table).map(|_| ip)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    Running { since: Instant },
    Done { at: Instant, outcome: Outcome },
}

/// Whether a test that started at `since` has run past `RUN_LIMIT` (stuck).
pub fn run_expired(since: Instant, now: Instant) -> bool {
    now.saturating_duration_since(since) >= RUN_LIMIT
}

/// Who may be tested now, and the last results. Pure: time comes in as an argument.
#[derive(Default)]
pub struct Registry {
    slots: Vec<((IpAddr, u16), Slot)>,
}

impl Registry {
    pub fn get(&self, key: (IpAddr, u16)) -> Option<Slot> {
        self.slots.iter().find(|(k, _)| *k == key).map(|(_, s)| *s)
    }

    /// Whether `key` may be tested at `now`: validated, no test running, row not cooling down.
    pub fn gate(&self, key: (IpAddr, u16), table: &[(IpAddr, u16)], now: Instant) -> Result<(), Reject> {
        validate(key.0, key.1, table)?;
        if self.slots.iter().any(|(_, s)| matches!(s, Slot::Running { since } if !run_expired(*since, now))) {
            return Err(Reject::Busy);
        }
        if let Some(Slot::Done { at, .. }) = self.get(key) {
            if now.saturating_duration_since(at) < COOLDOWN {
                return Err(Reject::Cooldown);
            }
        }
        Ok(())
    }

    /// Mark `key` running (after `gate` said yes).
    pub fn begin(&mut self, key: (IpAddr, u16), now: Instant) {
        self.slots.retain(|(k, s)| *k != key && !matches!(s, Slot::Done { at, .. } if now.saturating_duration_since(*at) > KEEP));
        while self.slots.len() >= MAX_ENTRIES {
            self.slots.remove(0);
        }
        self.slots.push((key, Slot::Running { since: now }));
    }

    pub fn finish(&mut self, key: (IpAddr, u16), outcome: Outcome, now: Instant) {
        if let Some((_, s)) = self.slots.iter_mut().find(|(k, _)| *k == key) {
            *s = Slot::Done { at: now, outcome };
        }
    }
}

static REG: Mutex<Option<Registry>> = Mutex::new(None);

fn reg<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let mut g = REG.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Registry::default))
}

/// The state of a row's test, for painting.
pub fn status(ip: IpAddr, port: u16) -> Option<Slot> {
    reg(|r| r.get((ip, port)))
}

/// Start the test of one table row on a worker thread: one echo and one connect, once. Called from
/// the click handler only. Returns why it did not start.
pub fn start(ip: IpAddr, port: u16, table: &[(IpAddr, u16)]) -> Result<(), Reject> {
    let key = (ip, port);
    let now = Instant::now();
    reg(|r| {
        r.gate(key, table, now)?;
        r.begin(key, now);
        Ok::<(), Reject>(())
    })?;
    let spawned = std::thread::Builder::new().name("hayate-net-test".into()).spawn(move || {
        let outcome = run(ip, port);
        reg(|r| r.finish(key, outcome, Instant::now()));
        crate::ui::icons::poke();
    });
    if spawned.is_err() {
        // no thread, no test: do not leave the row "running" forever
        reg(|r| r.finish(key, Outcome::PortFiltered, Instant::now()));
    }
    Ok(())
}

/// The two probes, in order, on the calling (worker) thread.
fn run(ip: IpAddr, port: u16) -> Outcome {
    let echo = match echo_allowed(ip) {
        Some(a) => {
            if crate::sample::probe::echo_once(a) {
                Echo::Reply
            } else {
                Echo::NoReply
            }
        }
        None => Echo::NotSent,
    };
    classify(echo, connect_once(ip, port))
}

/// One TCP connect to `ip:port`, answered within `CONNECT_TIMEOUT` or not. A plain Windows connect
/// to a closed port only reports the refusal after about 2 s (the stack retries the SYN after the
/// reset), longer than the timeout, so the socket is told not to retransmit the SYN
/// (`SIO_TCP_INITIAL_RTO`): the reset then ends the attempt at once. The socket is non-blocking,
/// waited on once with `select`, and closed without a byte sent.
fn connect_once(ip: IpAddr, port: u16) -> Tcp {
    use windows::Win32::Networking::WinSock::*;
    unsafe {
        let mut wsa = std::mem::zeroed::<WSADATA>();
        if WSAStartup(0x0202, &mut wsa) != 0 {
            return Tcp::Silent;
        }
        let tcp = connect_socket(ip, port);
        WSACleanup();
        tcp
    }
}

unsafe fn connect_socket(ip: IpAddr, port: u16) -> Tcp {
    use windows::Win32::Networking::WinSock::*;
    let family = if ip.is_ipv4() { AF_INET } else { AF_INET6 };
    let Ok(s) = socket(family.0 as i32, SOCK_STREAM, IPPROTO_TCP.0) else { return Tcp::Silent };
    let rto = TCP_INITIAL_RTO_PARAMETERS { Rtt: 0xFFFF, MaxSynRetransmissions: TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS as u8 };
    let mut got = 0u32;
    // best effort: an older stack ignores it, and the 1.5 s wait then reads a closed port as silent
    let _ = WSAIoctl(s, SIO_TCP_INITIAL_RTO, Some(&rto as *const _ as *const _), std::mem::size_of_val(&rto) as u32, None, 0, &mut got, None, None);
    let mut nb = 1u32;
    if ioctlsocket(s, FIONBIO, &mut nb) != 0 {
        // a blocking connect could hang the worker for the full OS timeout: give up instead
        let _ = closesocket(s);
        return Tcp::Silent;
    }
    let (sa, len) = socket_addr(ip, port);
    let tcp = if connect(s, sa.as_ptr() as *const SOCKADDR, len) == 0 {
        Tcp::Open
    } else if WSAGetLastError() != WSAEWOULDBLOCK {
        tcp_kind(WSAGetLastError().0)
    } else {
        let mut wr = FD_SET::default();
        wr.fd_count = 1;
        wr.fd_array[0] = s;
        let mut ex = wr;
        let ms = CONNECT_TIMEOUT.as_millis() as i32;
        let tv = TIMEVAL { tv_sec: ms / 1000, tv_usec: (ms % 1000) * 1000 };
        if select(0, None, Some(&mut wr), Some(&mut ex), Some(&tv)) > 0 {
            let mut err = 0i32;
            let mut optlen = std::mem::size_of::<i32>() as i32;
            let _ = getsockopt(s, SOL_SOCKET, SO_ERROR, windows::core::PSTR(&mut err as *mut i32 as *mut u8), &mut optlen);
            if err == 0 && wr.fd_count > 0 { Tcp::Open } else { tcp_kind(err) }
        } else {
            Tcp::Silent
        }
    };
    let _ = closesocket(s);
    tcp
}

/// The sockaddr bytes of `ip:port` and their length (a `sockaddr_in` or `sockaddr_in6`).
fn socket_addr(ip: IpAddr, port: u16) -> (Vec<u8>, i32) {
    let mut b = Vec::new();
    match ip {
        IpAddr::V4(a) => {
            b.extend_from_slice(&2u16.to_ne_bytes()); // AF_INET
            b.extend_from_slice(&port.to_be_bytes());
            b.extend_from_slice(&a.octets());
            b.extend_from_slice(&[0u8; 8]);
        }
        IpAddr::V6(a) => {
            b.extend_from_slice(&23u16.to_ne_bytes()); // AF_INET6
            b.extend_from_slice(&port.to_be_bytes());
            b.extend_from_slice(&[0u8; 4]); // flow info
            b.extend_from_slice(&a.octets());
            b.extend_from_slice(&[0u8; 4]); // scope id
        }
    }
    let n = b.len() as i32;
    (b, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classification_table() {
        use Echo::*;
        use Outcome::*;
        use Tcp::*;
        // (echo, tcp) -> outcome
        let t = [
            (Reply, Refused, UpPortClosed), // the usual closed port on a live LAN host
            (Reply, Open, UpPortOpen),
            (NoReply, Open, UpPortOpen),      // ICMP filtered, service answers: up
            (NoReply, Refused, UpPortClosed), // ICMP filtered, RST: up
            (NotSent, Open, UpPortOpen),
            (NotSent, Refused, UpPortClosed),
            (NoReply, Silent, HostDown), // an unanswered address: nothing at all
            (Reply, Silent, PortFiltered), // pings, but the port drops the SYN
            (NotSent, Silent, PortFiltered), // IPv6 / no echo: cannot tell down from filtered
        ];
        for (e, c, want) in t {
            assert_eq!(classify(e, c), want, "{e:?} + {c:?}");
        }
        // the four sentences and button words are the specified ones
        assert_eq!(UpPortClosed.text(), "host is up, port closed");
        assert_eq!(UpPortOpen.text(), "host is up, port open");
        assert_eq!(HostDown.text(), "host not answering (down, asleep or filtered)");
        assert_eq!(PortFiltered.text(), "port filtered/no reply");
        assert!([UpPortOpen, UpPortClosed, HostDown, PortFiltered].iter().all(|o| o.short().chars().count() <= 11));
    }

    #[test]
    fn only_a_refusal_proves_a_live_host() {
        for (code, want) in [(10061, Tcp::Refused), (10054, Tcp::Refused), (10053, Tcp::Refused), (10060, Tcp::Silent), (10065, Tcp::Silent), (10051, Tcp::Silent), (10035, Tcp::Silent), (0, Tcp::Silent)] {
            assert_eq!(tcp_kind(code), want, "winsock error {code}");
        }
    }

    #[test]
    fn socket_addresses_are_laid_out_like_the_winsock_structs() {
        let (b, n) = socket_addr(ip("10.0.0.3"), 25565);
        assert_eq!((b.len() as i32, n), (16, 16));
        assert_eq!(&b[0..2], &2u16.to_ne_bytes());
        assert_eq!(&b[2..4], &[0x63, 0xDD], "25565 in network order");
        assert_eq!(&b[4..8], &[192, 168, 50, 3]);
        let (b, n) = socket_addr(ip("2001:db8::1"), 443);
        assert_eq!((b.len() as i32, n), (28, 28));
        assert_eq!(&b[0..2], &23u16.to_ne_bytes());
        assert_eq!(&b[2..4], &[0x01, 0xBB]);
        assert_eq!(&b[8..10], &[0x20, 0x01]);
    }

    #[test]
    fn only_literal_ips_parse() {
        assert_eq!(parse_literal("10.0.0.3"), Some(ip("10.0.0.3")));
        assert_eq!(parse_literal("2001:db8::1"), Some(ip("2001:db8::1")));
        for bad in ["", "localhost", "example.com", "10.0.0.3.nip.io", "192.168.50", "10.0.0.3:80", "[::1]", "fe80::1%12", "1.2.3.4 ", " 1.2.3.4", "1.2.3.4/24", "0x7f.1", "http://1.2.3.4", "1.2.3.4\n"] {
            assert_eq!(parse_literal(bad), None, "{bad:?}");
        }
        assert_eq!(parse_literal(&"1".repeat(60)), None);
    }

    #[test]
    fn the_destination_must_be_a_unicast_row_of_the_table() {
        let table = vec![(ip("10.0.0.3"), 1u16), (ip("203.0.113.5"), 80), (ip("2001:db8::7"), 443), (ip("127.0.0.1"), 9)];
        assert_eq!(validate(ip("10.0.0.3"), 1, &table), Ok(()));
        assert_eq!(validate(ip("2001:db8::7"), 443, &table), Ok(()));
        assert_eq!(validate(ip("127.0.0.1"), 9, &table), Ok(()), "a loopback row may be connected to; it never gets an echo");
        // not a row: another port, another host
        assert_eq!(validate(ip("10.0.0.3"), 2, &table), Err(Reject::NotInTable));
        assert_eq!(validate(ip("10.0.0.4"), 1, &table), Err(Reject::NotInTable));
        // never even if the table said so
        let evil = vec![(ip("0.0.0.0"), 80), (ip("0.1.2.3"), 80), (ip("0.255.255.255"), 80), (ip("255.255.255.255"), 80), (ip("224.0.0.1"), 80), (ip("240.0.0.1"), 80), (ip("::"), 80), (ip("ff02::1"), 80), (ip("10.0.0.1"), 0)];
        for (a, p) in evil.iter().copied() {
            assert_eq!(validate(a, p, &evil), Err(Reject::NotUnicast), "{a}:{p}");
        }
        // text form: a hostname is not an address
        assert_eq!(validate_text("203.0.113.5", 80, &table), Ok(ip("203.0.113.5")));
        assert_eq!(validate_text("example.com", 80, &table), Err(Reject::NotLiteral));
        assert_eq!(validate_text("localhost", 9, &table), Err(Reject::NotLiteral));
        assert_eq!(validate_text("203.0.113.6", 80, &table), Err(Reject::NotInTable));
    }

    #[test]
    fn echoes_go_to_non_loopback_unicast_ipv4_only() {
        assert_eq!(echo_allowed(ip("10.0.0.3")), Some("10.0.0.3".parse().unwrap()));
        assert_eq!(echo_allowed(ip("203.0.113.5")), Some("203.0.113.5".parse().unwrap()));
        for no in ["127.0.0.1", "127.1.2.3", "0.0.0.0", "0.1.2.3", "0.255.0.9", "255.255.255.255", "224.0.0.251", "250.1.1.1", "::1", "2001:db8::1", "fe80::1"] {
            assert_eq!(echo_allowed(ip(no)), None, "{no}");
        }
    }

    #[test]
    fn one_test_at_a_time_and_five_seconds_between_tests_of_a_row() {
        let a = (ip("10.0.0.3"), 1u16);
        let b = (ip("203.0.113.5"), 80u16);
        let table = vec![a, b];
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = Registry::default();
        assert_eq!(r.gate(a, &table, t0), Ok(()));
        r.begin(a, t0);
        assert_eq!(r.get(a), Some(Slot::Running { since: t0 }));
        // while a runs, no other row starts, and a double click on a does not start a second
        assert_eq!(r.gate(b, &table, at(1)), Err(Reject::Busy));
        assert_eq!(r.gate(a, &table, at(1)), Err(Reject::Busy));
        r.finish(a, Outcome::UpPortClosed, at(2));
        assert_eq!(r.get(a), Some(Slot::Done { at: at(2), outcome: Outcome::UpPortClosed }));
        // another row may go right after; the same row waits 5 s from the end of its test
        assert_eq!(r.gate(b, &table, at(2)), Ok(()));
        assert_eq!(r.gate(a, &table, at(6)), Err(Reject::Cooldown));
        assert_eq!(r.gate(a, &table, at(7)), Ok(()));
        // a row that is not in the table is refused before anything else
        assert_eq!(r.gate((ip("10.9.9.9"), 1), &table, at(100)), Err(Reject::NotInTable));
        // a new test replaces the old result of the row
        r.begin(a, at(7));
        assert_eq!(r.get(a), Some(Slot::Running { since: at(7) }));
        assert_eq!(r.slots.len(), 1);
    }

    #[test]
    fn a_stuck_running_slot_expires_after_ten_seconds() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        assert!(!run_expired(t0, at(9)));
        assert!(run_expired(t0, at(10)));
        assert!(!run_expired(at(5), t0), "a clock that went backwards never expires");
        let a = (ip("10.0.0.3"), 1u16);
        let b = (ip("203.0.113.5"), 80u16);
        let table = vec![a, b];
        let mut r = Registry::default();
        r.begin(a, t0);
        assert_eq!(r.gate(b, &table, at(9)), Err(Reject::Busy));
        assert_eq!(r.gate(b, &table, at(10)), Ok(()), "the stuck test no longer blocks");
        assert_eq!(r.gate(a, &table, at(10)), Ok(()), "nor its own row");
    }

    #[test]
    fn the_registry_stays_small() {
        let t0 = Instant::now();
        let mut r = Registry::default();
        for i in 0..40u16 {
            let k = (ip("10.0.0.1"), i + 1);
            r.begin(k, t0);
            r.finish(k, Outcome::HostDown, t0);
        }
        assert!(r.slots.len() <= MAX_ENTRIES);
        assert!(r.get((ip("10.0.0.1"), 40)).is_some(), "the newest survives");
        assert!(r.get((ip("10.0.0.1"), 1)).is_none(), "the oldest went");
        // old results are forgotten when a new test starts
        let later = t0 + KEEP + Duration::from_secs(1);
        r.begin((ip("10.0.0.2"), 1), later);
        assert_eq!(r.slots.len(), 1);
    }

    /// The real thing against the loopback of this machine: an open port completes the handshake and
    /// a closed one is refused well inside the timeout (a plain Windows connect takes about 2 s for
    /// that). No echo is sent to loopback, so no ICMP leaves the machine.
    #[test]
    fn run_against_loopback_classifies_open_and_closed_ports() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert_eq!(run(ip("127.0.0.1"), port), Outcome::UpPortOpen);
        drop(l);
        let t0 = Instant::now();
        assert_eq!(run(ip("127.0.0.1"), port), Outcome::UpPortClosed);
        assert!(t0.elapsed() < CONNECT_TIMEOUT, "a refusal must not wait for the timeout: {:?}", t0.elapsed());
    }
}
