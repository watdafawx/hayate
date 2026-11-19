//! Network adapters from GetIfTable2: byte counters, link speed, state.

use std::collections::HashMap;
use windows::Win32::NetworkManagement::IpHelper::*;
use windows::Win32::NetworkManagement::Ndis::*;

#[derive(Clone, Default)]
pub struct Nic {
    pub luid: u64,
    pub name: String,
    pub desc: String,
    pub up: bool,
    pub link_bps: u64,
    pub rx_bps: f64,
    pub tx_bps: f64,
    /// bytes since the adapter came up (MIB_IF_ROW2 octet counters)
    pub rx_total: u64,
    pub tx_total: u64,
    /// "wired", "wireless", "virtual"
    pub kind: &'static str,
}

/// Addresses of one adapter (GetAdaptersAddresses), for the detail view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Addr {
    pub luid: u64,
    /// "10.0.0.10 / 24"
    pub ipv4: String,
    pub ipv6: String,
    pub gateway: String,
    pub dns: String,
    /// "9C-6B-00-4A-1F-3E"
    pub mac: String,
}

/// System-wide socket counts (IPv4 + IPv6).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Conns {
    pub tcp_established: u32,
    pub tcp_listening: u32,
    pub udp: u32,
}

pub fn fmt_ipv4(b: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

/// RFC 5952 text form: lower-case hex, the longest run (2+) of zero groups as `::`.
pub fn fmt_ipv6(b: [u8; 16]) -> String {
    let g: Vec<u16> = (0..8).map(|i| u16::from_be_bytes([b[2 * i], b[2 * i + 1]])).collect();
    let (mut best, mut cur) = ((0usize, 0usize), (0usize, 0usize));
    for (i, &v) in g.iter().enumerate() {
        if v == 0 {
            if cur.1 == 0 {
                cur.0 = i;
            }
            cur.1 += 1;
            if cur.1 > best.1 {
                best = cur;
            }
        } else {
            cur.1 = 0;
        }
    }
    let hex = |s: &[u16]| s.iter().map(|v| format!("{v:x}")).collect::<Vec<_>>().join(":");
    if best.1 < 2 {
        return hex(&g);
    }
    format!("{}::{}", hex(&g[..best.0]), hex(&g[best.0 + best.1..]))
}

pub fn fmt_mac(m: &[u8]) -> String {
    m.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join("-")
}

/// Text of a SOCKADDR (IPv4 or IPv6), None for anything else.
unsafe fn sockaddr(p: *const windows::Win32::Networking::WinSock::SOCKADDR) -> Option<String> {
    use windows::Win32::Networking::WinSock::*;
    if p.is_null() {
        return None;
    }
    match (*p).sa_family {
        AF_INET => { let a = (*(p as *const SOCKADDR_IN)).sin_addr.S_un.S_un_b; Some(fmt_ipv4([a.s_b1, a.s_b2, a.s_b3, a.s_b4])) }
        AF_INET6 => Some(fmt_ipv6((*(p as *const SOCKADDR_IN6)).sin6_addr.u.Byte)),
        _ => None,
    }
}

/// Every adapter's addresses. One GetAdaptersAddresses call (~0.5-2 ms):
/// runs on the I/O worker, only while a NIC detail is on screen.
pub fn addresses() -> Option<Vec<Addr>> {
    use windows::Win32::Networking::WinSock::AF_UNSPEC;
    let mut len = 16 * 1024u32;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buf.resize(len as usize / 8 + 1, 0);
        let r = unsafe { GetAdaptersAddresses(AF_UNSPEC.0 as u32, GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST, None, Some(buf.as_mut_ptr() as *mut _), &mut len) };
        match r {
            0 => break,
            111 => continue, // ERROR_BUFFER_OVERFLOW: len now holds the size
            _ => return None,
        }
    }
    let mut out = Vec::new();
    unsafe {
        let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !a.is_null() {
            let x = &*a;
            let mut ad = Addr { luid: x.Luid.Value, mac: fmt_mac(&x.PhysicalAddress[..(x.PhysicalAddressLength as usize).min(8)]), ..Default::default() };
            let mut u = x.FirstUnicastAddress;
            while !u.is_null() {
                let s = sockaddr((*u).Address.lpSockaddr);
                if let Some(s) = s {
                    if s.contains(':') {
                        // prefer a global address over the link-local fe80::
                        if ad.ipv6.is_empty() || ad.ipv6.starts_with("fe80") {
                            ad.ipv6 = s;
                        }
                    } else if ad.ipv4.is_empty() {
                        ad.ipv4 = format!("{s} / {}", (*u).OnLinkPrefixLength);
                    }
                }
                u = (*u).Next;
            }
            let mut gw = x.FirstGatewayAddress;
            while !gw.is_null() && ad.gateway.is_empty() {
                ad.gateway = sockaddr((*gw).Address.lpSockaddr).filter(|s| !s.contains(':')).unwrap_or_default();
                gw = (*gw).Next;
            }
            let mut d = x.FirstDnsServerAddress;
            while !d.is_null() && ad.dns.is_empty() {
                ad.dns = sockaddr((*d).Address.lpSockaddr).filter(|s| !s.contains(':')).unwrap_or_default();
                d = (*d).Next;
            }
            out.push(ad);
            a = x.Next;
        }
    }
    Some(out)
}

/// TCP / UDP endpoint counts from the owner-less tables (no admin needed).
/// On the I/O worker, only while a NIC detail is on screen.
pub fn connections() -> Option<Conns> {
    use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    let mut c = Conns::default();
    let mut buf: Vec<u64> = Vec::new();
    let mut get = |tcp: bool, af: u32| -> Option<Vec<u32>> {
        let mut len = 0u32;
        for _ in 0..3 {
            let p = if buf.is_empty() { None } else { Some(buf.as_mut_ptr() as *mut std::ffi::c_void) };
            let r = unsafe {
                if tcp {
                    GetExtendedTcpTable(p, &mut len, false, af, TCP_TABLE_BASIC_ALL, 0)
                } else {
                    GetExtendedUdpTable(p, &mut len, false, af, UDP_TABLE_BASIC, 0)
                }
            };
            match r {
                0 if !buf.is_empty() => {
                    let b = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u32, len as usize / 4) };
                    return Some(b.to_vec());
                }
                0 | 122 => buf.resize(len as usize / 8 + 64, 0), // ERROR_INSUFFICIENT_BUFFER
                _ => return None,
            }
        }
        None
    };
    // rows: MIB_TCPROW (v4, 5 u32: state first) / MIB_TCP6ROW (state + 2 x 16-byte addr ...)
    let v4 = get(true, AF_INET.0 as u32)?;
    let (est, lis) = tcp_states(&v4, 5);
    c.tcp_established += est;
    c.tcp_listening += lis;
    if let Some(v6) = get(true, AF_INET6.0 as u32) {
        // MIB_TCP6ROW: State, LocalAddr[16], LocalScopeId, LocalPort, RemoteAddr[16], RemoteScopeId, RemotePort = 13 u32
        let (est, lis) = tcp_states(&v6, 13);
        c.tcp_established += est;
        c.tcp_listening += lis;
    }
    c.udp += get(false, AF_INET.0 as u32).and_then(|v| v.first().copied()).unwrap_or(0);
    c.udp += get(false, AF_INET6.0 as u32).and_then(|v| v.first().copied()).unwrap_or(0);
    Some(c)
}

/// (established, listening) from a TCP table as u32 words: dwNumEntries,
/// then rows of `row` words with the state first.
pub fn tcp_states(t: &[u32], row: usize) -> (u32, u32) {
    let n = t.first().copied().unwrap_or(0) as usize;
    let (mut est, mut lis) = (0, 0);
    for i in 0..n {
        match t.get(1 + i * row) {
            Some(5) => est += 1, // MIB_TCP_STATE_ESTAB
            Some(2) => lis += 1, // MIB_TCP_STATE_LISTEN
            Some(_) => {}
            None => break,
        }
    }
    (est, lis)
}

#[derive(Default)]
pub struct NetState {
    last: HashMap<u64, (u64, u64, std::time::Instant)>,
    /// LUIDs the last full enumeration kept, in its order
    kept: Vec<u64>,
    /// light samples taken; every LIGHT_FULL-th one enumerates again
    light_n: u32,
}

/// Light sampling (window hidden: History only) re-reads just the adapters
/// the last full enumeration kept, one GetIfEntry2 each: GetIfTable2 walks
/// every interface including the filter and miniport shims (dev PC: 3.2 ms vs
/// 0.33 ms for 8 kept adapters). Every LIGHT_FULL-th light sample (hidden
/// NIC samples come every 3 s: 60 s) enumerates again to pick up new ones.
const LIGHT_FULL: u32 = 20;

fn wstr(a: &[u16]) -> String {
    let n = a.iter().position(|&c| c == 0).unwrap_or(a.len());
    String::from_utf16_lossy(&a[..n])
}

/// (alias, description, up) of an adapter worth showing, None for noise.
fn keep(r: &MIB_IF_ROW2) -> Option<(String, String, bool)> {
    let alias = wstr(&r.Alias);
    let desc = wstr(&r.Description);
    let hw = r.InterfaceAndOperStatusFlags._bitfield & 1 != 0;
    let up = r.OperStatus == IfOperStatusUp;
    let ty = r.Type;
    // loopback, NDIS filter shims, and miniport duplicates are noise
    if ty == 24 || alias.contains("-WFP") || alias.contains("-QoS") || alias.contains("Filter-") || desc.contains("Filter") {
        return None;
    }
    let tunnel = ty == 53 || ty == 131;
    if !(hw || (tunnel && up)) || alias.is_empty() {
        return None;
    }
    Some((alias, desc, up))
}

impl NetState {
    fn nic(&mut self, r: &MIB_IF_ROW2, now: std::time::Instant) -> Option<Nic> {
        let (name, desc, up) = keep(r)?;
        // SAFETY: NET_LUID is a plain u64 union
        let luid = unsafe { r.InterfaceLuid.Value };
        let (rx, tx) = (r.InOctets, r.OutOctets);
        let (mut rxb, mut txb) = (0.0, 0.0);
        if let Some((lr, lt, lt0)) = self.last.get(&luid) {
            let dt = now.duration_since(*lt0).as_secs_f64();
            if dt > 0.05 {
                rxb = rx.saturating_sub(*lr) as f64 / dt;
                txb = tx.saturating_sub(*lt) as f64 / dt;
            }
        }
        self.last.insert(luid, (rx, tx, now));
        let kind = match r.Type { 71 => "wireless", 6 => "wired", _ => "virtual" };
        Some(Nic { luid, name, desc, up, link_bps: r.ReceiveLinkSpeed.max(r.TransmitLinkSpeed), rx_bps: rxb * 8.0, tx_bps: txb * 8.0, rx_total: rx, tx_total: tx, kind })
    }

    /// `light`: re-read only the adapters found last time (see LIGHT_FULL).
    pub fn sample(&mut self, light: bool) -> Vec<Nic> {
        let full = !light || self.kept.is_empty() || self.light_n.is_multiple_of(LIGHT_FULL);
        if light {
            self.light_n = self.light_n.wrapping_add(1);
        }
        let mut out = Vec::new();
        let now = std::time::Instant::now();
        if !full {
            for luid in self.kept.clone() {
                let mut r = MIB_IF_ROW2::default();
                r.InterfaceLuid.Value = luid;
                if unsafe { GetIfEntry2(&mut r) }.is_err() {
                    // gone: enumerate again next time
                    self.kept.clear();
                    continue;
                }
                if let Some(n) = self.nic(&r, now) {
                    out.push(n);
                }
            }
        } else {
            unsafe {
                let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
                if GetIfTable2(&mut table).is_err() || table.is_null() {
                    return out;
                }
                let t = &*table;
                let rows = std::slice::from_raw_parts(t.Table.as_ptr(), t.NumEntries as usize);
                for r in rows {
                    if let Some(n) = self.nic(r, now) {
                        out.push(n);
                    }
                }
                FreeMibTable(table as *const _);
            }
            self.kept = out.iter().map(|n| n.luid).collect();
        }
        // connected first, then hardware order
        out.sort_by_key(|n| !n.up);
        let _ = NET_IF_OPER_STATUS::default();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_compresses_longest_zero_run() {
        let mut b = [0u8; 16];
        b[0] = 0x20;
        b[1] = 0x01;
        b[2] = 0x0d;
        b[3] = 0xb8;
        b[15] = 1;
        assert_eq!(fmt_ipv6(b), "2001:db8::1");
        assert_eq!(fmt_ipv6([0; 16]), "::");
        let mut ll = [0u8; 16];
        ll[0] = 0xfe;
        ll[1] = 0x80;
        ll[8] = 0x12;
        ll[9] = 0x34;
        assert_eq!(fmt_ipv6(ll), "fe80::1234:0:0:0");
    }

    #[test]
    fn ipv4_and_mac_text() {
        assert_eq!(fmt_ipv4([10, 0, 0, 3]), "10.0.0.3");
        assert_eq!(fmt_mac(&[0x9c, 0x6b, 0, 0x4a]), "9C-6B-00-4A");
    }

    #[test]
    fn tcp_table_counts_states() {
        // 3 rows of 5 words: ESTAB, LISTEN, TIME_WAIT
        let t = [3, 5, 0, 0, 0, 0, 2, 0, 0, 0, 0, 11, 0, 0, 0, 0];
        assert_eq!(tcp_states(&t, 5), (1, 1));
        // truncated table stops cleanly
        assert_eq!(tcp_states(&[4, 5, 0, 0, 0, 0], 5), (1, 0));
    }
}
