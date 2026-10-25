//! The listening-party wire protocol: every message that travels over the DataChannel.
//!
//! All messages are small JSON objects with a `type` field. Guests send intents
//! ([`GuestMsg`]); the host is the only writer and answers with [`HostMsg`]. Mutating
//! requests carry an `opId` for idempotency and the `aclVersion` they were made under; track
//! operations carry the `trackInstanceId` (`<trackId>#<epoch>`) they refer to.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change; guests with another version are turned away.
pub const PROTOCOL_VERSION: u32 = 1;
/// Larger messages are dropped (`invalid`).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Party queue cap.
pub const MAX_QUEUE: usize = 100;
/// Guests per party.
pub const MAX_GUESTS: usize = 16;
/// Display names are cut to this many characters.
pub const MAX_NAME_CHARS: usize = 32;

/// A peer's stable id: base32 of its DTLS certificate's SHA-256 fingerprint.
pub type PeerId = String;

/// Something a guest may be allowed to do. `kick` and `manage_authority` are host-only and
/// deliberately not representable here.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    Skip,
    Seek,
    Pause,
    QueueAdd,
    QueueRemove,
    QueueMove,
}

impl Cap {
    pub const ALL: [Cap; 6] =
        [Cap::Skip, Cap::Seek, Cap::Pause, Cap::QueueAdd, Cap::QueueRemove, Cap::QueueMove];
}

/// A shareable track. Only YouTube and SoundCloud tracks can be heard by everyone.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TrackRef {
    /// Same as the fingerprint: `yt:<videoId>` or `sc:<permalink path>`; empty for a track
    /// only the host has (local / Nextcloud).
    pub track_id: String,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artist: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration_ms: i64,
    /// "youtube", "soundcloud", or "local" for host-only tracks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueueEntry {
    pub entry_id: String,
    pub track: TrackRef,
    pub added_by: PeerId,
    /// Copied in so the name stays visible after that peer leaves.
    pub added_by_name: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Guest,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub peer_id: PeerId,
    pub name: String,
    pub role: Role,
    pub online: bool,
    pub caps: Vec<Cap>,
    /// Their trax profile (public key, hex), if they proved it. Lets everyone see which
    /// member is which friend.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AclEntry {
    pub peer_id: PeerId,
    pub caps: Vec<Cap>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CurrentTrack {
    #[serde(flatten)]
    pub track: TrackRef,
    pub track_instance_id: String,
    pub position_ms: i64,
    pub playing: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AddMode {
    End,
    Next,
}

/// What a guest asks the host to do.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RequestOp {
    Skip {
        #[serde(rename = "trackInstanceId")]
        track_instance_id: String,
    },
    Seek {
        #[serde(rename = "positionMs")]
        position_ms: i64,
        #[serde(rename = "trackInstanceId")]
        track_instance_id: String,
    },
    Pause {
        #[serde(rename = "trackInstanceId")]
        track_instance_id: String,
    },
    Play {
        #[serde(rename = "trackInstanceId")]
        track_instance_id: String,
    },
    QueueAdd {
        track: TrackRef,
        mode: AddMode,
    },
    QueueRemove {
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    /// Move a song in the queue. It lands right after `after_entry_id`, or first when that
    /// is empty. Naming the neighbour instead of a position keeps the move on the right
    /// song when the queue changed in the meantime.
    QueueMove {
        #[serde(rename = "entryId")]
        entry_id: String,
        #[serde(rename = "afterEntryId", default)]
        after_entry_id: String,
    },
}

impl RequestOp {
    /// The capability a guest needs for this request.
    pub fn cap(&self) -> Cap {
        match self {
            RequestOp::Skip { .. } => Cap::Skip,
            RequestOp::Seek { .. } => Cap::Seek,
            RequestOp::Pause { .. } | RequestOp::Play { .. } => Cap::Pause,
            RequestOp::QueueAdd { .. } => Cap::QueueAdd,
            RequestOp::QueueRemove { .. } => Cap::QueueRemove,
            RequestOp::QueueMove { .. } => Cap::QueueMove,
        }
    }

    /// The track instance a transport request refers to.
    pub fn track_instance(&self) -> Option<&str> {
        match self {
            RequestOp::Skip { track_instance_id }
            | RequestOp::Seek { track_instance_id, .. }
            | RequestOp::Pause { track_instance_id }
            | RequestOp::Play { track_instance_id } => Some(track_instance_id),
            _ => None,
        }
    }
}

/// Guest → host.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GuestMsg {
    /// First message on every connection; sent again when the display name changes.
    #[serde(rename_all = "camelCase")]
    Hello {
        name: String,
        peer_id: PeerId,
        protocol_version: u32,
        /// Optional proof of the guest's trax profile (see `social::profile::party_proof`).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        profile: String,
    },
    #[serde(rename_all = "camelCase")]
    Request {
        op_id: String,
        acl_version: u64,
        #[serde(flatten)]
        op: RequestOp,
    },
    /// Clock sync: `t0` is the guest's own monotonic time in ms.
    Ping { t0: i64 },
    ResyncRequest,
}

/// The kinds of applied operations.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppliedOp {
    Skip,
    TrackChange,
    Pause,
    Play,
    Seek,
    QueueAdd,
    QueueRemove,
    QueueReorder,
    QueueClear,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityAction {
    Grant,
    Revoke,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    NoCap,
    StaleTrack,
    Cooldown,
    NotFoundLocally,
    AclStale,
    QueueFull,
    UnknownEntry,
    Invalid,
}

impl RejectReason {
    /// What a guest sees.
    pub fn message(self) -> &'static str {
        match self {
            RejectReason::NoCap | RejectReason::AclStale => "You don't have permission for that",
            RejectReason::StaleTrack => "Already skipped",
            RejectReason::Cooldown => "Slow down a little",
            RejectReason::NotFoundLocally => "The host can't play that song",
            RejectReason::QueueFull => "The queue is full",
            RejectReason::UnknownEntry => "That song is no longer in the queue",
            RejectReason::Invalid => "The host didn't understand that request",
        }
    }
}

/// Host → guests (all, or one).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HostMsg {
    #[serde(rename_all = "camelCase")]
    RoomSnapshot {
        seq: u64,
        acl_version: u64,
        acl: Vec<AclEntry>,
        queue: Vec<QueueEntry>,
        current_track: Option<CurrentTrack>,
        peers: Vec<PeerInfo>,
    },
    #[serde(rename_all = "camelCase")]
    Applied {
        seq: u64,
        /// The guest request this came from; empty for the host's own actions.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        op_id: String,
        op: AppliedOp,
        by: PeerId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        by_name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        track_instance_id: String,
        #[serde(default)]
        position_ms: i64,
        /// `track_change` / `skip`: the track now playing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<TrackRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        playing: Option<bool>,
        /// `queue_add`: the new entry and its index.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<QueueEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
        /// `queue_remove`: the removed entry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        /// `queue_reorder`: the full new order.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        order: Option<Vec<String>>,
    },
    #[serde(rename_all = "camelCase")]
    AuthorityUpdate { acl_version: u64, peer_id: PeerId, caps: Vec<Cap>, action: AuthorityAction },
    #[serde(rename_all = "camelCase")]
    SyncHeartbeat { seq: u64, host_time: i64, position_ms: i64, track_instance_id: String, playing: bool },
    #[serde(rename_all = "camelCase")]
    Rejected { op_id: String, reason: RejectReason },
    #[serde(rename_all = "camelCase")]
    Pong { t0: i64, host_time: i64 },
    Kicked,
    PartyEnded,
}

impl HostMsg {
    /// A bare `APPLIED` for `op`; callers fill in the op-specific fields.
    pub fn applied(seq: u64, op_id: &str, op: AppliedOp, by: &str, by_name: &str) -> HostMsg {
        HostMsg::Applied {
            seq,
            op_id: op_id.to_string(),
            op,
            by: by.to_string(),
            by_name: by_name.to_string(),
            track_instance_id: String::new(),
            position_ms: 0,
            track: None,
            playing: None,
            entry: None,
            index: None,
            entry_id: None,
            order: None,
        }
    }
}

pub fn encode<T: Serialize>(msg: &T) -> String {
    serde_json::to_string(msg).unwrap_or_default()
}

/// Parse a message, refusing anything larger than [`MAX_MESSAGE_BYTES`].
pub fn decode<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    if text.len() > MAX_MESSAGE_BYTES {
        return Err("message too large".into());
    }
    serde_json::from_str(text).map_err(|e| e.to_string())
}

/// Display names: trimmed, no control characters, at most [`MAX_NAME_CHARS`].
pub fn clean_name(name: &str) -> String {
    let n: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    let n = n.trim().to_string();
    if n.is_empty() { "Guest".into() } else { n }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn value<T: Serialize>(m: &T) -> Value {
        serde_json::from_str(&encode(m)).unwrap()
    }

    #[test]
    fn requests_match_the_spec_shapes() {
        let skip = GuestMsg::Request {
            op_id: "a".into(),
            acl_version: 7,
            op: RequestOp::Skip { track_instance_id: "t7#3".into() },
        };
        assert_eq!(
            value(&skip),
            json!({"type":"REQUEST","opId":"a","aclVersion":7,"op":"skip","trackInstanceId":"t7#3"})
        );
        let seek: GuestMsg = decode(
            r#"{"type":"REQUEST","opId":"b","aclVersion":1,"op":"seek","positionMs":12000,"trackInstanceId":"t7#3"}"#,
        )
        .unwrap();
        assert_eq!(
            seek,
            GuestMsg::Request {
                op_id: "b".into(),
                acl_version: 1,
                op: RequestOp::Seek { position_ms: 12000, track_instance_id: "t7#3".into() }
            }
        );
        let add: GuestMsg = decode(
            r#"{"type":"REQUEST","opId":"c","aclVersion":1,"op":"queue_add","track":{"trackId":"yt:x","fingerprint":"yt:x"},"mode":"next"}"#,
        )
        .unwrap();
        assert!(matches!(add, GuestMsg::Request { op: RequestOp::QueueAdd { mode: AddMode::Next, .. }, .. }));
        let remove: GuestMsg =
            decode(r#"{"type":"REQUEST","opId":"d","aclVersion":1,"op":"queue_remove","entryId":"e1"}"#).unwrap();
        assert!(matches!(remove, GuestMsg::Request { op: RequestOp::QueueRemove { .. }, .. }));
        assert_eq!(value(&GuestMsg::ResyncRequest), json!({"type":"RESYNC_REQUEST"}));
        assert_eq!(value(&GuestMsg::Ping { t0: 5 }), json!({"type":"PING","t0":5}));
    }

    #[test]
    fn host_messages_match_the_spec_shapes() {
        let mut applied = HostMsg::applied(1043, "op1", AppliedOp::Skip, "peerA", "Alex");
        if let HostMsg::Applied { track_instance_id, .. } = &mut applied {
            *track_instance_id = "t7#4".into();
        }
        let v = value(&applied);
        assert_eq!(v["type"], "APPLIED");
        assert_eq!(v["seq"], 1043);
        assert_eq!(v["op"], "skip");
        assert_eq!(v["by"], "peerA");
        assert_eq!(v["trackInstanceId"], "t7#4");
        assert_eq!(v["positionMs"], 0);

        let auth = HostMsg::AuthorityUpdate {
            acl_version: 8,
            peer_id: "p".into(),
            caps: vec![Cap::Skip, Cap::QueueAdd],
            action: AuthorityAction::Grant,
        };
        assert_eq!(
            value(&auth),
            json!({"type":"AUTHORITY_UPDATE","aclVersion":8,"peerId":"p","caps":["skip","queue_add"],"action":"grant"})
        );
        let hb = HostMsg::SyncHeartbeat {
            seq: 1044,
            host_time: 1730000000123,
            position_ms: 41000,
            track_instance_id: "t7#4".into(),
            playing: true,
        };
        assert_eq!(
            value(&hb),
            json!({"type":"SYNC_HEARTBEAT","seq":1044,"hostTime":1730000000123i64,"positionMs":41000,"trackInstanceId":"t7#4","playing":true})
        );
        let rej = HostMsg::Rejected { op_id: "x".into(), reason: RejectReason::AclStale };
        assert_eq!(value(&rej), json!({"type":"REJECTED","opId":"x","reason":"acl_stale"}));

        let snap = HostMsg::RoomSnapshot {
            seq: 1042,
            acl_version: 7,
            acl: vec![],
            queue: vec![],
            current_track: Some(CurrentTrack {
                track: TrackRef {
                    track_id: "yt:a".into(),
                    fingerprint: "yt:a".into(),
                    title: "Song".into(),
                    artist: String::new(),
                    duration_ms: 0,
                    source: "youtube".into(),
                },
                track_instance_id: "yt:a#4".into(),
                position_ms: 0,
                playing: true,
            }),
            peers: vec![],
        };
        let v = value(&snap);
        assert_eq!(v["type"], "ROOM_SNAPSHOT");
        assert_eq!(v["currentTrack"]["trackId"], "yt:a");
        assert_eq!(v["currentTrack"]["trackInstanceId"], "yt:a#4");
        assert_eq!(decode::<HostMsg>(&encode(&snap)).unwrap(), snap);
    }

    #[test]
    fn rejects_oversized_and_unknown() {
        let big = format!(r#"{{"type":"PING","t0":1,"pad":"{}"}}"#, "x".repeat(MAX_MESSAGE_BYTES));
        assert!(decode::<GuestMsg>(&big).is_err());
        assert!(decode::<GuestMsg>(r#"{"type":"REQUEST","opId":"a","aclVersion":1,"op":"kick"}"#).is_err());
        assert!(decode::<GuestMsg>("not json").is_err());
    }

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Al\u{7}ex  "), "Alex");
        assert_eq!(clean_name(""), "Guest");
        assert_eq!(clean_name(&"x".repeat(50)).chars().count(), MAX_NAME_CHARS);
    }
}
