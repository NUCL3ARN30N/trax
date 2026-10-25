//! A guest's view of the party, kept in step with the host.
//!
//! Pure state, no IO. Host messages come in through [`GuestRoom::on_message`]; `APPLIED`
//! messages are applied strictly in `seq` order (out-of-order ones wait up to 300 ms, then a
//! resync is asked for). The result is a list of [`GuestOut`] instructions the App carries
//! out with its "remote apply" path, which never sends requests (no echoes).
//!
//! Own requests are tracked as pending until the host answers. The App may show their effect
//! early (optimistic UI), but the room state here only changes from host messages.

use super::clock::{self, Clock};
use super::protocol::*;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

const GAP_WAIT: Duration = Duration::from_millis(300);
const PING_EVERY: Duration = Duration::from_secs(5);
const PENDING_TIMEOUT: Duration = Duration::from_secs(10);

/// What the App should do.
#[derive(Debug, Clone, PartialEq)]
pub enum GuestOut {
    Send(GuestMsg),
    /// Play this track from `position_ms` (a new track instance).
    LoadTrack { track: TrackRef, instance: String, position_ms: i64, playing: bool },
    /// Nothing is playing on the host.
    Stop,
    SetPlaying { playing: bool, position_ms: i64 },
    Seek(i64),
    /// A heartbeat arrived: compare the local position with `expected_position` and correct.
    CheckSync,
    QueueChanged,
    PeersChanged,
    CapsChanged,
    /// Undo optimistic changes: re-apply the room's real state.
    Reconcile,
    Toast(String),
    RequestDone(String),
    Kicked,
    Ended,
}

#[derive(Debug, Clone)]
struct Pending {
    op: RequestOp,
    sent: Instant,
}

pub struct GuestRoom {
    pub me: PeerId,
    /// Proof of our trax profile, sent with HELLO (empty without friends).
    pub profile_proof: String,
    next_seq: Option<u64>,
    buffer: BTreeMap<u64, HostMsg>,
    gap_since: Option<Instant>,
    pub acl_version: u64,
    pub caps: Vec<Cap>,
    /// Caps from before a reconnect, to notice when they were lost.
    had_caps: bool,
    pub peers: Vec<PeerInfo>,
    pub queue: Vec<QueueEntry>,
    pub current: Option<CurrentTrack>,
    /// Last heartbeat: (position, host time, playing).
    heartbeat: Option<(i64, i64, bool)>,
    pub clock: Clock,
    pending: HashMap<String, Pending>,
    last_ping: Option<Instant>,
}

impl GuestRoom {
    pub fn new(me: PeerId) -> GuestRoom {
        GuestRoom {
            me,
            profile_proof: String::new(),
            next_seq: None,
            buffer: BTreeMap::new(),
            gap_since: None,
            acl_version: 0,
            caps: vec![],
            had_caps: false,
            peers: vec![],
            queue: vec![],
            current: None,
            heartbeat: None,
            clock: Clock::default(),
            pending: HashMap::new(),
            last_ping: None,
        }
    }

    /// First messages on a (re)connected channel.
    pub fn hello(&mut self, name: &str) -> Vec<GuestOut> {
        // After a reconnect the snapshot decides what's true again.
        self.next_seq = None;
        self.buffer.clear();
        vec![
            GuestOut::Send(GuestMsg::Hello {
                name: name.into(),
                peer_id: self.me.clone(),
                protocol_version: PROTOCOL_VERSION,
                profile: self.profile_proof.clone(),
            }),
            GuestOut::Send(GuestMsg::ResyncRequest),
        ]
    }

    pub fn can(&self, cap: Cap) -> bool {
        self.caps.contains(&cap)
    }

    pub fn host_name(&self) -> String {
        self.peers.iter().find(|p| p.role == Role::Host).map(|p| p.name.clone()).unwrap_or_default()
    }

    /// Whether a request of this kind is waiting for the host (for pending spinners).
    pub fn is_pending(&self, cap: Cap) -> bool {
        self.pending.values().any(|p| p.op.cap() == cap && !matches!(p.op, RequestOp::QueueAdd { .. } | RequestOp::QueueRemove { .. }))
    }

    pub fn pending_removals(&self) -> Vec<String> {
        self.pending
            .values()
            .filter_map(|p| match &p.op {
                RequestOp::QueueRemove { entry_id } => Some(entry_id.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn pending_adds(&self) -> Vec<TrackRef> {
        self.pending
            .values()
            .filter_map(|p| match &p.op {
                RequestOp::QueueAdd { track, .. } => Some(track.clone()),
                _ => None,
            })
            .collect()
    }

    /// A transport request for the current track, if allowed. Returns the message to send.
    pub fn request(&mut self, op: RequestOp, now: Instant) -> Option<GuestMsg> {
        if !self.can(op.cap()) {
            return None;
        }
        let op_id = uuid::Uuid::new_v4().to_string();
        self.pending.insert(op_id.clone(), Pending { op: op.clone(), sent: now });
        Some(GuestMsg::Request { op_id, acl_version: self.acl_version, op })
    }

    /// The current track instance (requests refer to it).
    pub fn instance(&self) -> Option<String> {
        self.current.as_ref().map(|c| c.track_instance_id.clone())
    }

    /// Where the host is in the current track right now (ms), from the last heartbeat.
    pub fn expected_position(&self, local_now_ms: i64) -> Option<i64> {
        let (pos, host_time, playing) = self.heartbeat?;
        let host_now = self.clock.host_now(local_now_ms)?;
        Some(clock::expected_position(pos, host_time, host_now, playing))
    }

    // ------------------------------------------------------------------ incoming

    pub fn on_message(&mut self, msg: HostMsg, now: Instant, local_ms: i64) -> Vec<GuestOut> {
        match msg {
            HostMsg::RoomSnapshot { .. } => self.apply_snapshot(msg),
            HostMsg::Applied { seq, .. } => {
                let Some(next) = self.next_seq else {
                    // No snapshot yet: keep it until the snapshot says where we are.
                    self.buffer.insert(seq, msg);
                    return vec![];
                };
                if seq < next {
                    return vec![];
                }
                self.buffer.insert(seq, msg);
                let out = self.drain();
                if !self.buffer.is_empty() && self.gap_since.is_none() {
                    self.gap_since = Some(now);
                }
                out
            }
            HostMsg::AuthorityUpdate { acl_version, peer_id, caps, .. } => {
                if acl_version <= self.acl_version {
                    return vec![];
                }
                self.acl_version = acl_version;
                if let Some(p) = self.peers.iter_mut().find(|p| p.peer_id == peer_id) {
                    p.caps = caps.clone();
                }
                let mut out = vec![GuestOut::PeersChanged];
                if peer_id == self.me {
                    let gained = caps.iter().any(|c| !self.caps.contains(c));
                    self.caps = caps;
                    self.had_caps = !self.caps.is_empty();
                    out.push(GuestOut::CapsChanged);
                    out.push(GuestOut::Toast(if gained {
                        "The host gave you more control".into()
                    } else {
                        "The host changed what you can control".into()
                    }));
                }
                out
            }
            HostMsg::SyncHeartbeat { position_ms, host_time, track_instance_id, playing, seq } => {
                // Nothing playing on either side: nothing to sync.
                if track_instance_id.is_empty() && self.current.is_none() {
                    return vec![];
                }
                // The host moved on to something this guest hasn't seen: resync.
                if self.next_seq.is_some_and(|n| seq >= n) || self.instance().as_deref() != Some(track_instance_id.as_str())
                {
                    if self.gap_since.is_none() {
                        self.gap_since = Some(now - GAP_WAIT);
                    }
                    return vec![];
                }
                self.heartbeat = Some((position_ms, host_time, playing));
                match self.clock.host_now(local_ms) {
                    Some(_) => vec![GuestOut::CheckSync],
                    None => vec![],
                }
            }
            HostMsg::Rejected { op_id, reason } => match self.pending.remove(&op_id) {
                Some(_) => vec![GuestOut::Reconcile, GuestOut::Toast(reason.message().into()), GuestOut::RequestDone(op_id)],
                None => vec![],
            },
            HostMsg::Pong { t0, host_time } => {
                self.clock.pong(t0, host_time, local_ms);
                vec![]
            }
            HostMsg::Kicked => vec![GuestOut::Kicked],
            HostMsg::PartyEnded => vec![GuestOut::Ended],
        }
    }

    /// Periodic work: resync after a gap, pings for the clock, giving up on lost requests.
    pub fn tick(&mut self, now: Instant, local_ms: i64) -> Vec<GuestOut> {
        let mut out = vec![];
        if self.gap_since.is_some_and(|t| now.duration_since(t) >= GAP_WAIT) {
            self.gap_since = None;
            out.push(GuestOut::Send(GuestMsg::ResyncRequest));
        }
        if self.last_ping.is_none_or(|t| now.duration_since(t) >= PING_EVERY) {
            self.last_ping = Some(now);
            out.push(GuestOut::Send(GuestMsg::Ping { t0: local_ms }));
        }
        let expired: Vec<String> =
            self.pending.iter().filter(|(_, p)| now.duration_since(p.sent) > PENDING_TIMEOUT).map(|(k, _)| k.clone()).collect();
        if !expired.is_empty() {
            for id in &expired {
                self.pending.remove(id);
                out.push(GuestOut::RequestDone(id.clone()));
            }
            out.push(GuestOut::Reconcile);
            out.push(GuestOut::Toast("The host didn't answer".into()));
        }
        out
    }

    // ------------------------------------------------------------------ applying

    fn apply_snapshot(&mut self, msg: HostMsg) -> Vec<GuestOut> {
        let HostMsg::RoomSnapshot { seq, acl_version, acl, queue, current_track, peers } = msg else { return vec![] };
        let mut out = vec![];
        self.next_seq = Some(seq + 1);
        self.buffer.retain(|s, _| *s > seq);
        self.gap_since = None;
        self.acl_version = acl_version;
        let caps = acl.into_iter().find(|a| a.peer_id == self.me).map(|a| a.caps).unwrap_or_default();
        if caps.is_empty() && self.had_caps {
            out.push(GuestOut::Toast("Permissions were reset — ask the host".into()));
        }
        if caps != self.caps {
            out.push(GuestOut::CapsChanged);
        }
        self.had_caps = !caps.is_empty();
        self.caps = caps;
        self.peers = peers;
        self.queue = queue;
        out.push(GuestOut::PeersChanged);
        out.push(GuestOut::QueueChanged);
        let same_instance = self.instance() == current_track.as_ref().map(|c| c.track_instance_id.clone());
        match &current_track {
            Some(c) if !same_instance => out.push(GuestOut::LoadTrack {
                track: c.track.clone(),
                instance: c.track_instance_id.clone(),
                position_ms: c.position_ms,
                playing: c.playing,
            }),
            // Same track: only the play state; small timing differences are left to the drift
            // correction (a snapshot is often just about who joined or left).
            Some(c) => out.push(GuestOut::SetPlaying { playing: c.playing, position_ms: c.position_ms }),
            None if self.current.is_some() => out.push(GuestOut::Stop),
            None => {}
        }
        if current_track.is_none() {
            self.heartbeat = None;
        }
        self.current = current_track;
        out.extend(self.drain());
        out
    }

    /// Apply buffered `APPLIED` messages that are next in line.
    fn drain(&mut self) -> Vec<GuestOut> {
        let mut out = vec![];
        while let Some(next) = self.next_seq {
            let Some(msg) = self.buffer.remove(&next) else { break };
            self.next_seq = Some(next + 1);
            out.extend(self.apply(msg));
        }
        if self.buffer.is_empty() {
            self.gap_since = None;
        }
        out
    }

    fn apply(&mut self, msg: HostMsg) -> Vec<GuestOut> {
        let HostMsg::Applied {
            op_id, op, by, by_name, track_instance_id, position_ms, track, playing, entry, index, entry_id, order, ..
        } = msg
        else {
            return vec![];
        };
        let mine = by == self.me;
        let mut out = vec![];
        if !op_id.is_empty() && self.pending.remove(&op_id).is_some() {
            out.push(GuestOut::RequestDone(op_id));
        }
        let who = if by_name.is_empty() { "someone".to_string() } else { by_name };
        match op {
            AppliedOp::Skip | AppliedOp::TrackChange => {
                if let Some(id) = entry_id {
                    self.queue.retain(|e| e.entry_id != id);
                    out.push(GuestOut::QueueChanged);
                }
                if let Some(track) = track {
                    let playing = playing.unwrap_or(true);
                    self.current = Some(CurrentTrack {
                        track: track.clone(),
                        track_instance_id: track_instance_id.clone(),
                        position_ms,
                        playing,
                    });
                    // A fresh track: the old heartbeat no longer applies.
                    self.heartbeat = None;
                    out.push(GuestOut::LoadTrack { track, instance: track_instance_id, position_ms, playing });
                }
                if op == AppliedOp::Skip && !mine {
                    out.push(GuestOut::Toast(format!("Skipped by {who}")));
                }
            }
            AppliedOp::Pause | AppliedOp::Play => {
                let playing = op == AppliedOp::Play;
                if let Some(c) = self.current.as_mut() {
                    c.playing = playing;
                    c.position_ms = position_ms;
                }
                if let Some(hb) = self.heartbeat.as_mut() {
                    hb.2 = playing;
                }
                out.push(GuestOut::SetPlaying { playing, position_ms });
                if !mine {
                    out.push(GuestOut::Toast(format!("{} by {who}", if playing { "Resumed" } else { "Paused" })));
                }
            }
            AppliedOp::Seek => {
                if let Some(c) = self.current.as_mut() {
                    c.position_ms = position_ms;
                }
                self.heartbeat = None;
                out.push(GuestOut::Seek(position_ms));
            }
            AppliedOp::QueueAdd => {
                if let Some(e) = entry {
                    let i = index.unwrap_or(self.queue.len()).min(self.queue.len());
                    if !mine {
                        out.push(GuestOut::Toast(format!("{} added “{}”", e.added_by_name, e.track.title)));
                    }
                    self.queue.insert(i, e);
                    out.push(GuestOut::QueueChanged);
                }
            }
            AppliedOp::QueueRemove => {
                if let Some(id) = entry_id {
                    self.queue.retain(|e| e.entry_id != id);
                    out.push(GuestOut::QueueChanged);
                }
            }
            AppliedOp::QueueReorder => {
                if let Some(order) = order {
                    let mut pool = std::mem::take(&mut self.queue);
                    for id in order {
                        if let Some(i) = pool.iter().position(|e| e.entry_id == id) {
                            self.queue.push(pool.remove(i));
                        }
                    }
                    self.queue.extend(pool);
                    out.push(GuestOut::QueueChanged);
                }
            }
            AppliedOp::QueueClear => {
                self.queue.clear();
                out.push(GuestOut::QueueChanged);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yt(id: &str) -> TrackRef {
        TrackRef {
            track_id: format!("yt:{id}"),
            fingerprint: format!("yt:{id}"),
            title: id.into(),
            artist: String::new(),
            duration_ms: 1000,
            source: "youtube".into(),
        }
    }

    fn snapshot(seq: u64, caps: Vec<Cap>) -> HostMsg {
        HostMsg::RoomSnapshot {
            seq,
            acl_version: 3,
            acl: vec![AclEntry { peer_id: "me".into(), caps }],
            queue: vec![],
            current_track: Some(CurrentTrack {
                track: yt("a"),
                track_instance_id: "yt:a#1".into(),
                position_ms: 0,
                playing: true,
            }),
            peers: vec![],
        }
    }

    fn add(seq: u64, id: &str) -> HostMsg {
        let mut m = HostMsg::applied(seq, "", AppliedOp::QueueAdd, "host", "H");
        if let HostMsg::Applied { entry, .. } = &mut m {
            *entry = Some(QueueEntry {
                entry_id: id.into(),
                track: yt(id),
                added_by: "host".into(),
                added_by_name: "H".into(),
            });
        }
        m
    }

    fn ids(r: &GuestRoom) -> Vec<String> {
        r.queue.iter().map(|e| e.entry_id.clone()).collect()
    }

    #[test]
    fn applies_in_seq_order_and_resyncs_gaps() {
        let mut r = GuestRoom::new("me".into());
        let t = Instant::now();
        r.on_message(snapshot(10, vec![]), t, 0);
        // 12 before 11: waits.
        r.on_message(add(12, "b"), t, 0);
        assert!(ids(&r).is_empty());
        r.on_message(add(11, "a"), t, 0);
        assert_eq!(ids(&r), vec!["a", "b"]);
        // Duplicates are ignored.
        r.on_message(add(11, "a"), t, 0);
        assert_eq!(ids(&r), vec!["a", "b"]);
        // A gap that doesn't fill leads to a resync request.
        r.on_message(add(14, "d"), t, 0);
        assert!(!r.tick(t + Duration::from_millis(100), 0).contains(&GuestOut::Send(GuestMsg::ResyncRequest)));
        assert!(r.tick(t + Duration::from_millis(400), 0).contains(&GuestOut::Send(GuestMsg::ResyncRequest)));
    }

    #[test]
    fn messages_before_the_snapshot_wait() {
        let mut r = GuestRoom::new("me".into());
        let t = Instant::now();
        r.on_message(add(11, "a"), t, 0);
        r.on_message(add(10, "old"), t, 0);
        r.on_message(snapshot(10, vec![]), t, 0);
        assert_eq!(ids(&r), vec!["a"]);
    }

    #[test]
    fn rejected_request_rolls_back() {
        let mut r = GuestRoom::new("me".into());
        let t = Instant::now();
        r.on_message(snapshot(1, vec![Cap::Skip]), t, 0);
        let GuestMsg::Request { op_id, acl_version, .. } =
            r.request(RequestOp::Skip { track_instance_id: r.instance().unwrap() }, t).unwrap()
        else {
            panic!()
        };
        assert_eq!(acl_version, 3);
        assert!(r.is_pending(Cap::Skip));
        let out = r.on_message(HostMsg::Rejected { op_id: op_id.clone(), reason: RejectReason::StaleTrack }, t, 0);
        assert!(out.contains(&GuestOut::Reconcile));
        assert!(out.contains(&GuestOut::Toast("Already skipped".into())));
        assert!(!r.is_pending(Cap::Skip));
        // Without the capability no request is made at all.
        assert!(r.request(RequestOp::Seek { position_ms: 0, track_instance_id: "x".into() }, t).is_none());
    }

    #[test]
    fn own_applied_clears_pending_without_toast() {
        let mut r = GuestRoom::new("me".into());
        let t = Instant::now();
        r.on_message(snapshot(1, vec![Cap::Pause]), t, 0);
        let GuestMsg::Request { op_id, .. } =
            r.request(RequestOp::Pause { track_instance_id: r.instance().unwrap() }, t).unwrap()
        else {
            panic!()
        };
        let mut applied = HostMsg::applied(2, &op_id, AppliedOp::Pause, "me", "Me");
        if let HostMsg::Applied { position_ms, .. } = &mut applied {
            *position_ms = 1500;
        }
        let out = r.on_message(applied, t, 0);
        assert!(out.contains(&GuestOut::RequestDone(op_id)));
        assert!(out.contains(&GuestOut::SetPlaying { playing: false, position_ms: 1500 }));
        assert!(!out.iter().any(|o| matches!(o, GuestOut::Toast(_))));
        // Someone else's pause shows who did it, and never creates a request.
        let out = r.on_message(HostMsg::applied(3, "", AppliedOp::Play, "g2", "Gus"), t, 0);
        assert!(out.contains(&GuestOut::Toast("Resumed by Gus".into())));
        assert!(!out.iter().any(|o| matches!(o, GuestOut::Send(_))));
    }

    #[test]
    fn track_change_pops_the_queue_and_loads() {
        let mut r = GuestRoom::new("me".into());
        let t = Instant::now();
        r.on_message(snapshot(1, vec![]), t, 0);
        r.on_message(add(2, "b"), t, 0);
        let mut m = HostMsg::applied(3, "", AppliedOp::Skip, "g2", "Gus");
        if let HostMsg::Applied { track, track_instance_id, entry_id, playing, .. } = &mut m {
            *track = Some(yt("b"));
            *track_instance_id = "yt:b#2".into();
            *entry_id = Some("b".into());
            *playing = Some(true);
        }
        let out = r.on_message(m, t, 0);
        assert!(r.queue.is_empty());
        assert!(out.iter().any(|o| matches!(o, GuestOut::LoadTrack { instance, .. } if instance == "yt:b#2")));
        assert!(out.contains(&GuestOut::Toast("Skipped by Gus".into())));
    }

    #[test]
    fn lost_permissions_after_reconnect_are_noticed() {
        let mut r = GuestRoom::new("me".into());
        let t = Instant::now();
        r.on_message(snapshot(1, vec![Cap::Skip]), t, 0);
        r.hello("Me");
        let out = r.on_message(snapshot(5, vec![]), t, 0);
        assert!(out.contains(&GuestOut::Toast("Permissions were reset — ask the host".into())));
        assert!(r.caps.is_empty());
    }

    #[test]
    fn idle_heartbeat_with_nothing_playing_is_quiet() {
        let mut r = GuestRoom::new("me".into());
        let t = Instant::now();
        let mut snap = snapshot(1, vec![]);
        if let HostMsg::RoomSnapshot { current_track, .. } = &mut snap {
            *current_track = None;
        }
        r.on_message(snap, t, 0);
        r.on_message(
            HostMsg::SyncHeartbeat { seq: 1, host_time: 0, position_ms: 0, track_instance_id: String::new(), playing: false },
            t,
            0,
        );
        assert!(!r.tick(t + Duration::from_secs(1), 0).contains(&GuestOut::Send(GuestMsg::ResyncRequest)));
    }

    #[test]
    fn heartbeat_for_another_track_triggers_resync() {
        let mut r = GuestRoom::new("me".into());
        let t = Instant::now();
        r.on_message(snapshot(1, vec![]), t, 0);
        r.on_message(
            HostMsg::SyncHeartbeat { seq: 1, host_time: 0, position_ms: 0, track_instance_id: "yt:z#9".into(), playing: true },
            t,
            0,
        );
        assert!(r.tick(t, 0).contains(&GuestOut::Send(GuestMsg::ResyncRequest)));
    }
}
