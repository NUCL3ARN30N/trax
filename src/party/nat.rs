//! What kind of NAT this PC sits behind, so the user can see up front whether a party needs
//! a TURN relay instead of finding out from a failed connection.
//!
//! Plain STUN binding requests (RFC 5389) from one socket to two servers. If both report the
//! same public address, the NAT keeps one mapping per socket and WebRTC can punch through it.
//! A different address per server is a symmetric ("strict") NAT, which direct connections
//! cannot cross — that needs TURN.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

/// The same servers the party connection itself uses (see `transport::STUN`).
const SERVERS: &[&str] = &["stun.cloudflare.com:3478", "stun.l.google.com:19302", "stun.nextcloud.com:443"];
pub(super) const MAGIC: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
const TIMEOUT: Duration = Duration::from_secs(2);
const ATTEMPTS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// No NAT at all: the socket's own address is what the world sees.
    Open,
    /// One mapping per socket whatever the destination — parties connect directly.
    Normal,
    /// A mapping per destination (symmetric NAT): direct connections fail, TURN is needed.
    Strict,
    /// No STUN server answered: UDP is blocked, or there is no internet.
    Blocked,
    /// Only one server answered, which is not enough to tell the two apart.
    Unclear,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub kind: Kind,
    /// The public address seen by the first server that answered.
    pub public: Option<SocketAddr>,
    /// Servers whose name has no usable address here: a DNS blocker (AdGuard, Pi-hole, the
    /// router) answers for them with an empty one, so they could not even be asked.
    pub blocked_names: Vec<String>,
}

impl Report {
    /// Does a party need a TURN relay on this network?
    pub fn needs_turn(&self) -> bool {
        matches!(self.kind, Kind::Strict | Kind::Blocked)
    }

    pub fn headline(&self) -> String {
        match self.kind {
            Kind::Open => "Open network — parties connect directly.".into(),
            Kind::Normal => "Normal NAT — parties connect directly.".into(),
            Kind::Strict => "Strict NAT — parties need a TURN server.".into(),
            Kind::Blocked => "No answer from the STUN servers — UDP looks blocked here.".into(),
            Kind::Unclear => "Only one server answered — can't tell how strict this network is.".into(),
        }
    }

    /// The sentence about servers a DNS blocker swallowed, if any.
    pub fn dns_note(&self) -> String {
        match self.blocked_names.len() {
            0 => String::new(),
            1 => format!(
                " {} has no address on this PC: a DNS blocker (AdGuard, Pi-hole, your router) \
                 answers for it with an empty one, so it could not be asked. The same happens \
                 to a TURN server whose name is blocked.",
                self.blocked_names[0]
            ),
            _ => format!(
                " {} have no address on this PC: a DNS blocker (AdGuard, Pi-hole, your router) \
                 answers for them with an empty one, so they could not be asked. The same \
                 happens to a TURN server whose name is blocked.",
                self.blocked_names.join(" and ")
            ),
        }
    }

    pub fn detail(&self) -> String {
        let seen = match self.public {
            Some(a) => format!("This PC appears on the internet as {a}. "),
            None => String::new(),
        };
        let dns = self.dns_note();
        match self.kind {
            Kind::Open | Kind::Normal => format!("{seen}No TURN server needed.{dns}"),
            Kind::Strict => format!(
                "{seen}Your router gives every connection a different port, which direct \
                 connections can't cross. Fill in a TURN server below, or let the other side \
                 host the party if their network is less strict.{dns}"
            ),
            Kind::Blocked => format!(
                "A firewall or this network blocks the UDP traffic parties use, or nothing here \
                 can reach the internet. A TURN server only helps once UDP gets out at all.{dns}"
            ),
            Kind::Unclear => {
                format!("{seen}Try again; if it keeps happening, a TURN server is the safe choice.{dns}")
            }
        }
    }
}

/// Ask every server from one socket and compare what they report back.
pub fn check() -> Report {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
        return Report { kind: Kind::Blocked, public: None, blocked_names: vec![] };
    };
    let _ = sock.set_read_timeout(Some(TIMEOUT));
    let local = sock.local_addr().ok();

    let mut seen: Vec<SocketAddr> = vec![];
    let mut blocked_names: Vec<String> = vec![];
    for server in SERVERS {
        let Some(addr) = resolve(server) else {
            blocked_names.push(server.split(':').next().unwrap_or(server).to_string());
            continue;
        };
        if let Some(mapped) = binding_request(&sock, addr) {
            seen.push(mapped);
        }
    }

    let public = seen.first().copied();
    // Nothing to punch through only when the address outside is this machine's own, port and
    // all. A router that happens to keep the port still rewrites the address.
    let ip = local_ip();
    let un_natted = |m: &SocketAddr| ip == Some(m.ip()) && local.is_some_and(|l| l.port() == m.port());
    let kind = match seen.len() {
        0 => Kind::Blocked,
        1 if un_natted(&seen[0]) => Kind::Open,
        1 => Kind::Unclear,
        _ if seen.iter().any(|a| *a != seen[0]) => Kind::Strict,
        _ if un_natted(&seen[0]) => Kind::Open,
        _ => Kind::Normal,
    };
    log::info!("nat check: {kind:?} (mapped {seen:?}, no address for {blocked_names:?})");
    Report { kind, public, blocked_names }
}

/// The address this PC uses to reach the internet. Connecting a UDP socket only picks the
/// route; nothing is sent.
fn local_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("1.1.1.1:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

fn resolve(server: &str) -> Option<SocketAddr> {
    server.to_socket_addrs().ok()?.find(|a| a.is_ipv4() && !is_null_address(a))
}

/// An address no server can be behind: a DNS blocker answers with one of these (0.0.0.0,
/// ::, 192.0.2.1 and the other documentation ranges) to make a name disappear.
pub(super) fn is_null_address(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_unspecified() || ip.is_broadcast() || ip.is_documentation(),
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // ::, 100::/64 (discard), 2001:db8::/32 (documentation)
            ip.is_unspecified()
                || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)
                || (s[0] == 0x2001 && s[1] == 0x0db8)
        }
    }
}

/// One STUN binding request; returns the public address the server saw.
fn binding_request(sock: &UdpSocket, server: SocketAddr) -> Option<SocketAddr> {
    let mut tx_id = [0u8; 12];
    tx_id.copy_from_slice(&rand::random::<u128>().to_be_bytes()[..12]);
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&0x0001u16.to_be_bytes()); // binding request
    req[2..4].copy_from_slice(&0u16.to_be_bytes()); // no attributes
    req[4..8].copy_from_slice(&MAGIC);
    req[8..20].copy_from_slice(&tx_id);

    for _ in 0..ATTEMPTS {
        if sock.send_to(&req, server).is_err() {
            continue;
        }
        let mut buf = [0u8; 576];
        // Ignore answers from anywhere else, and late answers to an earlier request.
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            if from == server {
                if let Some(mapped) = parse_mapped(&buf[..n], &tx_id) {
                    return Some(mapped);
                }
            }
        }
    }
    None
}

/// The (type, value) pairs of a STUN message whose header matches `tx_id`.
pub(super) fn attributes<'a>(buf: &'a [u8], tx_id: &[u8; 12]) -> Option<Vec<(u16, &'a [u8])>> {
    if buf.len() < 20 || buf[4..8] != MAGIC || buf[8..20] != tx_id[..] {
        return None;
    }
    let end = (20 + u16::from_be_bytes([buf[2], buf[3]]) as usize).min(buf.len());
    let mut out = vec![];
    let mut i = 20;
    while i + 4 <= end {
        let kind = u16::from_be_bytes([buf[i], buf[i + 1]]);
        let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        let Some(value) = buf.get(i + 4..i + 4 + len) else { break };
        out.push((kind, value));
        // Attributes are padded to a multiple of four bytes.
        i += 4 + len.div_ceil(4) * 4;
    }
    Some(out)
}

fn parse_mapped(buf: &[u8], tx_id: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20 || u16::from_be_bytes([buf[0], buf[1]]) != 0x0101 {
        return None; // not a binding success response
    }
    for (kind, value) in attributes(buf, tx_id)? {
        match kind {
            0x0020 => return address(value, tx_id, true),
            0x0001 => return address(value, tx_id, false),
            _ => {}
        }
    }
    None
}

/// (XOR-)MAPPED-ADDRESS or XOR-RELAYED-ADDRESS: reserved, family, port, address.
pub(super) fn address(value: &[u8], tx_id: &[u8; 12], xor: bool) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let mut port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]);
    if xor {
        port ^= u16::from_be_bytes([MAGIC[0], MAGIC[1]]);
    }
    match family {
        0x01 => {
            let mut ip = [0u8; 4];
            ip.copy_from_slice(value.get(4..8)?);
            if xor {
                for (b, m) in ip.iter_mut().zip(MAGIC) {
                    *b ^= m;
                }
            }
            Some(SocketAddr::from((ip, port)))
        }
        0x02 => {
            let mut ip = [0u8; 16];
            ip.copy_from_slice(value.get(4..20)?);
            if xor {
                let mut key = [0u8; 16];
                key[..4].copy_from_slice(&MAGIC);
                key[4..].copy_from_slice(tx_id);
                for (b, k) in ip.iter_mut().zip(key) {
                    *b ^= k;
                }
            }
            Some(SocketAddr::from((ip, port)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn response(tx_id: &[u8; 12], ip: [u8; 4], port: u16, xor: bool) -> Vec<u8> {
        let mut attr = vec![0u8, 0x01];
        let (p, a) = if xor {
            let mut a = ip;
            for (b, m) in a.iter_mut().zip(MAGIC) {
                *b ^= m;
            }
            (port ^ u16::from_be_bytes([MAGIC[0], MAGIC[1]]), a)
        } else {
            (port, ip)
        };
        attr.extend_from_slice(&p.to_be_bytes());
        attr.extend_from_slice(&a);
        let mut msg = vec![0x01, 0x01];
        msg.extend_from_slice(&((attr.len() + 4) as u16).to_be_bytes());
        msg.extend_from_slice(&MAGIC);
        msg.extend_from_slice(tx_id);
        msg.extend_from_slice(&if xor { 0x0020u16 } else { 0x0001u16 }.to_be_bytes());
        msg.extend_from_slice(&(attr.len() as u16).to_be_bytes());
        msg.extend_from_slice(&attr);
        msg
    }

    #[test]
    fn reads_mapped_addresses() {
        let tx = [7u8; 12];
        let want = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 5), 51234));
        assert_eq!(parse_mapped(&response(&tx, [203, 0, 113, 5], 51234, true), &tx), Some(want));
        assert_eq!(parse_mapped(&response(&tx, [203, 0, 113, 5], 51234, false), &tx), Some(want));
        // Another request's answer, and anything that isn't a success response, are ignored.
        assert_eq!(parse_mapped(&response(&[9u8; 12], [203, 0, 113, 5], 51234, true), &tx), None);
        let mut not_binding = response(&tx, [203, 0, 113, 5], 51234, true);
        not_binding[1] = 0x11;
        assert_eq!(parse_mapped(&not_binding, &tx), None);
        assert_eq!(parse_mapped(&[], &tx), None);
    }

    /// Asks the real STUN servers; run with `cargo test -- --ignored --nocapture nat`.
    #[test]
    #[ignore = "needs the internet"]
    fn check_this_network() {
        let report = check();
        println!("{}\n{}", report.headline(), report.detail());
        assert_ne!(report.kind, Kind::Blocked, "no STUN server answered");
    }

    #[test]
    fn a_blocked_name_is_named() {
        let report = Report {
            kind: Kind::Unclear,
            public: None,
            blocked_names: vec!["stun.l.google.com".into()],
        };
        assert!(report.detail().contains("stun.l.google.com"));
        assert!(report.detail().contains("DNS blocker"));
        assert!(Report { kind: Kind::Unclear, public: None, blocked_names: vec![] }.dns_note().is_empty());
        // The addresses a blocker hands out instead of a server's.
        assert!(is_null_address(&"0.0.0.0:3478".parse().unwrap()));
        assert!(is_null_address(&"192.0.2.1:3478".parse().unwrap()));
        assert!(is_null_address(&"[::]:3478".parse().unwrap()));
        assert!(is_null_address(&"[100::1]:3478".parse().unwrap()));
        assert!(!is_null_address(&"141.101.90.1:3478".parse().unwrap()));
    }

    #[test]
    fn advice_matches_the_kind() {
        let strict = Report { kind: Kind::Strict, public: None, blocked_names: vec![] };
        assert!(strict.needs_turn() && strict.headline().contains("TURN"));
        assert!(Report { kind: Kind::Blocked, public: None, blocked_names: vec![] }.needs_turn());
        assert!(!Report { kind: Kind::Normal, public: None, blocked_names: vec![] }.needs_turn());
        assert!(!Report { kind: Kind::Open, public: None, blocked_names: vec![] }.needs_turn());
    }
}
