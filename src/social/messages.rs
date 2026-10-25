//! Private messages between profiles, sent as NIP-59 gift wraps.
//!
//! A gift wrap is encrypted to the recipient (NIP-44) and signed with a throw-away key, so
//! relays see neither the sender nor the content, only that someone sent the recipient
//! something. Relays keep gift wraps, so a friend request reaches someone who is offline.
//! Invites and join requests are only valid for a few minutes.

use nostr::nips::nip59::{self, GiftWrapBuilder};
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, FinalizeUnsignedEvent, Keys, Kind, PublicKey};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};

/// Kind of the inner (unsigned) event. Only trax reads these.
pub const RUMOR_KIND: u16 = 14050;
/// Party invites, join requests and their answers expire after this.
pub const LIVE_SECS: i64 = 600;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Body {
    FriendRequest { name: String },
    FriendAccept { name: String },
    FriendDecline,
    FriendRemove,
    /// The key the sender's presence is sealed with (base64).
    PresenceKey { key: String },
    /// `asked`: the answer to the recipient's join request, so they join right away.
    PartyInvite { code: String, password: String, host: String, asked: bool },
    JoinRequest { name: String },
    JoinReply { accepted: bool },
}

impl Body {
    /// Party messages only make sense for a few minutes.
    pub fn is_live(&self) -> bool {
        matches!(self, Body::PartyInvite { .. } | Body::JoinRequest { .. } | Body::JoinReply { .. })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message {
    #[serde(flatten)]
    pub body: Body,
    pub nonce: String,
    pub ts: i64,
}

impl Message {
    pub fn new(body: Body) -> Message {
        Message { body, nonce: format!("{:032x}", rand::random::<u128>()), ts: crate::net::relays::now() }
    }
}

/// The gift wrap carrying `msg` to `to`.
pub fn wrap(keys: &Keys, to: &PublicKey, msg: &Message) -> anyhow::Result<Event> {
    let rumor = EventBuilder::new(Kind::Custom(RUMOR_KIND), serde_json::to_string(msg)?)
        .finalize_unsigned(keys.public_key());
    GiftWrapBuilder::new(*to, rumor).finalize(keys).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Sender and message of a gift wrap for us. None for anything else (other apps' gift
/// wraps, broken or forged ones).
pub fn unwrap(keys: &Keys, event: &Event) -> Option<(PublicKey, Message)> {
    if event.kind != Kind::GiftWrap {
        return None;
    }
    let gift = nip59::extract_rumor(keys, event).ok()?;
    if gift.rumor.kind != Kind::Custom(RUMOR_KIND) {
        return None;
    }
    let msg: Message = serde_json::from_str(&gift.rumor.content).ok()?;
    Some((gift.sender, msg))
}

/// Drops repeats (the same message again after a restart, or from a second relay) and
/// party messages that are too old. Gift wrap ids already handled are also kept in the
/// database, so they stay handled across restarts.
#[derive(Default)]
pub struct Replay {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl Replay {
    pub fn fresh(&mut self, msg: &Message, now: i64) -> bool {
        if msg.body.is_live() && (now - msg.ts).abs() > LIVE_SECS {
            return false;
        }
        if !self.seen.insert(msg.nonce.clone()) {
            return false;
        }
        self.order.push_back(msg.nonce.clone());
        if self.order.len() > 5000 {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gift_wraps_round_trip() {
        let (alice, bob, eve) = (Keys::generate(), Keys::generate(), Keys::generate());
        let msg = Message::new(Body::FriendRequest { name: "Alice".into() });
        let event = wrap(&alice, &bob.public_key(), &msg).unwrap();
        // Relays see a throw-away author, not Alice.
        assert_ne!(event.pubkey, alice.public_key());
        assert_eq!(unwrap(&bob, &event), Some((alice.public_key(), msg)));
        // Nobody else can open it.
        assert_eq!(unwrap(&eve, &event), None);
    }

    #[test]
    fn json_shape() {
        let msg = Message { body: Body::JoinReply { accepted: true }, nonce: "n".into(), ts: 5 };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json, serde_json::json!({"t": "join_reply", "accepted": true, "nonce": "n", "ts": 5}));
        assert_eq!(serde_json::from_value::<Message>(json).unwrap(), msg);
    }

    #[test]
    fn replays_and_expiry() {
        let mut replay = Replay::default();
        let now = crate::net::relays::now();
        let req = Message::new(Body::FriendRequest { name: "A".into() });
        assert!(replay.fresh(&req, now));
        assert!(!replay.fresh(&req, now));
        // Old friend requests still count (they wait for you while you're offline) ...
        let mut old = Message::new(Body::FriendRequest { name: "A".into() });
        old.ts -= 86_400;
        assert!(replay.fresh(&old, now));
        // ... old invites don't.
        let mut invite = Message::new(Body::PartyInvite {
            code: "X".into(),
            password: String::new(),
            host: "A".into(),
            asked: false,
        });
        invite.ts -= LIVE_SECS + 1;
        assert!(!replay.fresh(&invite, now));
    }
}
