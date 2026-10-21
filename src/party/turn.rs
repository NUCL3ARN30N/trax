//! Does the TURN server from the settings actually work?
//!
//! A TURN server that is wrong, unreachable or of a kind this build can't use looks exactly
//! like a network that can't be crossed: the party just never connects. This asks the server
//! for a relay address the same way a party does, so the settings page can say which of the
//! two it is.
//!
//! The WebRTC stack trax uses relays over **UDP only**: `turns:` (TLS) and `?transport=tcp`
//! are skipped while candidates are gathered, with nothing but a log line to show for it, so
//! they are refused here instead.

use super::identity;
use super::transport::Turn;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceServer,
    RTCIceTransportPolicy, RTCPeerConnectionIceErrorEvent,
};

/// How long the server gets to hand out a relay address.
const TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    /// No TURN server is set (which is fine on most networks).
    NotSet,
    /// The address can't be used, for a reason that needs no network: the message says why.
    Unusable(String),
    /// The server handed out a relay address: parties can use it.
    Works { relay: String },
    /// The server answered, but not with a relay address: usually a wrong user or password.
    Refused { code: u16, text: String },
    /// The server relays, but the connection still ended up without a relay candidate.
    NotGathered { relay: String },
    /// Nothing came back at all.
    NoAnswer,
    /// The connection couldn't even be set up.
    Error(String),
}

impl Check {
    pub fn ok(&self) -> bool {
        matches!(self, Check::Works { .. })
    }

    pub fn headline(&self) -> String {
        match self {
            Check::NotSet => "No TURN server set.".into(),
            Check::Unusable(_) => "This TURN address can't be used.".into(),
            Check::Works { .. } => "The TURN server works.".into(),
            Check::Refused { .. } => "The TURN server refused this PC.".into(),
            Check::NotGathered { .. } => "The server works, but trax couldn't use it.".into(),
            Check::NoAnswer => "No answer from the TURN server.".into(),
            Check::Error(_) => "The test couldn't run.".into(),
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Check::NotSet => {
                "Parties connect directly. Fill one in if “Check my network” says this network \
                 is strict, or if a party can't connect."
                    .into()
            }
            Check::Unusable(why) => why.clone(),
            Check::Works { relay } => {
                format!("It gave this PC the relay address {relay}. A party can get through here.")
            }
            Check::Refused { code, text } => {
                let why = match code {
                    401 | 403 => "The user and password were not accepted. ",
                    486 | 508 => "The server has no room for another relay right now. ",
                    _ => "",
                };
                match text.is_empty() {
                    true => format!("{why}It answered with {code}."),
                    false => format!("{why}It answered with {code} {text}."),
                }
            }
            Check::NotGathered { relay } => {
                format!(
                    "It gave this PC the relay address {relay}, but no relay candidate reached                      the connection. Please report this with the log (trax writes one to the                      data folder); a party will not get through until it is fixed."
                )
            }
            Check::NoAnswer => {
                "The server didn't give this PC a relay address within 8 seconds. Check the \
                 address, the user and the password, and that the server allows UDP on that \
                 port — a firewall between here and it blocks the same traffic a party uses."
                    .into()
            }
            Check::Error(e) => e.clone(),
        }
    }
}

/// What is wrong with `url` before anything is sent, if anything.
fn url_problem(url: &str) -> Option<String> {
    let lower = url.to_lowercase();
    if lower.starts_with("turns:") {
        return Some(
            "trax can only relay over plain UDP, so a turns: (TLS) address can't be used. Use \
             the turn: address of the same server, usually on port 3478."
                .into(),
        );
    }
    if !lower.starts_with("turn:") {
        return Some("The address has to start with turn:, for example turn:turn.example.com:3478.".into());
    }
    if lower.contains("transport=tcp") {
        return Some(
            "trax can only relay over UDP, so ?transport=tcp can't be used. Leave the transport \
             off, or set it to udp."
                .into(),
        );
    }
    let rest = &url[5..];
    let host = rest.split(['?', '/']).next().unwrap_or("");
    if host.is_empty() || host.starts_with(':') {
        return Some("The address is missing the server, for example turn:turn.example.com:3478.".into());
    }
    None
}

/// Ask the server for a relay address. Blocks for up to [`TIMEOUT`]; call it off the UI thread.
pub fn check(turn: &Turn) -> Check {
    let url = turn.url.trim().to_string();
    if url.is_empty() {
        return Check::NotSet;
    }
    if let Some(why) = url_problem(&url) {
        return Check::Unusable(why);
    }
    // Both are mandatory: the WebRTC stack refuses a TURN server without them.
    if turn.username.trim().is_empty() || turn.credential.is_empty() {
        return Check::Unusable(
            "A TURN server needs a user and a password; one of them is empty.".into(),
        );
    }
    // What the server itself says, which is where a wrong password or a blocked port shows.
    let server = url["turn:".len()..].split(['?', '/']).next().unwrap_or("").to_string();
    let server = if server.contains(':') { server } else { format!("{server}:3478") };
    let user = turn.username.trim().to_string();
    let answer = allocate::ask(&server, &user, &turn.credential);
    let Check::Works { relay } = answer else { return answer };

    // The server works; now make sure the WebRTC stack gets a relay candidate out of it,
    // because that is what a party actually needs.
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => return Check::Error(format!("{e}")),
    };
    match runtime.block_on(gather(url, user, turn.credential.clone())) {
        Check::Works { relay } => Check::Works { relay },
        Check::Error(e) => Check::Error(e),
        // Everything was right and it still did not end up in the connection.
        _ => Check::NotGathered { relay },
    }
}

/// Collects what the ICE agent is told about the server while it gathers.
#[derive(Default)]
struct Errors(Mutex<Vec<(u16, String)>>);

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Errors {
    async fn on_ice_candidate_error(&self, e: RTCPeerConnectionIceErrorEvent) {
        log::info!("turn check: {} answered {} {}", e.url, e.error_code, e.error_text);
        self.0.lock().unwrap().push((e.error_code, e.error_text));
    }
}

/// Does the WebRTC stack itself gather a relay candidate from this server?
async fn gather(url: String, username: String, credential: String) -> Check {
    let identity = identity::ephemeral();
    let errors = Arc::new(Errors::default());
    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(vec![RTCIceServer { urls: vec![url], username, credential }])
        // Relay only: every candidate that turns up had to come from the TURN server.
        .with_ice_transport_policy(RTCIceTransportPolicy::Relay)
        .with_certificates(vec![identity.certificate.clone()])
        .build();
    let Some(runtime) = webrtc::runtime::default_runtime() else {
        return Check::Error("no WebRTC runtime".into());
    };
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_handler(errors.clone())
        .with_runtime(runtime)
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await;
    let pc = match pc {
        Ok(pc) => pc,
        Err(e) => return Check::Error(format!("{e}")),
    };
    // A connection without a channel has nothing to gather for.
    if let Err(e) = pc.create_data_channel("probe", None).await {
        let _ = pc.close().await;
        return Check::Error(format!("{e}"));
    }
    let offer = match pc.create_offer(None).await {
        Ok(o) => o,
        Err(e) => {
            let _ = pc.close().await;
            return Check::Error(format!("{e}"));
        }
    };
    if let Err(e) = pc.set_local_description(offer).await {
        let _ = pc.close().await;
        return Check::Error(format!("{e}"));
    }
    // Poll the description instead of the gathering state: one relay address is the answer,
    // whether or not the rest of the gathering has finished.
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut result = Check::NoAnswer;
    while tokio::time::Instant::now() < deadline {
        if let Some(desc) = pc.local_description().await {
            if let Some(relay) = relay_candidate(&desc.sdp) {
                result = Check::Works { relay };
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = pc.close().await;
    // What the server said beats "nothing came back".
    if result == Check::NoAnswer {
        if let Some((code, text)) = errors.0.lock().unwrap().first().cloned() {
            return Check::Refused { code, text };
        }
    }
    result
}

/// "address:port" of the first relay candidate in an SDP.
fn relay_candidate(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        let line = line.trim();
        if !line.contains("typ relay") {
            continue;
        }
        // a=candidate:<foundation> <component> <proto> <priority> <address> <port> typ relay …
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let (Some(addr), Some(port)) = (parts.get(4), parts.get(5)) {
            return Some(format!("{addr}:{port}"));
        }
    }
    None
}

// ------------------------------------------------------------------ the server itself

/// A TURN Allocate (RFC 5766) straight from a UDP socket, so the answer is the server's own
/// instead of "no candidates turned up". Long-term credentials: the first request is refused
/// with a realm and a nonce, the second one carries them and a MESSAGE-INTEGRITY.
mod allocate {
    use super::super::nat::{MAGIC, address, attributes, is_null_address};
    use super::Check;
    use hmac::{KeyInit, Mac, SimpleHmac};
    use md5::{Digest, Md5};
    use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
    use std::time::Duration;

    const ALLOCATE_REQUEST: u16 = 0x0003;
    const ALLOCATE_SUCCESS: u16 = 0x0103;
    const ALLOCATE_ERROR: u16 = 0x0113;
    const ATTR_USERNAME: u16 = 0x0006;
    const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
    const ATTR_ERROR_CODE: u16 = 0x0009;
    const ATTR_REALM: u16 = 0x0014;
    const ATTR_NONCE: u16 = 0x0015;
    const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
    const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
    const TIMEOUT: Duration = Duration::from_secs(3);

    /// Ask `server` for a relay address for `user`/`pass`.
    pub(super) fn ask(server: &str, user: &str, pass: &str) -> Check {
        let addr = match resolve(server) {
            Resolved::Addr(addr) => addr,
            Resolved::Blocked(ip) => {
                return Check::Unusable(format!(
                    "{server} has no real address on this PC: the name resolves to {ip}, which a \
                     DNS blocker (AdGuard, Pi-hole, your router) hands out to make a name \
                     disappear. Allow the name, or put the server's IP address here instead."
                ));
            }
            Resolved::Unknown => {
                return Check::Unusable(format!(
                    "Nothing answers for {server}. Check the spelling of the address and that \
                     this PC can look names up at all."
                ));
            }
        };
        let Ok(sock) = UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }) else {
            return Check::Error("no UDP socket".into());
        };
        let _ = sock.set_read_timeout(Some(TIMEOUT));

        // First try: no credentials, which a TURN server answers with its realm and a nonce.
        let (tx, req) = request(&[]);
        let Some(reply) = exchange(&sock, addr, &req, &tx) else {
            return Check::NoAnswer;
        };
        let (realm, nonce) = match &reply {
            Reply::Error { code: 401, realm, nonce, .. } if !realm.is_empty() && !nonce.is_empty() => {
                (realm.clone(), nonce.clone())
            }
            // A server that hands out a relay address without asking who we are (rare, but
            // some open relays do) is just as good.
            Reply::Relay(relay) => return Check::Works { relay: relay.clone() },
            Reply::Error { code, text, .. } => return Check::Refused { code: *code, text: text.clone() },
        };

        // Second try: with the credentials the server asked for.
        let key = long_term_key(user, &realm, pass);
        let mut attempts = 0;
        let (mut realm, mut nonce) = (realm, nonce);
        loop {
            let attrs = vec![
                (ATTR_USERNAME, user.as_bytes().to_vec()),
                (ATTR_REALM, realm.as_bytes().to_vec()),
                (ATTR_NONCE, nonce.as_bytes().to_vec()),
            ];
            let (tx, req) = signed_request(&attrs, &key);
            let Some(reply) = exchange(&sock, addr, &req, &tx) else {
                return Check::NoAnswer;
            };
            match reply {
                Reply::Relay(relay) => return Check::Works { relay },
                // A stale nonce is the server asking to try again with a fresh one.
                Reply::Error { code: 438, realm: r, nonce: n, .. } if attempts < 2 && !n.is_empty() => {
                    attempts += 1;
                    if !r.is_empty() {
                        realm = r;
                    }
                    nonce = n;
                }
                Reply::Error { code, text, .. } => return Check::Refused { code, text },
            }
        }
    }

    enum Reply {
        Relay(String),
        Error { code: u16, text: String, realm: String, nonce: String },
    }

    pub(super) enum Resolved {
        Addr(SocketAddr),
        /// The name resolves, but to an address no server can be behind.
        Blocked(String),
        /// The name resolves to nothing at all.
        Unknown,
    }

    fn resolve(server: &str) -> Resolved {
        let Ok(addrs) = server.to_socket_addrs() else { return Resolved::Unknown };
        let mut addrs: Vec<SocketAddr> = addrs.collect();
        // IPv4 first: a TURN server reached over IPv6 is of no use to an IPv4-only peer.
        addrs.sort_by_key(|a| !a.is_ipv4());
        match addrs.iter().find(|a| !is_null_address(a)) {
            Some(addr) => Resolved::Addr(*addr),
            None => match addrs.first() {
                Some(addr) => Resolved::Blocked(addr.ip().to_string()),
                None => Resolved::Unknown,
            },
        }
    }

    /// Send `req` (twice, in case a packet is lost) and read the answer to it.
    fn exchange(sock: &UdpSocket, addr: SocketAddr, req: &[u8], tx: &[u8; 12]) -> Option<Reply> {
        for _ in 0..2 {
            if sock.send_to(req, addr).is_err() {
                continue;
            }
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                if from != addr {
                    continue;
                }
                if let Some(reply) = parse(&buf[..n], tx) {
                    return Some(reply);
                }
            }
        }
        None
    }

    fn parse(buf: &[u8], tx: &[u8; 12]) -> Option<Reply> {
        let kind = u16::from_be_bytes([*buf.first()?, *buf.get(1)?]);
        if kind != ALLOCATE_SUCCESS && kind != ALLOCATE_ERROR {
            return None;
        }
        let attrs = attributes(buf, tx)?;
        if kind == ALLOCATE_SUCCESS {
            let relayed = attrs.iter().find(|(k, _)| *k == ATTR_XOR_RELAYED_ADDRESS)?;
            return Some(Reply::Relay(address(relayed.1, tx, true)?.to_string()));
        }
        let text_of = |k: u16| {
            attrs
                .iter()
                .find(|(kind, _)| *kind == k)
                .and_then(|(_, v)| String::from_utf8(v.to_vec()).ok())
                .unwrap_or_default()
        };
        let (mut code, mut text) = (0u16, String::new());
        if let Some((_, v)) = attrs.iter().find(|(k, _)| *k == ATTR_ERROR_CODE) {
            // ERROR-CODE: two zero bytes, a class (3 bits), a number, then the reason.
            if v.len() >= 4 {
                code = (v[2] & 0x07) as u16 * 100 + v[3] as u16;
                text = String::from_utf8_lossy(&v[4..]).trim().to_string();
            }
        }
        Some(Reply::Error { code, text, realm: text_of(ATTR_REALM), nonce: text_of(ATTR_NONCE) })
    }

    /// An Allocate request for a UDP relay, plus `extra` attributes.
    fn request(extra: &[(u16, Vec<u8>)]) -> ([u8; 12], Vec<u8>) {
        let mut tx = [0u8; 12];
        tx.copy_from_slice(&rand::random::<u128>().to_be_bytes()[..12]);
        let mut attrs: Vec<(u16, Vec<u8>)> = vec![(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])];
        attrs.extend_from_slice(extra);
        (tx, message(ALLOCATE_REQUEST, &tx, &attrs))
    }

    /// The same, signed with MESSAGE-INTEGRITY (HMAC-SHA1 over the message, whose length
    /// field already counts the signature).
    fn signed_request(extra: &[(u16, Vec<u8>)], key: &[u8]) -> ([u8; 12], Vec<u8>) {
        let (tx, mut msg) = request(extra);
        let signed_len = (msg.len() - 20 + 24) as u16;
        msg[2..4].copy_from_slice(&signed_len.to_be_bytes());
        let mut mac = <SimpleHmac<sha1::Sha1> as KeyInit>::new_from_slice(key).expect("any key length");
        mac.update(&msg);
        let tag = mac.finalize().into_bytes();
        msg.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
        msg.extend_from_slice(&20u16.to_be_bytes());
        msg.extend_from_slice(&tag[..20]);
        (tx, msg)
    }

    /// MD5(user:realm:password), the long-term credential key of RFC 5389.
    fn long_term_key(user: &str, realm: &str, pass: &str) -> Vec<u8> {
        let mut md5 = Md5::new();
        md5.update(format!("{user}:{realm}:{pass}").as_bytes());
        md5.finalize().to_vec()
    }

    fn message(kind: u16, tx: &[u8; 12], attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = vec![];
        for (k, v) in attrs {
            body.extend_from_slice(&k.to_be_bytes());
            body.extend_from_slice(&(v.len() as u16).to_be_bytes());
            body.extend_from_slice(v);
            // Attributes are padded to a multiple of four bytes.
            body.resize(body.len().div_ceil(4) * 4, 0);
        }
        let mut msg = Vec::with_capacity(20 + body.len());
        msg.extend_from_slice(&kind.to_be_bytes());
        msg.extend_from_slice(&(body.len() as u16).to_be_bytes());
        msg.extend_from_slice(&MAGIC);
        msg.extend_from_slice(tx);
        msg.extend_from_slice(&body);
        msg
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn builds_an_allocate_request() {
            let (tx, msg) = request(&[]);
            assert_eq!(u16::from_be_bytes([msg[0], msg[1]]), ALLOCATE_REQUEST);
            assert_eq!(u16::from_be_bytes([msg[2], msg[3]]) as usize, msg.len() - 20);
            assert_eq!(msg[4..8], MAGIC);
            assert_eq!(msg[8..20], tx[..]);
            let attrs = attributes(&msg, &tx).unwrap();
            assert_eq!(attrs, vec![(ATTR_REQUESTED_TRANSPORT, &[17u8, 0, 0, 0][..])]);
        }

        #[test]
        fn signs_with_the_length_the_signature_will_have() {
            let key = long_term_key("user", "realm", "pass");
            let (tx, msg) = signed_request(&[(ATTR_USERNAME, b"user".to_vec())], &key);
            // The header length counts everything, signature included.
            assert_eq!(u16::from_be_bytes([msg[2], msg[3]]) as usize, msg.len() - 20);
            let attrs = attributes(&msg, &tx).unwrap();
            let (kind, value) = *attrs.last().unwrap();
            assert_eq!(kind, ATTR_MESSAGE_INTEGRITY);
            assert_eq!(value.len(), 20);
            // RFC 5389 §15.4: the key is MD5 of user:realm:password.
            assert_eq!(key.len(), 16);
        }

        #[test]
        fn reads_a_relay_address_and_an_error() {
            let tx = [3u8; 12];
            let mut xor_ip = [203u8, 0, 113, 7];
            for (b, m) in xor_ip.iter_mut().zip(MAGIC) {
                *b ^= m;
            }
            let port = 49152u16 ^ u16::from_be_bytes([MAGIC[0], MAGIC[1]]);
            let mut value = vec![0, 0x01];
            value.extend_from_slice(&port.to_be_bytes());
            value.extend_from_slice(&xor_ip);
            let msg = message(ALLOCATE_SUCCESS, &tx, &[(ATTR_XOR_RELAYED_ADDRESS, value)]);
            assert!(matches!(parse(&msg, &tx), Some(Reply::Relay(r)) if r == "203.0.113.7:49152"));

            let err = message(
                ALLOCATE_ERROR,
                &tx,
                &[
                    (ATTR_ERROR_CODE, vec![0, 0, 4, 1, b'U', b'n', b'a', b'u', b't', b'h']),
                    (ATTR_REALM, b"example.org".to_vec()),
                    (ATTR_NONCE, b"abc123".to_vec()),
                ],
            );
            match parse(&err, &tx) {
                Some(Reply::Error { code, text, realm, nonce }) => {
                    assert_eq!((code, text.as_str(), realm.as_str(), nonce.as_str()), (401, "Unauth", "example.org", "abc123"));
                }
                other => panic!("expected an error reply, got {}", other.is_some()),
            }
            // An answer to another request is not an answer to this one.
            assert!(parse(&msg, &[9u8; 12]).is_none());
        }

        #[test]
        fn a_blocked_name_is_told_apart_from_one_that_does_not_exist() {
            assert!(matches!(resolve("141.101.90.1:3478"), Resolved::Addr(_)));
            // What a DNS blocker answers with.
            assert!(matches!(resolve("0.0.0.0:3478"), Resolved::Blocked(_)));
            assert!(matches!(resolve("192.0.2.1:3478"), Resolved::Blocked(_)));
            assert!(matches!(resolve("not a host name:3478"), Resolved::Unknown));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_this_build_cannot_use_are_refused() {
        assert!(url_problem("turn:turn.example.com:3478").is_none());
        assert!(url_problem("turn:turn.example.com").is_none());
        assert!(url_problem("turn:turn.example.com:3478?transport=udp").is_none());
        assert!(url_problem("turns:turn.example.com:5349").unwrap().contains("UDP"));
        assert!(url_problem("turn:turn.example.com:443?transport=tcp").unwrap().contains("UDP"));
        assert!(url_problem("stun:stun.example.com:3478").unwrap().contains("turn:"));
        assert!(url_problem("turn.example.com:3478").unwrap().contains("turn:"));
        assert!(url_problem("turn::3478").unwrap().contains("missing"));
    }

    #[test]
    fn credentials_are_mandatory() {
        let turn = Turn { url: "turn:turn.example.com:3478".into(), username: String::new(), credential: "x".into() };
        assert!(matches!(check(&turn), Check::Unusable(_)));
        assert_eq!(check(&Turn::default()), Check::NotSet);
    }

    #[test]
    fn reads_the_relay_address() {
        let sdp = "a=candidate:1 1 udp 2122260223 192.168.1.5 50000 typ host\r\n\
                   a=candidate:2 1 udp 41885439 203.0.113.9 56000 typ relay raddr 0.0.0.0 rport 0\r\n";
        assert_eq!(relay_candidate(sdp), Some("203.0.113.9:56000".into()));
        assert_eq!(relay_candidate("a=candidate:1 1 udp 1 10.0.0.1 1 typ host"), None);
    }

    /// Asks a real TURN server, from `TRAX_TURN_URL`, `TRAX_TURN_USER` and `TRAX_TURN_PASS`:
    /// `cargo test turn_server -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a TURN server"]
    fn turn_server_from_the_environment() {
        // RUST_LOG=debug shows what the ICE agent does with the server.
        let _ = env_logger::builder().is_test(false).try_init();
        let turn = Turn {
            url: std::env::var("TRAX_TURN_URL").unwrap_or_default(),
            username: std::env::var("TRAX_TURN_USER").unwrap_or_default(),
            credential: std::env::var("TRAX_TURN_PASS").unwrap_or_default(),
        };
        let result = check(&turn);
        println!("{}\n{}", result.headline(), result.detail());
        assert!(result.ok(), "{result:?}");
    }
}
