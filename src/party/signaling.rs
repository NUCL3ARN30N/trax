//! Finding each other without a server: encrypted messages through public Nostr relays.
//!
//! Every message is an ephemeral Nostr event (kind 25050) tagged with the room topic (a hash
//! of the room code). The content is sealed with the room key (code + password), so relays
//! and anyone without the code and password can't read or forge offers. The same message is
//! sent to all relays; any one that works is enough. Only offers and answers go this way —
//! once the DataChannel is open, signaling is idle.

use super::code;
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, Tag};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

pub const EVENT_KIND: u16 = 25050;
/// Messages older (or newer) than this are ignored, so recorded traffic can't be replayed.
const MAX_AGE_SECS: i64 = 60;

pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.nostr.band",
    "wss://relay.snort.social",
    "wss://nostr.mom",
];

/// What travels (encrypted) through the relays.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Envelope {
    /// "join", "offer", "answer", "auth_failed".
    pub t: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub sdp: String,
    pub nonce: String,
    pub ts: i64,
}

impl Envelope {
    pub fn new(t: &str, from: &str, to: &str, sdp: &str) -> Envelope {
        Envelope {
            t: t.into(),
            from: from.into(),
            to: to.into(),
            sdp: sdp.into(),
            nonce: format!("{:032x}", rand::random::<u128>()),
            ts: now(),
        }
    }
}

/// Outer, plain JSON in the event content. `room` is sealed with the room key. A `join`
/// also carries `probe`, its nonce sealed with the code-only key, so a host can answer a
/// wrong password with `auth_failed` without learning anything else.
#[derive(Serialize, Deserialize)]
struct Wire {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    room: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    probe: String,
    /// `auth_failed` only: sealed with the code-only key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    code: String,
}

/// A message read from the relays.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Message(Envelope),
    /// Someone with the right code but the wrong password tried to join (host side).
    WrongPasswordJoin { nonce: String },
    /// Our join was refused because of the password (guest side).
    AuthFailed { to: String },
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn b64(bytes: &[u8]) -> String {
    data_encoding::BASE64.encode(bytes)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    data_encoding::BASE64.decode(s.as_bytes()).ok()
}

/// Keys and replay protection for one room.
pub struct Room {
    room_key: [u8; 32],
    code_key: [u8; 32],
    pub topic: String,
    seen: Mutex<(HashSet<String>, VecDeque<String>)>,
}

impl Room {
    pub fn new(code: &str, password: &str) -> Room {
        Room {
            room_key: code::room_key(code, password),
            code_key: code::code_key(code),
            topic: code::topic(code),
            seen: Mutex::new((HashSet::new(), VecDeque::new())),
        }
    }

    /// Event content for `env`.
    pub fn seal(&self, env: &Envelope) -> String {
        let plain = serde_json::to_vec(env).unwrap_or_default();
        let wire = match env.t.as_str() {
            "auth_failed" => Wire { room: String::new(), probe: String::new(), code: b64(&code::seal(&self.code_key, &plain)) },
            "join" => Wire {
                room: b64(&code::seal(&self.room_key, &plain)),
                probe: b64(&code::seal(&self.code_key, env.nonce.as_bytes())),
                code: String::new(),
            },
            _ => Wire { room: b64(&code::seal(&self.room_key, &plain)), probe: String::new(), code: String::new() },
        };
        serde_json::to_string(&wire).unwrap_or_default()
    }

    /// Read event content. None for anything that isn't for this room, is too old, or was
    /// seen before.
    pub fn open(&self, content: &str) -> Option<Incoming> {
        let wire: Wire = serde_json::from_str(content).ok()?;
        if !wire.room.is_empty() {
            match code::open(&self.room_key, &unb64(&wire.room)?) {
                Some(plain) => {
                    let env: Envelope = serde_json::from_slice(&plain).ok()?;
                    return self.fresh(&env.nonce, env.ts).then_some(Incoming::Message(env));
                }
                None if !wire.probe.is_empty() => {
                    let nonce = String::from_utf8(code::open(&self.code_key, &unb64(&wire.probe)?)?).ok()?;
                    return self.fresh(&format!("probe:{nonce}"), now()).then_some(Incoming::WrongPasswordJoin { nonce });
                }
                None => return None,
            }
        }
        if !wire.code.is_empty() {
            let env: Envelope = serde_json::from_slice(&code::open(&self.code_key, &unb64(&wire.code)?)?).ok()?;
            if env.t == "auth_failed" && self.fresh(&env.nonce, env.ts) {
                return Some(Incoming::AuthFailed { to: env.to });
            }
        }
        None
    }

    fn fresh(&self, nonce: &str, ts: i64) -> bool {
        if (now() - ts).abs() > MAX_AGE_SECS {
            return false;
        }
        let mut seen = self.seen.lock().unwrap();
        if !seen.0.insert(nonce.to_string()) {
            return false;
        }
        seen.1.push_back(nonce.to_string());
        if seen.1.len() > 2000 {
            if let Some(old) = seen.1.pop_front() {
                seen.0.remove(&old);
            }
        }
        true
    }
}

/// Connection to the relays for one room.
#[derive(Clone)]
pub struct Signaling {
    room: Arc<Room>,
    keys: Keys,
    out: broadcast::Sender<String>,
    connected: Arc<Mutex<HashSet<String>>>,
}

impl Signaling {
    /// Connect to all relays (in the background) and deliver what arrives to `incoming`.
    pub fn start(room: Room, relays: Vec<String>, incoming: mpsc::UnboundedSender<Incoming>) -> Signaling {
        // Several TLS backends are compiled in (WebRTC and HTTP pull in different ones), so
        // rustls needs one picked for the relay websockets.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let room = Arc::new(room);
        let (out, _) = broadcast::channel(64);
        let connected = Arc::new(Mutex::new(HashSet::new()));
        let seen_events = Arc::new(Mutex::new(HashSet::<String>::new()));
        for url in relays {
            let (room, rx, incoming, connected, seen) =
                (room.clone(), out.subscribe(), incoming.clone(), connected.clone(), seen_events.clone());
            tokio::spawn(relay_task(url, room, rx, incoming, connected, seen));
        }
        Signaling { room, keys: Keys::generate(), out, connected }
    }

    pub fn send(&self, env: &Envelope) {
        let content = self.room.seal(env);
        let event = EventBuilder::new(Kind::Custom(EVENT_KIND), content)
            .tag(Tag::custom("x", [self.room.topic.clone()]))
            .finalize(&self.keys);
        match event {
            Ok(e) => {
                let _ = self.out.send(format!(r#"["EVENT",{}]"#, e.as_json()));
            }
            Err(e) => log::warn!("party: can't sign signaling event: {e}"),
        }
    }

    /// Relays currently connected.
    pub fn connected(&self) -> usize {
        self.connected.lock().unwrap().len()
    }
}

async fn relay_task(
    url: String,
    room: Arc<Room>,
    mut out: broadcast::Receiver<String>,
    incoming: mpsc::UnboundedSender<Incoming>,
    connected: Arc<Mutex<HashSet<String>>>,
    seen: Arc<Mutex<HashSet<String>>>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let mut backoff = 2;
    loop {
        if incoming.is_closed() {
            return;
        }
        let conn = tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(url.as_str())).await;
        let Ok(Ok((mut ws, _))) = conn else {
            log::info!("party: relay {url} not reachable, retrying in {backoff}s");
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(30);
            continue;
        };
        backoff = 2;
        let sub = format!("trax{:08x}", rand::random::<u32>());
        let req = serde_json::json!(["REQ", sub, {"kinds": [EVENT_KIND], "#x": [room.topic], "since": now() - 30}]);
        if ws.send(Message::text(req.to_string())).await.is_err() {
            continue;
        }
        connected.lock().unwrap().insert(url.clone());
        loop {
            tokio::select! {
                msg = out.recv() => match msg {
                    Ok(text) => {
                        if ws.send(Message::text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        connected.lock().unwrap().remove(&url);
                        let _ = ws.close(None).await;
                        return;
                    }
                },
                msg = ws.next() => match msg {
                    Some(Ok(Message::Text(text))) => handle_relay_message(&text, &sub, &room, &incoming, &seen),
                    Some(Ok(Message::Ping(p))) => {
                        let _ = ws.send(Message::Pong(p)).await;
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                },
            }
        }
        connected.lock().unwrap().remove(&url);
        tokio::time::sleep(Duration::from_secs(backoff)).await;
    }
}

fn handle_relay_message(
    text: &str,
    sub: &str,
    room: &Room,
    incoming: &mpsc::UnboundedSender<Incoming>,
    seen: &Mutex<HashSet<String>>,
) {
    let Ok(serde_json::Value::Array(parts)) = serde_json::from_str::<serde_json::Value>(text) else { return };
    if parts.first().and_then(|v| v.as_str()) != Some("EVENT") || parts.get(1).and_then(|v| v.as_str()) != Some(sub) {
        return;
    }
    let Some(raw) = parts.get(2) else { return };
    let Ok(event) = Event::from_json(raw.to_string()) else { return };
    // The same event arrives once per relay.
    if !seen.lock().unwrap().insert(event.id.to_hex()) {
        return;
    }
    if event.kind != Kind::Custom(EVENT_KIND) || event.verify().is_err() {
        return;
    }
    let tagged = event.tags.iter().any(|t| t.as_slice().first().map(String::as_str) == Some("x")
        && t.as_slice().get(1) == Some(&room.topic));
    if !tagged {
        return;
    }
    if let Some(msg) = room.open(&event.content) {
        let _ = incoming.send(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_messages_round_trip_once() {
        let room = Room::new("XYASKDHGASDF", "pw");
        let env = Envelope::new("offer", "host", "guest", "v=0...");
        let sealed = room.seal(&env);
        assert_eq!(room.open(&sealed), Some(Incoming::Message(env.clone())));
        // Replays are dropped.
        assert_eq!(room.open(&sealed), None);
        // Other rooms can't read it.
        assert_eq!(Room::new("XYASKDHGASDG", "pw").open(&sealed), None);
    }

    #[test]
    fn wrong_password_is_reported() {
        let host = Room::new("XYASKDHGASDF", "right");
        let guest = Room::new("XYASKDHGASDF", "wrong");
        let join = Envelope::new("join", "g1", "", "");
        // The host can't read the join, but learns its nonce from the probe.
        assert_eq!(host.open(&guest.seal(&join)), Some(Incoming::WrongPasswordJoin { nonce: join.nonce.clone() }));
        // The host's answer can be read by the guest with only the code.
        let reply = Envelope::new("auth_failed", "", &join.nonce, "");
        assert_eq!(guest.open(&host.seal(&reply)), Some(Incoming::AuthFailed { to: join.nonce }));
        // No password and an empty password are the same room.
        let a = Room::new("XYASKDHGASDF", "");
        let msg = Envelope::new("answer", "g", "h", "sdp");
        assert!(Room::new("XYASKDHGASDF", "").open(&a.seal(&msg)).is_some());
    }

    /// Uses the real public relays: `cargo test relays_deliver -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn relays_deliver() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let code = code::generate();
            let (tx_a, _rx_a) = mpsc::unbounded_channel();
            let (tx_b, mut rx_b) = mpsc::unbounded_channel();
            let relays: Vec<String> = DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect();
            let a = Signaling::start(Room::new(&code, ""), relays.clone(), tx_a);
            let b = Signaling::start(Room::new(&code, ""), relays, tx_b);
            tokio::time::sleep(Duration::from_secs(4)).await;
            println!("connected relays: a={} b={}", a.connected(), b.connected());
            let env = Envelope::new("offer", "a", "b", "hello");
            a.send(&env);
            let got = tokio::time::timeout(Duration::from_secs(10), rx_b.recv()).await;
            println!("received: {got:?}");
            assert_eq!(got.ok().flatten(), Some(Incoming::Message(env)));
        });
    }

    #[test]
    fn old_messages_are_dropped() {
        let room = Room::new("XYASKDHGASDF", "");
        let mut env = Envelope::new("offer", "a", "b", "");
        env.ts -= MAX_AGE_SECS + 5;
        assert_eq!(room.open(&room.seal(&env)), None);
    }
}
