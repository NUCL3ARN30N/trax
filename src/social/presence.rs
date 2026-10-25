//! What you're listening to, for your friends only.
//!
//! Presence is one replaceable Nostr event per profile (kind 30078, NIP-78 app data), so a
//! friend who comes online reads your current state straight away. The content is sealed
//! with your presence key, which only your friends have; removing a friend replaces the key.
//! The `d` tag is a hash, so relays can't tell it's trax.

use crate::party::code;
use crate::party::protocol::TrackRef;
use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, PublicKey, Tag};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const KIND: u16 = 30078;
/// Presence is republished this often while trax runs.
pub const HEARTBEAT_SECS: i64 = 120;
/// A friend whose presence is older than this counts as offline.
pub const ONLINE_SECS: i64 = 300;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Playing {
    pub title: String,
    #[serde(default)]
    pub artist: String,
    /// "local", "nextcloud", "youtube" or "soundcloud".
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub paused: bool,
    /// YouTube / SoundCloud songs only: lets friends play it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<TrackRef>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Presence {
    pub name: String,
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing: Option<Playing>,
    /// "hosting" or "guest" while in a listening party.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub party: Option<String>,
    pub updated: i64,
}

impl Presence {
    pub fn is_online(&self, now: i64) -> bool {
        self.online && now - self.updated < ONLINE_SECS
    }

    pub fn is_hosting(&self, now: i64) -> bool {
        self.is_online(now) && self.party.as_deref() == Some("hosting")
    }
}

pub fn new_key() -> [u8; 32] {
    rand::random()
}

pub fn key_to_string(key: &[u8; 32]) -> String {
    data_encoding::BASE64.encode(key)
}

pub fn key_from_string(s: &str) -> Option<[u8; 32]> {
    data_encoding::BASE64.decode(s.as_bytes()).ok()?.try_into().ok()
}

/// The `d` tag of a profile's presence event.
pub fn d_tag(pk: &PublicKey) -> String {
    let digest = Sha256::digest(format!("trax-presence/{}", pk.to_hex()));
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn event(keys: &Keys, key: &[u8; 32], presence: &Presence) -> anyhow::Result<Event> {
    let sealed = code::seal(key, &serde_json::to_vec(presence)?);
    EventBuilder::new(Kind::Custom(KIND), data_encoding::BASE64.encode(&sealed))
        .tag(Tag::identifier(d_tag(&keys.public_key())))
        .finalize(keys)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// A friend's presence, if `event` is one and `key` (theirs) opens it.
pub fn read(event: &Event, key: &[u8; 32]) -> Option<Presence> {
    if event.kind != Kind::Custom(KIND) {
        return None;
    }
    let sealed = data_encoding::BASE64.decode(event.content.as_bytes()).ok()?;
    let mut presence: Presence = serde_json::from_slice(&code::open(key, &sealed)?).ok()?;
    // The signed event time is what counts, not what the content claims.
    presence.updated = presence.updated.min(event.created_at.as_secs() as i64);
    Some(presence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_for_friends_only() {
        let alice = Keys::generate();
        let key = new_key();
        let now = crate::net::relays::now();
        let presence = Presence {
            name: "Alice".into(),
            online: true,
            playing: Some(Playing { title: "Song".into(), artist: "Band".into(), source: "local".into(), ..Default::default() }),
            party: Some("hosting".into()),
            updated: now,
        };
        let event = event(&alice, &key, &presence).unwrap();
        assert!(!event.content.contains("Song"));
        let read_back = read(&event, &key).unwrap();
        assert_eq!(read_back.name, "Alice");
        assert!(read_back.is_online(now));
        assert!(read_back.is_hosting(now));
        // A new key (after removing a friend) locks out the old one.
        assert_eq!(read(&super::event(&alice, &new_key(), &presence).unwrap(), &key), None);
        assert_eq!(key_from_string(&key_to_string(&key)), Some(key));
    }

    #[test]
    fn online_rule() {
        let p = Presence { online: true, updated: 1000, ..Default::default() };
        assert!(p.is_online(1000 + ONLINE_SECS - 1));
        assert!(!p.is_online(1000 + ONLINE_SECS));
        assert!(!Presence { online: false, updated: 1000, ..Default::default() }.is_online(1000));
    }
}
