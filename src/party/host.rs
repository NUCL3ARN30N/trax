//! The host's side of a party: who is there, who may do what, and the order of events.
//!
//! Pure state, no IO. Guest requests are validated here and turned into a [`Effect`] the App
//! carries out with its normal player functions; the App then reports what really happened
//! (`track_started`, `play_state`, `seeked`, `queue_changed`) and this module broadcasts it as
//! `APPLIED` with the next `seq`. The host is the only writer, so everyone sees one order.

use super::protocol::*;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

const SKIP_COOLDOWN: Duration = Duration::from_millis(2500);
const RATE_WINDOW: Duration = Duration::from_secs(10);
const RATE_MAX: usize = 20;
const ADD_WINDOW: Duration = Duration::from_secs(30);
const ADD_MAX: usize = 5;
const DONE_OPS: usize = 500;

/// Where a message goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    All(HostMsg),
    To(PeerId, HostMsg),
    Disconnect(PeerId),
}

/// What the App should do for an accepted request.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Skip,
    Pause,
    Play,
    Seek(i64),
    QueueAdd { track: TrackRef, next: bool },
    /// Index in the upcoming queue.
    QueueRemove(usize),
}

#[derive(Debug, Clone)]
struct Peer {
    id: PeerId,
    name: String,
    online: bool,
    kicked: bool,
}

#[derive(Debug, Clone)]
struct Current {
    track: TrackRef,
    instance: String,
    playing: bool,
}

/// The guest request being carried out right now.
#[derive(Debug, Clone)]
struct Pending {
    op_id: String,
    by: PeerId,
    kind: AppliedOp,
    used: bool,
}

pub struct HostRoom {
    pub me: PeerId,
    pub my_name: String,
    seq: u64,
    acl_version: u64,
    acl: HashMap<PeerId, BTreeSet<Cap>>,
    /// Guests in the order they first joined.
    peers: Vec<Peer>,
    epoch: u64,
    current: Option<Current>,
    entries: Vec<QueueEntry>,
    next_entry: u64,
    /// Latest playback position, kept up to date by the App.
    pub position_ms: i64,
    done: HashMap<String, (PeerId, HostMsg)>,
    done_order: VecDeque<String>,
    last_skip: HashMap<PeerId, Instant>,
    recent: HashMap<PeerId, VecDeque<Instant>>,
    recent_adds: HashMap<PeerId, VecDeque<Instant>>,
    pending: Option<Pending>,
}

/// Queue identity of a track: its fingerprint, or title + artist for host-only tracks.
pub fn track_key(t: &TrackRef) -> String {
    if t.fingerprint.is_empty() { format!("local:{}|{}", t.title, t.artist) } else { t.fingerprint.clone() }
}

impl HostRoom {
    pub fn new(me: PeerId, my_name: &str) -> HostRoom {
        HostRoom {
            me,
            my_name: clean_name(my_name),
            seq: 0,
            acl_version: 1,
            acl: HashMap::new(),
            peers: vec![],
            epoch: 0,
            current: None,
            entries: vec![],
            next_entry: 1,
            position_ms: 0,
            done: HashMap::new(),
            done_order: VecDeque::new(),
            last_skip: HashMap::new(),
            recent: HashMap::new(),
            recent_adds: HashMap::new(),
            pending: None,
        }
    }

    // ------------------------------------------------------------------ views

    fn name_of(&self, peer: &str) -> String {
        if peer == self.me {
            return self.my_name.clone();
        }
        self.peers.iter().find(|p| p.id == peer).map(|p| p.name.clone()).unwrap_or_else(|| "Guest".into())
    }

    pub fn caps_of(&self, peer: &str) -> Vec<Cap> {
        self.acl.get(peer).map(|c| c.iter().copied().collect()).unwrap_or_default()
    }

    /// Everyone in the party, host first.
    pub fn peer_list(&self) -> Vec<PeerInfo> {
        let mut list = vec![PeerInfo {
            peer_id: self.me.clone(),
            name: self.my_name.clone(),
            role: Role::Host,
            online: true,
            caps: Cap::ALL.to_vec(),
        }];
        list.extend(self.peers.iter().filter(|p| !p.kicked).map(|p| PeerInfo {
            peer_id: p.id.clone(),
            name: p.name.clone(),
            role: Role::Guest,
            online: p.online,
            caps: self.caps_of(&p.id),
        }));
        list
    }

    pub fn queue(&self) -> &[QueueEntry] {
        &self.entries
    }

    pub fn current_instance(&self) -> Option<&str> {
        self.current.as_ref().map(|c| c.instance.as_str())
    }

    pub fn snapshot(&self) -> HostMsg {
        let mut acl: Vec<AclEntry> =
            self.acl.iter().map(|(p, c)| AclEntry { peer_id: p.clone(), caps: c.iter().copied().collect() }).collect();
        acl.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        HostMsg::RoomSnapshot {
            seq: self.seq,
            acl_version: self.acl_version,
            acl,
            queue: self.entries.clone(),
            current_track: self.current.as_ref().map(|c| CurrentTrack {
                track: c.track.clone(),
                track_instance_id: c.instance.clone(),
                position_ms: self.position_ms,
                playing: c.playing,
            }),
            peers: self.peer_list(),
        }
    }

    pub fn heartbeat(&self, host_time: i64) -> Out {
        let (instance, playing) =
            self.current.as_ref().map(|c| (c.instance.clone(), c.playing)).unwrap_or_default();
        Out::All(HostMsg::SyncHeartbeat {
            seq: self.seq,
            host_time,
            position_ms: self.position_ms,
            track_instance_id: instance,
            playing,
        })
    }

    // ------------------------------------------------------------------ membership

    /// A guest's channel opened; it still has to say HELLO.
    pub fn guest_connected(&mut self, peer: &str) -> Vec<Out> {
        if self.peers.iter().any(|p| p.id == peer && p.kicked) {
            return vec![Out::To(peer.into(), HostMsg::Kicked), Out::Disconnect(peer.into())];
        }
        vec![]
    }

    pub fn guest_disconnected(&mut self, peer: &str) -> Vec<Out> {
        match self.peers.iter_mut().find(|p| p.id == peer) {
            Some(p) if p.online => {
                p.online = false;
                vec![Out::All(self.snapshot())]
            }
            _ => vec![],
        }
    }

    fn hello(&mut self, peer: &str, name: &str, version: u32) -> Vec<Out> {
        if version != PROTOCOL_VERSION {
            return vec![Out::Disconnect(peer.into())];
        }
        let name = clean_name(name);
        match self.peers.iter_mut().find(|p| p.id == peer) {
            Some(p) if p.kicked => return vec![Out::To(peer.into(), HostMsg::Kicked), Out::Disconnect(peer.into())],
            // A known guest reconnecting (or renaming) keeps its permissions.
            Some(p) => {
                p.name = name;
                p.online = true;
            }
            None => {
                if self.peers.iter().filter(|p| p.online).count() >= MAX_GUESTS {
                    return vec![Out::Disconnect(peer.into())];
                }
                self.peers.push(Peer { id: peer.into(), name, online: true, kicked: false });
            }
        }
        vec![Out::All(self.snapshot())]
    }

    fn known(&self, peer: &str) -> bool {
        self.peers.iter().any(|p| p.id == peer && p.online && !p.kicked)
    }

    // ------------------------------------------------------------------ authority

    fn set_caps(&mut self, peer: &str, caps: BTreeSet<Cap>, action: AuthorityAction) -> Vec<Out> {
        if !self.peers.iter().any(|p| p.id == peer && !p.kicked) {
            return vec![];
        }
        self.acl_version += 1;
        let list: Vec<Cap> = caps.iter().copied().collect();
        if caps.is_empty() {
            self.acl.remove(peer);
        } else {
            self.acl.insert(peer.into(), caps);
        }
        vec![Out::All(HostMsg::AuthorityUpdate { acl_version: self.acl_version, peer_id: peer.into(), caps: list, action })]
    }

    pub fn grant(&mut self, peer: &str, cap: Cap) -> Vec<Out> {
        let mut caps = self.acl.get(peer).cloned().unwrap_or_default();
        if !caps.insert(cap) {
            return vec![];
        }
        self.set_caps(peer, caps, AuthorityAction::Grant)
    }

    pub fn revoke(&mut self, peer: &str, cap: Cap) -> Vec<Out> {
        let mut caps = self.acl.get(peer).cloned().unwrap_or_default();
        if !caps.remove(&cap) {
            return vec![];
        }
        self.set_caps(peer, caps, AuthorityAction::Revoke)
    }

    pub fn grant_all(&mut self, peer: &str) -> Vec<Out> {
        self.set_caps(peer, Cap::ALL.into_iter().collect(), AuthorityAction::Grant)
    }

    pub fn revoke_all(&mut self, peer: &str) -> Vec<Out> {
        self.set_caps(peer, BTreeSet::new(), AuthorityAction::Revoke)
    }

    /// Remove a guest for the rest of the party.
    pub fn kick(&mut self, peer: &str) -> Vec<Out> {
        let Some(p) = self.peers.iter_mut().find(|p| p.id == peer) else { return vec![] };
        p.kicked = true;
        p.online = false;
        let mut out = vec![Out::To(peer.into(), HostMsg::Kicked), Out::Disconnect(peer.into())];
        if self.acl.remove(peer).is_some() {
            self.acl_version += 1;
            out.push(Out::All(HostMsg::AuthorityUpdate {
                acl_version: self.acl_version,
                peer_id: peer.into(),
                caps: vec![],
                action: AuthorityAction::Revoke,
            }));
        }
        out.push(Out::All(self.snapshot()));
        out
    }

    pub fn rename_self(&mut self, name: &str) -> Vec<Out> {
        self.my_name = clean_name(name);
        vec![Out::All(self.snapshot())]
    }

    // ------------------------------------------------------------------ guest messages

    /// Handle a guest message. An accepted request also returns the [`Effect`] to carry out;
    /// the App carries it out and then calls [`end_op`](Self::end_op).
    pub fn handle(
        &mut self,
        peer: &str,
        msg: GuestMsg,
        now: Instant,
        host_time: i64,
        can_play: &dyn Fn(&TrackRef) -> bool,
    ) -> (Vec<Out>, Option<(Effect, String)>) {
        match msg {
            GuestMsg::Hello { name, peer_id, protocol_version } => {
                // The transport already checked the DTLS identity; HELLO must agree with it.
                if peer_id != peer {
                    return (vec![Out::Disconnect(peer.into())], None);
                }
                (self.hello(peer, &name, protocol_version), None)
            }
            _ if !self.known(peer) => (vec![], None),
            GuestMsg::Ping { t0 } => (vec![Out::To(peer.into(), HostMsg::Pong { t0, host_time })], None),
            GuestMsg::ResyncRequest => (vec![Out::To(peer.into(), self.snapshot())], None),
            GuestMsg::Request { op_id, acl_version, op } => self.request(peer, op_id, acl_version, op, now, can_play),
        }
    }

    fn reject(&mut self, peer: &str, op_id: &str, reason: RejectReason) -> (Vec<Out>, Option<(Effect, String)>) {
        let msg = HostMsg::Rejected { op_id: op_id.into(), reason };
        self.remember(op_id, peer, msg.clone());
        (vec![Out::To(peer.into(), msg)], None)
    }

    fn remember(&mut self, op_id: &str, peer: &str, msg: HostMsg) {
        if op_id.is_empty() {
            return;
        }
        if self.done.insert(op_id.into(), (peer.into(), msg)).is_none() {
            self.done_order.push_back(op_id.into());
            if self.done_order.len() > DONE_OPS {
                if let Some(old) = self.done_order.pop_front() {
                    self.done.remove(&old);
                }
            }
        }
    }

    fn request(
        &mut self,
        peer: &str,
        op_id: String,
        acl_version: u64,
        op: RequestOp,
        now: Instant,
        can_play: &dyn Fn(&TrackRef) -> bool,
    ) -> (Vec<Out>, Option<(Effect, String)>) {
        if op_id.is_empty() || op_id.len() > 64 {
            return (vec![Out::To(peer.into(), HostMsg::Rejected { op_id, reason: RejectReason::Invalid })], None);
        }
        // Same request again (e.g. resent after a reconnect): same answer, nothing new happens.
        if let Some((by, msg)) = self.done.get(&op_id) {
            return if by == peer { (vec![Out::To(peer.into(), msg.clone())], None) } else { (vec![], None) };
        }
        // 1. capability
        if !self.acl.get(peer).is_some_and(|c| c.contains(&op.cap())) {
            return self.reject(peer, &op_id, RejectReason::NoCap);
        }
        // 2. ACL version the request was made under
        if acl_version != self.acl_version {
            return self.reject(peer, &op_id, RejectReason::AclStale);
        }
        // 3. the track it refers to is still playing
        if let Some(instance) = op.track_instance() {
            if self.current_instance() != Some(instance) {
                return self.reject(peer, &op_id, RejectReason::StaleTrack);
            }
        }
        // 4. cooldown / rate limit
        let recent = self.recent.entry(peer.into()).or_default();
        while recent.front().is_some_and(|t| now.duration_since(*t) > RATE_WINDOW) {
            recent.pop_front();
        }
        if recent.len() >= RATE_MAX {
            return self.reject(peer, &op_id, RejectReason::Cooldown);
        }
        if matches!(op, RequestOp::Skip { .. })
            && self.last_skip.get(peer).is_some_and(|t| now.duration_since(*t) < SKIP_COOLDOWN)
        {
            return self.reject(peer, &op_id, RejectReason::Cooldown);
        }
        if matches!(op, RequestOp::QueueAdd { .. }) {
            let adds = self.recent_adds.entry(peer.into()).or_default();
            while adds.front().is_some_and(|t| now.duration_since(*t) > ADD_WINDOW) {
                adds.pop_front();
            }
            if adds.len() >= ADD_MAX {
                return self.reject(peer, &op_id, RejectReason::Cooldown);
            }
        }
        // Op-specific checks, then hand over to the App.
        let (effect, kind) = match op {
            RequestOp::Skip { .. } => (Effect::Skip, AppliedOp::Skip),
            RequestOp::Pause { .. } => (Effect::Pause, AppliedOp::Pause),
            RequestOp::Play { .. } => (Effect::Play, AppliedOp::Play),
            RequestOp::Seek { position_ms, .. } => (Effect::Seek(position_ms.max(0)), AppliedOp::Seek),
            RequestOp::QueueAdd { track, mode } => {
                let shareable = track.fingerprint.starts_with("yt:") || track.fingerprint.starts_with("sc:");
                if !shareable || track.track_id != track.fingerprint || !can_play(&track) {
                    return self.reject(peer, &op_id, RejectReason::NotFoundLocally);
                }
                if self.entries.len() >= MAX_QUEUE {
                    return self.reject(peer, &op_id, RejectReason::QueueFull);
                }
                (Effect::QueueAdd { track, next: mode == AddMode::Next }, AppliedOp::QueueAdd)
            }
            RequestOp::QueueRemove { entry_id } => match self.entries.iter().position(|e| e.entry_id == entry_id) {
                Some(i) => (Effect::QueueRemove(i), AppliedOp::QueueRemove),
                None => return self.reject(peer, &op_id, RejectReason::UnknownEntry),
            },
        };
        self.recent.entry(peer.into()).or_default().push_back(now);
        if kind == AppliedOp::Skip {
            self.last_skip.insert(peer.into(), now);
        }
        if kind == AppliedOp::QueueAdd {
            self.recent_adds.entry(peer.into()).or_default().push_back(now);
        }
        self.pending = Some(Pending { op_id: op_id.clone(), by: peer.into(), kind, used: false });
        (vec![], Some((effect, op_id)))
    }

    // ------------------------------------------------------------------ what happened

    /// Who and which request an event belongs to: the pending request if it matches,
    /// otherwise the host itself.
    fn attribution(&mut self, kinds: &[AppliedOp]) -> (String, PeerId) {
        match self.pending.as_mut() {
            Some(p) if !p.used && kinds.contains(&p.kind) => {
                p.used = true;
                (p.op_id.clone(), p.by.clone())
            }
            _ => (String::new(), self.me.clone()),
        }
    }

    fn next_applied(&mut self, op: AppliedOp, op_id: &str, by: &str) -> HostMsg {
        self.seq += 1;
        let name = self.name_of(by);
        HostMsg::applied(self.seq, op_id, op, by, &name)
    }

    fn finish(&mut self, op_id: &str, by: &str, msg: HostMsg) -> Vec<Out> {
        if !op_id.is_empty() {
            self.remember(op_id, by, msg.clone());
        }
        vec![Out::All(msg)]
    }

    /// A track started. `skipped`: the user (host or a guest's skip) moved on, rather than the
    /// previous track ending.
    pub fn track_started(&mut self, track: TrackRef, position_ms: i64, playing: bool, skipped: bool) -> Vec<Out> {
        self.epoch += 1;
        let instance = format!("{}#{}", if track.track_id.is_empty() { "local" } else { &track.track_id }, self.epoch);
        // The track came off the head of the queue.
        let popped = match self.entries.first() {
            Some(e) if track_key(&e.track) == track_key(&track) => Some(self.entries.remove(0).entry_id),
            _ => None,
        };
        let (op_id, by) = self.attribution(&[AppliedOp::Skip]);
        let op = if skipped || !op_id.is_empty() { AppliedOp::Skip } else { AppliedOp::TrackChange };
        self.current = Some(Current { track: track.clone(), instance: instance.clone(), playing });
        self.position_ms = position_ms;
        let mut msg = self.next_applied(op, &op_id, &by);
        if let HostMsg::Applied { track_instance_id, position_ms: pos, track: t, playing: p, entry_id, .. } = &mut msg {
            *track_instance_id = instance;
            *pos = position_ms;
            *t = Some(track);
            *p = Some(playing);
            *entry_id = popped;
        }
        self.finish(&op_id, &by, msg)
    }

    /// Playback stopped completely (end of the queue).
    pub fn stopped(&mut self) -> Vec<Out> {
        if self.current.is_none() {
            return vec![];
        }
        self.current = None;
        vec![Out::All(self.snapshot())]
    }

    pub fn play_state(&mut self, playing: bool, position_ms: i64) -> Vec<Out> {
        let Some(cur) = self.current.as_mut() else { return vec![] };
        if cur.playing == playing {
            return vec![];
        }
        cur.playing = playing;
        let instance = cur.instance.clone();
        self.position_ms = position_ms;
        let op = if playing { AppliedOp::Play } else { AppliedOp::Pause };
        let (op_id, by) = self.attribution(&[op]);
        let mut msg = self.next_applied(op, &op_id, &by);
        if let HostMsg::Applied { track_instance_id, position_ms: pos, playing: p, .. } = &mut msg {
            *track_instance_id = instance;
            *pos = position_ms;
            *p = Some(playing);
        }
        self.finish(&op_id, &by, msg)
    }

    pub fn seeked(&mut self, position_ms: i64) -> Vec<Out> {
        let Some(cur) = self.current.as_ref() else { return vec![] };
        let (instance, playing) = (cur.instance.clone(), cur.playing);
        self.position_ms = position_ms;
        let (op_id, by) = self.attribution(&[AppliedOp::Seek]);
        let mut msg = self.next_applied(AppliedOp::Seek, &op_id, &by);
        if let HostMsg::Applied { track_instance_id, position_ms: pos, playing: p, .. } = &mut msg {
            *track_instance_id = instance;
            *pos = position_ms;
            *p = Some(playing);
        }
        self.finish(&op_id, &by, msg)
    }

    /// The host's upcoming queue after any change. Sends the smallest matching update.
    pub fn queue_changed(&mut self, upcoming: Vec<TrackRef>) -> Vec<Out> {
        let old: Vec<String> = self.entries.iter().map(|e| track_key(&e.track)).collect();
        let new: Vec<String> = upcoming.iter().map(track_key).collect();
        if old == new {
            return vec![];
        }
        let (op_id, by) = self.attribution(&[AppliedOp::QueueAdd, AppliedOp::QueueRemove]);
        // Cleared.
        if new.is_empty() {
            self.entries.clear();
            let msg = self.next_applied(AppliedOp::QueueClear, &op_id, &by);
            return self.finish(&op_id, &by, msg);
        }
        // One added.
        if new.len() == old.len() + 1 {
            if let Some(i) = (0..new.len()).find(|&i| new[..i] == old[..i] && new[i + 1..] == old[i..]) {
                let entry = self.new_entry(upcoming[i].clone(), &by);
                self.entries.insert(i, entry.clone());
                let mut msg = self.next_applied(AppliedOp::QueueAdd, &op_id, &by);
                if let HostMsg::Applied { entry: e, index, .. } = &mut msg {
                    *e = Some(entry);
                    *index = Some(i);
                }
                return self.finish(&op_id, &by, msg);
            }
        }
        // One removed.
        if new.len() + 1 == old.len() {
            if let Some(i) = (0..old.len()).find(|&i| old[..i] == new[..i] && old[i + 1..] == new[i..]) {
                let removed = self.entries.remove(i);
                let mut msg = self.next_applied(AppliedOp::QueueRemove, &op_id, &by);
                if let HostMsg::Applied { entry_id, .. } = &mut msg {
                    *entry_id = Some(removed.entry_id);
                }
                return self.finish(&op_id, &by, msg);
            }
        }
        // Same songs, other order.
        let mut sorted_old = old.clone();
        let mut sorted_new = new.clone();
        sorted_old.sort();
        sorted_new.sort();
        if sorted_old == sorted_new {
            let mut pool = std::mem::take(&mut self.entries);
            for key in &new {
                let i = pool.iter().position(|e| &track_key(&e.track) == key).expect("same songs");
                self.entries.push(pool.remove(i));
            }
            let order = self.entries.iter().map(|e| e.entry_id.clone()).collect();
            let mut msg = self.next_applied(AppliedOp::QueueReorder, &op_id, &by);
            if let HostMsg::Applied { order: o, .. } = &mut msg {
                *o = Some(order);
            }
            return self.finish(&op_id, &by, msg);
        }
        // Anything bigger: rebuild, keeping the entries that are still there.
        let mut pool = std::mem::take(&mut self.entries);
        for t in upcoming {
            let key = track_key(&t);
            match pool.iter().position(|e| track_key(&e.track) == key) {
                Some(i) => self.entries.push(pool.remove(i)),
                None => {
                    let e = self.new_entry(t, &by);
                    self.entries.push(e);
                }
            }
        }
        vec![Out::All(self.snapshot())]
    }

    fn new_entry(&mut self, track: TrackRef, by: &str) -> QueueEntry {
        let id = format!("e{}", self.next_entry);
        self.next_entry += 1;
        QueueEntry { entry_id: id, track, added_by: by.into(), added_by_name: self.name_of(by) }
    }

    /// After carrying it out. If nothing changed (e.g. "play" while already playing), the
    /// requester still gets its `APPLIED`, so it isn't left waiting.
    pub fn end_op(&mut self) -> Vec<Out> {
        let Some(p) = self.pending.take() else { return vec![] };
        if p.used {
            return vec![];
        }
        let (instance, playing) = self.current.as_ref().map(|c| (c.instance.clone(), c.playing)).unwrap_or_default();
        let mut msg = self.next_applied(p.kind, &p.op_id, &p.by);
        if let HostMsg::Applied { track_instance_id, position_ms, playing: pl, .. } = &mut msg {
            *track_instance_id = instance;
            *position_ms = self.position_ms;
            *pl = Some(playing);
        }
        self.finish(&p.op_id, &p.by, msg)
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

    fn room_with_guest() -> (HostRoom, Instant) {
        let mut r = HostRoom::new("host".into(), "Hosty");
        let now = Instant::now();
        r.guest_connected("g1");
        let (out, _) = r.handle("g1", hello("g1", "Gina"), now, 0, &|_| true);
        assert!(matches!(&out[..], [Out::All(HostMsg::RoomSnapshot { .. })]));
        r.track_started(yt("a"), 0, true, false);
        (r, now)
    }

    fn hello(id: &str, name: &str) -> GuestMsg {
        GuestMsg::Hello { name: name.into(), peer_id: id.into(), protocol_version: PROTOCOL_VERSION }
    }

    fn req(op_id: &str, acl: u64, op: RequestOp) -> GuestMsg {
        GuestMsg::Request { op_id: op_id.into(), acl_version: acl, op }
    }

    fn skip(instance: &str) -> RequestOp {
        RequestOp::Skip { track_instance_id: instance.into() }
    }

    fn rejected(out: &[Out]) -> Option<RejectReason> {
        match out {
            [Out::To(_, HostMsg::Rejected { reason, .. })] => Some(*reason),
            _ => None,
        }
    }

    fn acl(r: &HostRoom) -> u64 {
        r.acl_version
    }

    #[test]
    fn validation_order() {
        let (mut r, now) = room_with_guest();
        let inst = r.current_instance().unwrap().to_string();
        // 1. No capability, even with everything else wrong.
        let (out, eff) = r.handle("g1", req("o1", 999, skip("old#1")), now, 0, &|_| true);
        assert_eq!(rejected(&out), Some(RejectReason::NoCap));
        assert!(eff.is_none());
        r.grant("g1", Cap::Skip);
        // 2. Stale ACL version beats a stale track.
        let (out, _) = r.handle("g1", req("o2", acl(&r) - 1, skip("old#1")), now, 0, &|_| true);
        assert_eq!(rejected(&out), Some(RejectReason::AclStale));
        // 3. Stale track.
        let (out, _) = r.handle("g1", req("o3", acl(&r), skip("old#1")), now, 0, &|_| true);
        assert_eq!(rejected(&out), Some(RejectReason::StaleTrack));
        // Accepted.
        let (out, eff) = r.handle("g1", req("o4", acl(&r), skip(&inst)), now, 0, &|_| true);
        assert!(out.is_empty());
        assert_eq!(eff, Some((Effect::Skip, "o4".into())));
        // The App skipped; that is broadcast as the guest's skip.
        let out = r.track_started(yt("b"), 0, true, true);
        match &out[..] {
            [Out::All(HostMsg::Applied { op, op_id, by, by_name, .. })] => {
                assert_eq!((*op, op_id.as_str(), by.as_str(), by_name.as_str()), (AppliedOp::Skip, "o4", "g1", "Gina"));
            }
            other => panic!("{other:?}"),
        }
        assert!(r.end_op().is_empty());
        // 4. Cooldown for a second skip right after.
        let inst = r.current_instance().unwrap().to_string();
        let (out, _) = r.handle("g1", req("o5", acl(&r), skip(&inst)), now + Duration::from_millis(500), 0, &|_| true);
        assert_eq!(rejected(&out), Some(RejectReason::Cooldown));
    }

    #[test]
    fn duplicate_op_ids_get_the_same_answer() {
        let (mut r, now) = room_with_guest();
        r.grant("g1", Cap::Pause);
        let inst = r.current_instance().unwrap().to_string();
        let pause = RequestOp::Pause { track_instance_id: inst.clone() };
        let (_, eff) = r.handle("g1", req("p1", acl(&r), pause.clone()), now, 0, &|_| true);
        assert_eq!(eff, Some((Effect::Pause, "p1".into())));
        let applied = r.play_state(false, 1200);
        assert!(matches!(&applied[..], [Out::All(HostMsg::Applied { op: AppliedOp::Pause, .. })]));
        r.end_op();
        let Out::All(first) = &applied[0] else { panic!() };
        // Resent after a reconnect: same APPLIED, to the requester only, nothing new happens.
        let (out, eff) = r.handle("g1", req("p1", acl(&r), pause), now, 0, &|_| true);
        assert!(eff.is_none());
        assert_eq!(out, vec![Out::To("g1".into(), first.clone())]);
    }

    #[test]
    fn simultaneous_skips_second_is_stale() {
        let (mut r, now) = room_with_guest();
        r.guest_connected("g2");
        r.handle("g2", hello("g2", "Gus"), now, 0, &|_| true);
        r.grant("g1", Cap::Skip);
        r.grant("g2", Cap::Skip);
        let inst = r.current_instance().unwrap().to_string();
        let (_, eff) = r.handle("g1", req("a", acl(&r), skip(&inst)), now, 0, &|_| true);
        assert!(eff.is_some());
        r.track_started(yt("b"), 0, true, true);
        r.end_op();
        let (out, _) = r.handle("g2", req("b", acl(&r), skip(&inst)), now, 0, &|_| true);
        assert_eq!(rejected(&out), Some(RejectReason::StaleTrack));
    }

    #[test]
    fn revoke_makes_in_flight_requests_stale() {
        let (mut r, now) = room_with_guest();
        r.grant("g1", Cap::Seek);
        r.grant("g1", Cap::Skip);
        let made_under = acl(&r);
        r.revoke("g1", Cap::Skip);
        let inst = r.current_instance().unwrap().to_string();
        let seek = RequestOp::Seek { position_ms: 5000, track_instance_id: inst };
        let (out, _) = r.handle("g1", req("s", made_under, seek), now, 0, &|_| true);
        assert_eq!(rejected(&out), Some(RejectReason::AclStale));
    }

    #[test]
    fn queue_add_checks() {
        let (mut r, now) = room_with_guest();
        r.grant("g1", Cap::QueueAdd);
        let mut local = yt("x");
        local.fingerprint = String::new();
        local.track_id = String::new();
        let add = |t: TrackRef| RequestOp::QueueAdd { track: t, mode: AddMode::End };
        let (out, _) = r.handle("g1", req("q1", acl(&r), add(local)), now, 0, &|_| true);
        assert_eq!(rejected(&out), Some(RejectReason::NotFoundLocally));
        // The host has YouTube switched off.
        let (out, _) = r.handle("g1", req("q2", acl(&r), add(yt("x"))), now, 0, &|_| false);
        assert_eq!(rejected(&out), Some(RejectReason::NotFoundLocally));
        let (_, eff) = r.handle("g1", req("q3", acl(&r), add(yt("x"))), now, 0, &|_| true);
        assert_eq!(eff, Some((Effect::QueueAdd { track: yt("x"), next: false }, "q3".into())));
        // The App added it; the queue update names the guest.
        let out = r.queue_changed(vec![yt("x")]);
        match &out[..] {
            [Out::All(HostMsg::Applied { op: AppliedOp::QueueAdd, entry: Some(e), index: Some(0), op_id, .. })] => {
                assert_eq!((e.added_by.as_str(), e.added_by_name.as_str(), op_id.as_str()), ("g1", "Gina", "q3"));
            }
            other => panic!("{other:?}"),
        }
        r.end_op();
        // Full queue.
        let many: Vec<TrackRef> = (0..MAX_QUEUE).map(|i| yt(&format!("m{i}"))).collect();
        r.queue_changed(many);
        let (out, _) = r.handle("g1", req("q4", acl(&r), add(yt("y"))), now + Duration::from_secs(60), 0, &|_| true);
        assert_eq!(rejected(&out), Some(RejectReason::QueueFull));
    }

    #[test]
    fn queue_diffs() {
        let (mut r, _) = room_with_guest();
        let applied_op = |out: &[Out]| match out {
            [Out::All(HostMsg::Applied { op, .. })] => Some(*op),
            _ => None,
        };
        assert_eq!(applied_op(&r.queue_changed(vec![yt("b")])), Some(AppliedOp::QueueAdd));
        assert_eq!(applied_op(&r.queue_changed(vec![yt("b"), yt("c")])), Some(AppliedOp::QueueAdd));
        assert_eq!(applied_op(&r.queue_changed(vec![yt("c"), yt("b")])), Some(AppliedOp::QueueReorder));
        assert_eq!(applied_op(&r.queue_changed(vec![yt("c")])), Some(AppliedOp::QueueRemove));
        assert!(r.queue_changed(vec![yt("c")]).is_empty());
        assert_eq!(applied_op(&r.queue_changed(vec![])), Some(AppliedOp::QueueClear));
        // Several changes at once: a snapshot.
        assert!(matches!(&r.queue_changed(vec![yt("d"), yt("e")])[..], [Out::All(HostMsg::RoomSnapshot { .. })]));
        // Playing the head of the queue pops it, and says which entry.
        let head = r.queue()[0].entry_id.clone();
        match &r.track_started(yt("d"), 0, true, false)[..] {
            [Out::All(HostMsg::Applied { op: AppliedOp::TrackChange, entry_id: Some(id), .. })] => assert_eq!(id, &head),
            other => panic!("{other:?}"),
        }
        assert!(r.queue_changed(vec![yt("e")]).is_empty());
    }

    #[test]
    fn kick_blocks_rejoin_and_everyone_sees_everyone() {
        let (mut r, now) = room_with_guest();
        r.handle("g2", hello("g2", "Gus"), now, 0, &|_| true);
        let Out::All(HostMsg::RoomSnapshot { peers, .. }) = &r.handle("g2", hello("g2", "Gus"), now, 0, &|_| true).0[0] else {
            panic!()
        };
        let names: Vec<_> = peers.iter().map(|p| (p.name.as_str(), p.role)).collect();
        assert_eq!(names, vec![("Hosty", Role::Host), ("Gina", Role::Guest), ("Gus", Role::Guest)]);
        r.grant("g2", Cap::Skip);
        let out = r.kick("g2");
        assert!(out.contains(&Out::To("g2".into(), HostMsg::Kicked)));
        assert!(out.contains(&Out::Disconnect("g2".into())));
        // Coming back is refused.
        let (out, _) = r.handle("g2", hello("g2", "Gus"), now, 0, &|_| true);
        assert!(out.contains(&Out::Disconnect("g2".into())));
        assert!(r.peer_list().iter().all(|p| p.peer_id != "g2"));
    }

    #[test]
    fn reconnect_keeps_permissions_and_names_persist() {
        let (mut r, now) = room_with_guest();
        r.grant("g1", Cap::QueueAdd);
        r.handle("g1", req("q", acl(&r), RequestOp::QueueAdd { track: yt("z"), mode: AddMode::End }), now, 0, &|_| true);
        r.queue_changed(vec![yt("z")]);
        r.end_op();
        r.guest_disconnected("g1");
        assert!(!r.peer_list()[1].online);
        r.handle("g1", hello("g1", "Gina2"), now, 0, &|_| true);
        assert_eq!(r.caps_of("g1"), vec![Cap::QueueAdd]);
        assert!(r.peer_list()[1].online);
        // The queue still shows the name used when the song was added.
        assert_eq!(r.queue()[0].added_by_name, "Gina");
    }

    #[test]
    fn unchanged_state_still_answers() {
        let (mut r, now) = room_with_guest();
        r.grant("g1", Cap::Pause);
        let inst = r.current_instance().unwrap().to_string();
        let (_, eff) = r.handle("g1", req("pl", acl(&r), RequestOp::Play { track_instance_id: inst }), now, 0, &|_| true);
        assert_eq!(eff.map(|e| e.0), Some(Effect::Play));
        // Already playing: the App changes nothing, the guest still gets APPLIED.
        assert!(r.play_state(true, 0).is_empty());
        let out = r.end_op();
        assert!(matches!(&out[..], [Out::All(HostMsg::Applied { op: AppliedOp::Play, .. })]));
    }

    #[test]
    fn hello_must_match_transport_identity() {
        let mut r = HostRoom::new("host".into(), "H");
        let (out, _) = r.handle("g1", hello("someone-else", "X"), Instant::now(), 0, &|_| true);
        assert_eq!(out, vec![Out::Disconnect("g1".into())]);
    }
}
