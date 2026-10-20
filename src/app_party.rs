//! Listening parties inside the app: connects the party state machines (`crate::party`) to
//! the player.
//!
//! Host: the player works as always; small hooks report what happened (track started, play /
//! pause, seek, queue changed) and guest requests are carried out with the normal player
//! functions. Guest: playback follows the host. Local controls become requests, and remote
//! changes are applied with `applying` set, so they never produce requests again (no echoes).

use super::{App, post, s};
use crate::model::{SourceKind, Track};
use crate::party::clock::{self, Correction};
use crate::party::code;
use crate::party::guest::{GuestOut, GuestRoom};
use crate::party::host::{Effect, HostRoom, Out};
use crate::party::protocol::*;
use crate::party::signaling::DEFAULT_RELAYS;
use crate::party::transport::{Config, Event, Failure, Party, Turn};
use crate::{Backend, PartyEntry, PartyPeer};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::time::{Duration, Instant};

const HEARTBEAT_EVERY: Duration = Duration::from_secs(3);
/// No message from the host for this long: reconnect.
const HOST_SILENT: Duration = Duration::from_secs(10);
const SOUNDCLOUD: &str = "https://soundcloud.com/";

#[cfg(debug_assertions)]
thread_local! {
    /// Test automation: (start, [(seconds after joining, action)]).
    static DEBUG_SCRIPT: std::cell::RefCell<Option<(Instant, Vec<(u64, String)>)>> = const { std::cell::RefCell::new(None) };
    /// Guests the automation already granted everything to (once each).
    static DEBUG_GRANTED: std::cell::RefCell<std::collections::HashSet<String>> = std::cell::RefCell::new(Default::default());
}

enum PartyRole {
    Host(Box<HostRoom>),
    Guest(Box<GuestRoom>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Connecting,
    Live,
    Reconnecting,
}

/// The guest's own playback from before joining, restored when leaving.
struct Saved {
    tracks: Vec<Track>,
    index: Option<usize>,
    position: Duration,
    tempo: f32,
}

/// A track the guest is loading; where to start it once it's ready.
struct Loading {
    instance: String,
    position_ms: i64,
    playing: bool,
    at: Instant,
}

pub(super) struct PartyState {
    id: u64,
    link: Party,
    role: PartyRole,
    code: String,
    has_password: bool,
    phase: Phase,
    relays: usize,
    started: Instant,
    /// Host: last heartbeat sent. Guest: last message from the host.
    last_beat: Instant,
    /// Set while remote changes are applied, so they don't turn into requests.
    applying: bool,
    /// The next track start comes from a user skip.
    user_skip: bool,
    saved: Option<Saved>,
    loading: Option<Loading>,
    /// Guest: why the current track can't be heard here (empty = it can).
    unavailable: String,
    nudge_until: Option<Instant>,
    /// Guest: entry ids of the songs mirrored into the local queue (after the current one).
    mirror_ids: Vec<String>,
}

/// Something a guest tried to do with the normal controls.
pub(super) enum Intercept {
    TogglePlay,
    Next,
    Previous,
    Seek(f32),
    QueueAdd(Track, bool),
    /// Index in the local queue.
    QueueRemove(usize),
    /// Reordering, clearing, shuffle, repeat.
    QueueEdit,
    /// Playing something else (a list, a file).
    PlayOther,
}

/// The shareable form of a track: YouTube / SoundCloud by id, anything else host-only.
pub(super) fn track_ref(t: &Track) -> TrackRef {
    let (fingerprint, source) = match t.source {
        SourceKind::YouTube => (format!("yt:{}", t.uri), "youtube"),
        SourceKind::SoundCloud => match t.uri.strip_prefix(SOUNDCLOUD) {
            Some(path) => (format!("sc:{path}"), "soundcloud"),
            None => (String::new(), "soundcloud"),
        },
        SourceKind::Nextcloud => (String::new(), "nextcloud"),
        SourceKind::Local | SourceKind::File => (String::new(), "local"),
    };
    TrackRef {
        track_id: fingerprint.clone(),
        fingerprint,
        title: t.title.clone(),
        artist: t.display_artist().to_string(),
        duration_ms: t.duration_ms,
        source: source.into(),
    }
}

/// The local track for a shared one (None for host-only tracks).
pub(super) fn track_from_ref(r: &TrackRef) -> Option<Track> {
    let mut t = if let Some(id) = r.fingerprint.strip_prefix("yt:") {
        Track::new(SourceKind::YouTube, id)
    } else if let Some(path) = r.fingerprint.strip_prefix("sc:") {
        Track::new(SourceKind::SoundCloud, format!("{SOUNDCLOUD}{path}"))
    } else {
        return None;
    };
    t.title = if r.title.is_empty() { t.uri.clone() } else { r.title.clone() };
    t.artist = r.artist.clone();
    t.duration_ms = r.duration_ms;
    t.album = if t.source == SourceKind::YouTube { "YouTube".into() } else { "SoundCloud".into() };
    Some(t)
}

/// "Can skip · pause · add songs", or "Listening".
fn caps_text(caps: &[Cap]) -> String {
    let names: Vec<&str> = Cap::ALL
        .iter()
        .filter(|c| caps.contains(c))
        .map(|c| match c {
            Cap::Skip => "skip",
            Cap::Seek => "seek",
            Cap::Pause => "pause",
            Cap::QueueAdd => "add songs",
            Cap::QueueRemove => "remove songs",
        })
        .collect();
    if names.is_empty() { "Listening".into() } else { format!("Can {}", names.join(" · ")) }
}

fn cap_from_name(name: &str) -> Option<Cap> {
    Some(match name {
        "skip" => Cap::Skip,
        "seek" => Cap::Seek,
        "pause" => Cap::Pause,
        "queue_add" => Cap::QueueAdd,
        "queue_remove" => Cap::QueueRemove,
        _ => return None,
    })
}

impl App {
    // ------------------------------------------------------------------ settings

    pub(super) fn load_party_settings(&self) {
        let db = self.db.lock().unwrap();
        let b = self.backend();
        b.set_party_name(s(db.setting_or("party_name", "")));
        b.set_party_turn_url(s(db.setting_or("party_turn_url", "")));
        b.set_party_turn_user(s(db.setting_or("party_turn_user", "")));
        b.set_party_turn_pass(s(db.setting_or("party_turn_pass", "")));
        b.set_party_relays(s(db.setting_or("party_relays", &DEFAULT_RELAYS.join("\n"))));
    }

    pub(super) fn save_party_settings(&mut self) {
        let b = self.backend();
        let name = clean_name(&b.get_party_name());
        let name = if b.get_party_name().trim().is_empty() { String::new() } else { name };
        b.set_party_name(s(&name));
        {
            let db = self.db.lock().unwrap();
            db.set_setting("party_name", &name);
            db.set_setting("party_turn_url", b.get_party_turn_url().trim());
            db.set_setting("party_turn_user", b.get_party_turn_user().trim());
            db.set_setting("party_turn_pass", &b.get_party_turn_pass());
            db.set_setting("party_relays", b.get_party_relays().trim());
        }
        // A new name is shown to everyone in a running party.
        if !name.is_empty() {
            let outs = match self.party.as_mut().map(|p| &mut p.role) {
                Some(PartyRole::Host(room)) => room.rename_self(&name),
                Some(PartyRole::Guest(room)) => {
                    let msg = GuestMsg::Hello { name: name.clone(), peer_id: room.me.clone(), protocol_version: PROTOCOL_VERSION };
                    self.party.as_ref().unwrap().link.broadcast(encode(&msg));
                    vec![]
                }
                None => vec![],
            };
            self.party_dispatch(outs);
        }
        self.toast("Party settings saved");
        self.party_ui();
    }

    fn party_config(&self, code: &str, password: &str) -> Config {
        let b = self.backend();
        let mut relays: Vec<String> = b
            .get_party_relays()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| l.starts_with("wss://") || l.starts_with("ws://"))
            .collect();
        if relays.is_empty() {
            relays = DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect();
        }
        Config {
            code: code.into(),
            password: password.into(),
            relays,
            turn: Turn {
                url: b.get_party_turn_url().to_string(),
                username: b.get_party_turn_user().to_string(),
                credential: b.get_party_turn_pass().to_string(),
            },
        }
    }

    fn party_name(&self) -> Option<String> {
        let n = self.backend().get_party_name().trim().to_string();
        (!n.is_empty()).then_some(n)
    }

    // ------------------------------------------------------------------ start / stop

    pub(super) fn party_create(&mut self, password: &str) {
        self.party_create_with(code::generate(), password);
    }

    fn party_create_with(&mut self, code: String, password: &str) {
        if self.party.is_some() {
            return;
        }
        let Some(name) = self.party_name() else {
            self.backend().set_party_error(s("Set your display name first"));
            return;
        };
        let id = self.next_party_id();
        let config = self.party_config(&code, password);
        let link = match Party::host(config, move |e| post(move |a| a.party_event(id, e))) {
            Ok(l) => l,
            Err(e) => {
                self.backend().set_party_error(s(format!("Couldn't start the party: {e:#}")));
                return;
            }
        };
        let mut room = HostRoom::new(link.peer_id.clone(), &name);
        // The party starts with whatever is playing now.
        if let Some(t) = self.queue.current().cloned() {
            room.track_started(track_ref(&t), self.party_position_ms(), self.backend().get_playing(), false);
        }
        room.queue_changed(self.party_upcoming());
        self.party = Some(PartyState {
            id,
            link,
            role: PartyRole::Host(Box::new(room)),
            code,
            has_password: !password.is_empty(),
            phase: Phase::Live,
            relays: 0,
            started: Instant::now(),
            last_beat: Instant::now(),
            applying: false,
            user_skip: false,
            saved: None,
            loading: None,
            unavailable: String::new(),
            nudge_until: None,
            mirror_ids: vec![],
        });
        self.backend().set_party_error(s(""));
        self.party_ui();
    }

    pub(super) fn party_join(&mut self, code_input: &str, password: &str) {
        if self.party.is_some() {
            return;
        }
        let Some(_) = self.party_name() else {
            self.backend().set_party_error(s("Set your display name first"));
            return;
        };
        let Some(code) = code::normalize(code_input) else {
            self.backend().set_party_error(s("That isn't a valid room code (12 letters)"));
            return;
        };
        let id = self.next_party_id();
        let config = self.party_config(&code, password);
        let link = match Party::guest(config, move |e| post(move |a| a.party_event(id, e))) {
            Ok(l) => l,
            Err(e) => {
                self.backend().set_party_error(s(format!("Couldn't join: {e:#}")));
                return;
            }
        };
        // Put the guest's own music aside; the party decides what plays.
        let saved = Saved {
            tracks: self.queue.tracks.clone(),
            index: self.queue.index,
            position: self.audio.as_ref().map(|a| a.position()).unwrap_or_default(),
            tempo: self.tempo(),
        };
        self.stop();
        if let Some(a) = &self.audio {
            a.fx.set_named("tempo", 1.0);
        }
        self.queue.set(vec![], 0);
        self.refresh_queue();
        let room = GuestRoom::new(link.peer_id.clone());
        self.party = Some(PartyState {
            id,
            link,
            role: PartyRole::Guest(Box::new(room)),
            code,
            has_password: !password.is_empty(),
            phase: Phase::Connecting,
            relays: 0,
            started: Instant::now(),
            last_beat: Instant::now(),
            applying: false,
            user_skip: false,
            saved: Some(saved),
            loading: None,
            unavailable: String::new(),
            nudge_until: None,
            mirror_ids: vec![],
        });
        self.backend().set_party_error(s(""));
        self.party_ui();
    }

    fn next_party_id(&mut self) -> u64 {
        self.party_ids += 1;
        self.party_ids
    }

    /// Leave (guest) or end (host) the party. `message` is shown to the user.
    pub(super) fn party_leave(&mut self, message: Option<String>) {
        let Some(party) = self.party.take() else { return };
        if matches!(party.role, PartyRole::Host(_)) {
            party.link.broadcast(encode(&HostMsg::PartyEnded));
        }
        party.link.shutdown();
        if let Some(saved) = party.saved {
            // Back to the guest's own music, paused where it was.
            self.stop();
            if let Some(a) = &self.audio {
                a.fx.set_named("tempo", saved.tempo);
            }
            self.queue.set(saved.tracks, saved.index.unwrap_or(0));
            self.queue.index = saved.index;
            self.resume_pos = Some(saved.position);
            match self.queue.current().cloned() {
                Some(t) => self.show_now_playing(&t),
                None => {
                    // Nothing of its own was queued: clear what the party showed.
                    let b = self.backend();
                    b.set_has_track(false);
                    b.set_now_title(s("Nothing playing"));
                    b.set_now_artist(s(""));
                    b.set_has_now_cover(false);
                    self.popup.global::<crate::TrayState>().set_has_track(false);
                }
            }
            self.refresh_queue();
            self.update_tray();
        }
        self.backend().set_party_error(s(message.clone().unwrap_or_default()));
        if let Some(m) = message {
            self.toast(m);
        }
        self.party_ui();
    }

    pub(super) fn party_copy_code(&mut self) {
        let Some(code) = self.party.as_ref().map(|p| code::display(&p.code)) else { return };
        match super::share_ui::copy_text(&code) {
            Ok(()) => self.toast("Room code copied"),
            Err(e) => self.toast(format!("Couldn't copy: {e}")),
        }
    }

    // ------------------------------------------------------------------ host controls

    pub(super) fn party_set_cap(&mut self, peer: &str, cap: &str, on: bool) {
        let Some(cap) = cap_from_name(cap) else { return };
        let outs = match self.party.as_mut().map(|p| &mut p.role) {
            Some(PartyRole::Host(room)) if on => room.grant(peer, cap),
            Some(PartyRole::Host(room)) => room.revoke(peer, cap),
            _ => return,
        };
        self.party_dispatch(outs);
        self.party_ui();
    }

    pub(super) fn party_grant_all(&mut self, peer: &str, on: bool) {
        let outs = match self.party.as_mut().map(|p| &mut p.role) {
            Some(PartyRole::Host(room)) if on => room.grant_all(peer),
            Some(PartyRole::Host(room)) => room.revoke_all(peer),
            _ => return,
        };
        self.party_dispatch(outs);
        self.party_ui();
    }

    pub(super) fn party_kick(&mut self, peer: &str) {
        let outs = match self.party.as_mut().map(|p| &mut p.role) {
            Some(PartyRole::Host(room)) => room.kick(peer),
            _ => return,
        };
        self.party_dispatch(outs);
        self.party_ui();
    }

    /// Send host messages where they belong.
    fn party_dispatch(&self, outs: Vec<Out>) {
        let Some(party) = &self.party else { return };
        for out in outs {
            if !matches!(&out, Out::All(HostMsg::SyncHeartbeat { .. }) | Out::To(_, HostMsg::Pong { .. })) {
                log::debug!("party host: {out:?}");
            }
            match out {
                Out::All(m) => party.link.broadcast(encode(&m)),
                Out::To(p, m) => party.link.send_to(&p, encode(&m)),
                Out::Disconnect(p) => party.link.disconnect(&p),
            }
        }
    }

    // ------------------------------------------------------------------ transport events

    fn party_event(&mut self, id: u64, event: Event) {
        let Some(party) = self.party.as_mut() else { return };
        if party.id != id {
            return; // from a party that already ended
        }
        let host = matches!(party.role, PartyRole::Host(_));
        match event {
            Event::Relays(n) => party.relays = n,
            Event::PeerOpen(peer) => {
                let PartyRole::Host(room) = &mut party.role else { return };
                let outs = room.guest_connected(&peer);
                self.party_dispatch(outs);
            }
            Event::PeerMessage(peer, text) => self.party_host_message(&peer, &text),
            Event::PeerClosed(peer) => {
                let PartyRole::Host(room) = &mut party.role else { return };
                let outs = room.guest_disconnected(&peer);
                self.party_dispatch(outs);
            }
            Event::HostOpen if !host => {
                party.phase = Phase::Live;
                party.last_beat = Instant::now();
                let name = self.party_name().unwrap_or_else(|| "Guest".into());
                let outs = match &mut self.party.as_mut().unwrap().role {
                    PartyRole::Guest(room) => room.hello(&name),
                    PartyRole::Host(_) => vec![],
                };
                self.party_apply(outs);
            }
            Event::HostMessage(text) => {
                party.last_beat = Instant::now();
                let Ok(msg) = decode::<HostMsg>(&text) else { return };
                let local = party.started.elapsed().as_millis() as i64;
                let outs = match &mut party.role {
                    PartyRole::Guest(room) => room.on_message(msg, Instant::now(), local),
                    PartyRole::Host(_) => vec![],
                };
                self.party_apply(outs);
            }
            Event::HostLost => {
                party.phase = Phase::Reconnecting;
            }
            Event::Failed(f) => {
                let msg = match f {
                    Failure::HostGone => Failure::HostGone.message().to_string(),
                    other => other.message().to_string(),
                };
                self.party_leave(Some(msg));
                return;
            }
            Event::HostOpen => {}
        }
        self.party_ui();
    }

    /// A message from a guest (host side).
    fn party_host_message(&mut self, peer: &str, text: &str) {
        let yt = self.backend().get_yt_enabled();
        let sc = self.backend().get_sc_enabled();
        let Some(party) = self.party.as_mut() else { return };
        let host_time = party.started.elapsed().as_millis() as i64;
        let PartyRole::Host(room) = &mut party.role else { return };
        room.position_ms = self.audio.as_ref().map(|a| a.position().as_millis() as i64).unwrap_or(0);
        let msg = match decode::<GuestMsg>(text) {
            Ok(m) => m,
            Err(_) => {
                // Answer a malformed request so the guest isn't left waiting.
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                    if let Some(op_id) = v.get("opId").and_then(|o| o.as_str()) {
                        let reject = HostMsg::Rejected { op_id: op_id.into(), reason: RejectReason::Invalid };
                        party.link.send_to(peer, encode(&reject));
                    }
                }
                return;
            }
        };
        let can_play = |t: &TrackRef| (t.source == "youtube" && yt) || (t.source == "soundcloud" && sc);
        let (outs, effect) = room.handle(peer, msg, Instant::now(), host_time, &can_play);
        let who = room.peer_list().into_iter().find(|p| p.peer_id == peer).map(|p| p.name).unwrap_or_default();
        self.party_dispatch(outs);
        if let Some((effect, _)) = effect {
            // The host sees what guests did, like guests see each other.
            let note = match &effect {
                Effect::Skip => format!("Skipped by {who}"),
                Effect::Pause => format!("Paused by {who}"),
                Effect::Play => format!("Resumed by {who}"),
                Effect::Seek(ms) => format!("{who} jumped to {}", crate::model::format_duration(*ms)),
                Effect::QueueAdd { track, .. } => format!("{who} added “{}”", track.title),
                Effect::QueueRemove(_) => format!("{who} removed a song from the queue"),
            };
            self.party_host_effect(effect);
            self.toast(note);
            // Queue changes are reported right away, so they belong to this request.
            self.party_sync_queue();
            let outs = match self.party.as_mut().map(|p| &mut p.role) {
                Some(PartyRole::Host(room)) => room.end_op(),
                _ => vec![],
            };
            self.party_dispatch(outs);
        }
        self.party_ui();
    }

    /// Carry out an accepted guest request with the normal player functions.
    fn party_host_effect(&mut self, effect: Effect) {
        log::info!("party host: carrying out {effect:?}");
        match effect {
            Effect::Skip => self.next(true),
            Effect::Pause => self.set_play_state(false),
            Effect::Play => self.set_play_state(true),
            Effect::Seek(ms) => self.seek(ms as f32 / 1000.0),
            Effect::QueueAdd { track, next } => {
                let Some(t) = track_from_ref(&track) else { return };
                let was_empty = self.queue.current().is_none();
                if next { self.queue.insert_next(t) } else { self.queue.push(t) }
                self.refresh_queue();
                if was_empty {
                    self.start_current();
                } else {
                    self.prefetch_next();
                }
            }
            Effect::QueueRemove(i) => {
                if let Some(cur) = self.queue.index {
                    self.queue.remove(cur + 1 + i);
                    self.refresh_queue();
                }
            }
        }
    }

    // ------------------------------------------------------------------ host hooks

    fn party_position_ms(&self) -> i64 {
        self.audio.as_ref().map(|a| a.position().as_millis() as i64).unwrap_or(0)
    }

    fn party_upcoming(&self) -> Vec<TrackRef> {
        match self.queue.index {
            Some(i) => self.queue.tracks.iter().skip(i + 1).map(track_ref).collect(),
            None => vec![],
        }
    }

    fn host_room(&mut self) -> Option<&mut HostRoom> {
        match self.party.as_mut().map(|p| &mut p.role) {
            Some(PartyRole::Host(room)) => Some(room),
            _ => None,
        }
    }

    pub(super) fn party_is_guest(&self) -> bool {
        matches!(self.party.as_ref().map(|p| &p.role), Some(PartyRole::Guest(_)))
    }

    /// `next()` is about to start a track; `user` = a skip rather than the end of a song.
    pub(super) fn party_note_skip(&mut self, user: bool) {
        if let Some(p) = self.party.as_mut() {
            p.user_skip = user;
        }
    }

    /// Hook: a track started (end of `start_current_with`).
    pub(super) fn party_track_started(&mut self) {
        if self.host_room().is_none() {
            return;
        }
        // The track start pops it from the party queue itself; the queue diff after that only
        // sends what else changed.
        let Some(t) = self.queue.current().cloned() else { return };
        let skipped = self.party.as_mut().map(|p| std::mem::take(&mut p.user_skip)).unwrap_or(false);
        let outs = self.host_room().map(|r| r.track_started(track_ref(&t), 0, true, skipped)).unwrap_or_default();
        self.party_dispatch(outs);
        self.party_sync_queue();
        self.party_ui();
    }

    /// Hook: play / pause changed (end of `toggle_play`).
    pub(super) fn party_play_state(&mut self) {
        let playing = self.backend().get_playing();
        let pos = self.party_position_ms();
        let outs = self.host_room().map(|r| r.play_state(playing, pos)).unwrap_or_default();
        self.party_dispatch(outs);
    }

    /// Hook: the user seeked.
    pub(super) fn party_seeked(&mut self) {
        let pos = self.party_position_ms();
        let outs = self.host_room().map(|r| r.seeked(pos)).unwrap_or_default();
        self.party_dispatch(outs);
    }

    /// Hook: playback stopped (end of the queue).
    pub(super) fn party_stopped(&mut self) {
        let outs = self.host_room().map(|r| r.stopped()).unwrap_or_default();
        self.party_dispatch(outs);
    }

    /// Send queue changes marked by `refresh_queue`.
    fn party_sync_queue(&mut self) {
        if !self.party_queue_dirty.replace(false) {
            return;
        }
        let upcoming = self.party_upcoming();
        let outs = self.host_room().map(|r| r.queue_changed(upcoming)).unwrap_or_default();
        self.party_dispatch(outs);
    }

    /// Every player tick.
    pub(super) fn party_tick(&mut self) {
        if self.party.is_none() {
            return;
        }
        #[cfg(debug_assertions)]
        self.party_debug_tick();
        if self.party.is_none() {
            return;
        }
        let now = Instant::now();
        let loading = self.loading;
        let pos = self.party_position_ms();
        let low_end = self.backend().get_low_end();
        let party = self.party.as_mut().unwrap();
        let local = party.started.elapsed().as_millis() as i64;
        match &mut party.role {
            PartyRole::Host(room) => {
                room.position_ms = pos;
                // Heartbeats only while a track is on (and loaded); nothing to sync otherwise.
                if now.duration_since(party.last_beat) >= HEARTBEAT_EVERY && !loading && room.current_instance().is_some() {
                    party.last_beat = now;
                    let beat = room.heartbeat(local);
                    self.party_dispatch(vec![beat]);
                }
                self.party_sync_queue();
            }
            PartyRole::Guest(room) => {
                let outs = room.tick(now, local);
                if party.phase == Phase::Live && now.duration_since(party.last_beat) > HOST_SILENT {
                    party.phase = Phase::Reconnecting;
                    party.last_beat = now;
                    party.link.reconnect();
                }
                if party.nudge_until.is_some_and(|t| now >= t) || (low_end && party.nudge_until.is_some()) {
                    party.nudge_until = None;
                    if let Some(a) = &self.audio {
                        a.fx.set_named("tempo", 1.0);
                    }
                }
                self.party_apply(outs);
                self.party_ui_light();
            }
        }
    }

    // ------------------------------------------------------------------ guest

    /// Carry out what the guest room says (remote changes, messages to send).
    fn party_apply(&mut self, outs: Vec<GuestOut>) {
        let mut ui = false;
        for out in outs {
            if !matches!(out, GuestOut::Send(GuestMsg::Ping { .. }) | GuestOut::CheckSync) {
                log::info!("party guest: {out:?}");
            }
            match out {
                GuestOut::Send(m) => {
                    if let Some(p) = &self.party {
                        p.link.broadcast(encode(&m));
                    }
                }
                GuestOut::LoadTrack { track, instance, position_ms, playing } => {
                    self.guest_load(track, instance, position_ms, playing);
                    ui = true;
                }
                GuestOut::Stop => {
                    self.guest_applying(|a| a.stop());
                    ui = true;
                }
                GuestOut::SetPlaying { playing, position_ms } => {
                    if self.party.as_ref().is_some_and(|p| p.unavailable.is_empty() && p.loading.is_none()) {
                        self.guest_applying(|a| a.set_play_state(playing));
                        self.guest_seek_if_off(position_ms, clock::HARD_SEEK_MS);
                    } else if let Some(l) = self.party.as_mut().and_then(|p| p.loading.as_mut()) {
                        l.playing = playing;
                        l.position_ms = position_ms;
                        l.at = Instant::now();
                    }
                    ui = true;
                }
                GuestOut::Seek(ms) => self.guest_seek_if_off(ms, 0),
                GuestOut::CheckSync => self.guest_check_sync(),
                GuestOut::Reconcile => self.guest_reconcile(),
                GuestOut::QueueChanged => {
                    self.guest_mirror_queue();
                    ui = true;
                }
                GuestOut::PeersChanged | GuestOut::CapsChanged | GuestOut::RequestDone(_) => ui = true,
                GuestOut::Toast(t) => self.toast(t),
                GuestOut::Kicked => {
                    self.party_leave(Some("The host removed you from the party".into()));
                    return;
                }
                GuestOut::Ended => {
                    self.party_leave(Some("The host ended the party".into()));
                    return;
                }
            }
        }
        if ui {
            self.party_ui();
        }
    }

    fn guest_applying(&mut self, f: impl FnOnce(&mut App)) {
        if let Some(p) = self.party.as_mut() {
            p.applying = true;
        }
        f(self);
        if let Some(p) = self.party.as_mut() {
            p.applying = false;
        }
    }

    fn guest_room(&self) -> Option<&GuestRoom> {
        match self.party.as_ref().map(|p| &p.role) {
            Some(PartyRole::Guest(room)) => Some(room),
            _ => None,
        }
    }

    fn guest_room_mut(&mut self) -> Option<&mut GuestRoom> {
        match self.party.as_mut().map(|p| &mut p.role) {
            Some(PartyRole::Guest(room)) => Some(room),
            _ => None,
        }
    }

    /// Play the host's track here, or explain why it can't be heard.
    fn guest_load(&mut self, track: TrackRef, instance: String, position_ms: i64, playing: bool) {
        let (yt, sc) = (self.backend().get_yt_enabled(), self.backend().get_sc_enabled());
        let unavailable = match track_from_ref(&track) {
            None => "This song is on the host's PC — only the host can hear it".to_string(),
            Some(t) if t.source == SourceKind::YouTube && !yt => {
                "Turn on YouTube in Settings → Sources to hear this song".to_string()
            }
            Some(t) if t.source == SourceKind::SoundCloud && !sc => {
                "Turn on SoundCloud in Settings → Sources to hear this song".to_string()
            }
            Some(_) => String::new(),
        };
        let Some(party) = self.party.as_mut() else { return };
        party.unavailable = unavailable.clone();
        party.nudge_until = None;
        if let Some(a) = &self.audio {
            a.fx.set_named("tempo", 1.0);
        }
        if !unavailable.is_empty() {
            // Stay in the party, silent, with the host's track shown.
            party.loading = None;
            self.guest_applying(|a| a.stop());
            let mut shown = Track::new(SourceKind::File, "");
            shown.title = track.title.clone();
            shown.artist = track.artist.clone();
            shown.duration_ms = track.duration_ms;
            self.show_now_playing(&shown);
            return;
        }
        party.loading = Some(Loading { instance, position_ms, playing, at: Instant::now() });
        let t = track_from_ref(&track).expect("checked above");
        self.queue.set(vec![t], 0);
        self.guest_mirror_queue();
        self.guest_applying(|a| a.start_current_with(false));
    }

    /// Hook: a track finished loading (end of `load_file`). Jump to where the host is.
    pub(super) fn party_track_loaded(&mut self) {
        let Some(party) = self.party.as_mut() else { return };
        let Some(loading) = party.loading.take() else { return };
        let local = party.started.elapsed().as_millis() as i64;
        let PartyRole::Guest(room) = &party.role else { return };
        if room.instance().as_deref() != Some(loading.instance.as_str()) {
            return; // the host moved on meanwhile; a newer load is coming
        }
        let fallback = loading.position_ms
            + if loading.playing { loading.at.elapsed().as_millis() as i64 } else { 0 };
        let target = room.expected_position(local).unwrap_or(fallback);
        if let Some(a) = &mut self.audio {
            a.seek(Duration::from_millis(target.max(0) as u64));
        }
        if !loading.playing {
            self.guest_applying(|a| a.set_play_state(false));
        }
    }

    /// Hook: a download for the current track failed.
    pub(super) fn party_track_failed(&mut self, error: &str) {
        if let Some(p) = self.party.as_mut() {
            p.loading = None;
            p.unavailable = format!("Can't play this song here: {error}");
        }
        self.party_ui();
    }

    fn guest_seek_if_off(&mut self, position_ms: i64, tolerance: i64) {
        if self.party.as_ref().is_some_and(|p| !p.unavailable.is_empty() || p.loading.is_some()) {
            return;
        }
        let local = self.party_position_ms();
        if (local - position_ms).abs() > tolerance {
            if let Some(a) = &mut self.audio {
                a.seek(Duration::from_millis(position_ms.max(0) as u64));
            }
        }
    }

    /// After a heartbeat: nudge the speed or jump to stay with the host.
    fn guest_check_sync(&mut self) {
        let low_end = self.backend().get_low_end();
        let loaded = self.audio.as_ref().is_some_and(|a| a.is_loaded());
        let pos = self.party_position_ms();
        let Some(party) = self.party.as_mut() else { return };
        if !party.unavailable.is_empty() || party.loading.is_some() || !loaded {
            return;
        }
        let local = party.started.elapsed().as_millis() as i64;
        let PartyRole::Guest(room) = &party.role else { return };
        let Some(expected) = room.expected_position(local) else { return };
        match clock::correction(pos, expected, !low_end) {
            Correction::InSync => {
                if party.nudge_until.take().is_some() {
                    if let Some(a) = &self.audio {
                        a.fx.set_named("tempo", 1.0);
                    }
                }
            }
            Correction::Rate(rate, dur) => {
                party.nudge_until = Some(Instant::now() + dur);
                if let Some(a) = &self.audio {
                    a.fx.set_named("tempo", rate);
                }
            }
            Correction::Seek(ms) => {
                party.nudge_until = None;
                if let Some(a) = &mut self.audio {
                    a.fx.set_named("tempo", 1.0);
                    a.seek(Duration::from_millis(ms as u64));
                }
            }
        }
        log::info!("party sync: local {pos} ms, host {expected} ms, drift {} ms", pos - expected);
    }

    /// Undo optimistic changes: back to the room's real state.
    fn guest_reconcile(&mut self) {
        let Some(playing) = self.guest_room().and_then(|r| r.current.as_ref()).map(|c| c.playing) else { return };
        if self.party.as_ref().is_some_and(|p| p.unavailable.is_empty() && p.loading.is_none()) {
            self.guest_applying(|a| a.set_play_state(playing));
            self.guest_check_sync();
        }
    }

    /// The local queue shows the party queue (playable songs), after the current track.
    fn guest_mirror_queue(&mut self) {
        let Some(entries) = self.guest_room().map(|r| r.queue.clone()) else { return };
        let keep = self.queue.index.map(|i| i + 1).unwrap_or(0).min(self.queue.tracks.len());
        self.queue.tracks.truncate(keep);
        let mut ids = vec![];
        for e in entries {
            if let Some(t) = track_from_ref(&e.track) {
                self.queue.tracks.push(t);
                ids.push(e.entry_id);
            }
        }
        if let Some(p) = self.party.as_mut() {
            p.mirror_ids = ids;
        }
        self.refresh_queue();
        self.prefetch_next();
    }

    /// The guest used a normal control. Returns true when the party took care of it (the
    /// caller must not do its usual thing).
    pub(super) fn party_intercept(&mut self, action: Intercept) -> bool {
        if !self.party_is_guest() || self.party.as_ref().is_some_and(|p| p.applying) {
            return false;
        }
        let now = Instant::now();
        let instance = self.guest_room().and_then(|r| r.instance());
        let request = |app: &mut App, op: RequestOp, denied: &str| -> bool {
            let msg = app.guest_room_mut().and_then(|r| r.request(op, now));
            match msg {
                Some(m) => {
                    if let Some(p) = &app.party {
                        p.link.broadcast(encode(&m));
                    }
                    true
                }
                None => {
                    app.toast(denied);
                    false
                }
            }
        };
        match action {
            Intercept::TogglePlay => {
                let Some(instance) = instance else { return true };
                let playing = self.backend().get_playing();
                let op = if playing {
                    RequestOp::Pause { track_instance_id: instance }
                } else {
                    RequestOp::Play { track_instance_id: instance }
                };
                if request(self, op, "The host hasn't allowed you to pause") {
                    // Optimistic: pause / resume here right away; corrected if refused.
                    if self.party.as_ref().is_some_and(|p| p.unavailable.is_empty() && p.loading.is_none()) {
                        self.guest_applying(|a| a.set_play_state(!playing));
                    }
                }
            }
            Intercept::Next => {
                if let Some(instance) = instance {
                    request(self, RequestOp::Skip { track_instance_id: instance }, "The host hasn't allowed you to skip");
                }
            }
            Intercept::Previous => self.toast("Only the host can go back"),
            Intercept::Seek(secs) => {
                let Some(instance) = instance else { return true };
                let ms = (secs.max(0.0) * 1000.0) as i64;
                if request(self, RequestOp::Seek { position_ms: ms, track_instance_id: instance }, "The host hasn't allowed you to seek") {
                    self.guest_seek_if_off(ms, 0);
                }
            }
            Intercept::QueueAdd(track, next) => {
                let r = track_ref(&track);
                if r.fingerprint.is_empty() {
                    self.toast("Only YouTube and SoundCloud songs can be added to a party");
                } else if request(
                    self,
                    RequestOp::QueueAdd { track: r, mode: if next { AddMode::Next } else { AddMode::End } },
                    "The host hasn't allowed you to add songs",
                ) {
                    self.toast(format!("Asked to add “{}”", track.title));
                }
            }
            Intercept::QueueRemove(i) => {
                let entry = self.queue.index.and_then(|cur| i.checked_sub(cur + 1)).and_then(|k| {
                    self.party.as_ref().and_then(|p| p.mirror_ids.get(k).cloned())
                });
                if let Some(entry_id) = entry {
                    request(self, RequestOp::QueueRemove { entry_id }, "The host hasn't allowed you to remove songs");
                }
            }
            Intercept::QueueEdit => self.toast("Only the host can change the queue order"),
            Intercept::PlayOther => self.toast("You're in a party — leave it to play your own music"),
        }
        self.party_ui();
        true
    }

    // ------------------------------------------------------------------ test automation

    /// Debug builds only: drive a party from environment variables, so two instances can
    /// be tested against each other without clicking.
    /// `TRAX_PARTY_HOST=<code>` (+ `TRAX_PARTY_PLAY=<playlist>`, `TRAX_PARTY_GRANT=1`) or
    /// `TRAX_PARTY_JOIN=<code>` (+ `TRAX_PARTY_SCRIPT="25:skip,35:pause,40:play,50:seek=60000,55:add=yt:ID"`);
    /// `TRAX_PARTY_PASSWORD` for both.
    #[cfg(debug_assertions)]
    pub(super) fn party_debug_autostart(&mut self) {
        let pw = std::env::var("TRAX_PARTY_PASSWORD").unwrap_or_default();
        let script = || -> Vec<(u64, String)> {
            std::env::var("TRAX_PARTY_SCRIPT")
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.split_once(':').and_then(|(t, a)| Some((t.trim().parse().ok()?, a.trim().to_string()))))
                .collect()
        };
        if let Ok(code) = std::env::var("TRAX_PARTY_HOST") {
            self.party_create_with(code, &pw);
            DEBUG_SCRIPT.with(|c| *c.borrow_mut() = Some((Instant::now(), script())));
            if let Ok(name) = std::env::var("TRAX_PARTY_PLAY") {
                let id = self.db.lock().unwrap().playlists().into_iter().find(|p| p.name == name).map(|p| p.id);
                if let Some(id) = id {
                    self.open_playlist(id);
                    self.play_all("playlist", false);
                }
            }
        }
        if let Ok(code) = std::env::var("TRAX_PARTY_JOIN") {
            self.party_join(&code, &pw);
            DEBUG_SCRIPT.with(|c| *c.borrow_mut() = Some((Instant::now(), script())));
        }
    }

    #[cfg(debug_assertions)]
    fn party_debug_tick(&mut self) {
        if std::env::var("TRAX_PARTY_GRANT").is_ok() {
            let guests: Vec<String> = match self.party.as_ref().map(|p| &p.role) {
                Some(PartyRole::Host(room)) => room
                    .peer_list()
                    .into_iter()
                    .filter(|p| p.role == Role::Guest && p.online)
                    .map(|p| p.peer_id)
                    .filter(|id| DEBUG_GRANTED.with(|g| g.borrow_mut().insert(id.clone())))
                    .collect(),
                _ => vec![],
            };
            for g in guests {
                log::info!("party test: granting all to {g}");
                self.party_grant_all(&g, true);
            }
        }
        let due: Vec<String> = DEBUG_SCRIPT.with(|c| {
            let mut c = c.borrow_mut();
            let Some((start, steps)) = c.as_mut() else { return vec![] };
            let elapsed = start.elapsed().as_secs();
            let (now, later): (Vec<_>, Vec<_>) = steps.drain(..).partition(|(t, _)| *t <= elapsed);
            *steps = later;
            now.into_iter().map(|(_, a)| a).collect()
        });
        for action in due {
            log::info!("party test: {action}");
            match action.as_str() {
                "skip" => self.next(true),
                "pause" if self.backend().get_playing() => self.toggle_play(),
                "play" if !self.backend().get_playing() => self.toggle_play(),
                "leave" | "end" => self.party_leave(None),
                // Host: by display name.
                a if a.starts_with("kick=") || a.starts_with("revoke=") => {
                    let (verb, name) = a.split_once('=').unwrap();
                    let peer = match self.party.as_ref().map(|p| &p.role) {
                        Some(PartyRole::Host(room)) => room.peer_list().into_iter().find(|p| p.name == name).map(|p| p.peer_id),
                        _ => None,
                    };
                    if let Some(peer) = peer {
                        if verb == "kick" { self.party_kick(&peer) } else { self.party_grant_all(&peer, false) }
                    }
                }
                a if a.starts_with("seek=") => {
                    let ms: f32 = a[5..].parse().unwrap_or(0.0);
                    self.seek(ms / 1000.0);
                }
                a if a.starts_with("add=") => {
                    let fp = a[4..].to_string();
                    let r = TrackRef {
                        track_id: fp.clone(),
                        fingerprint: fp,
                        title: "Added by script".into(),
                        artist: String::new(),
                        duration_ms: 0,
                        source: "youtube".into(),
                    };
                    if let Some(t) = track_from_ref(&r) {
                        self.party_intercept(Intercept::QueueAdd(t, false));
                    }
                }
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------------ UI

    /// Everything the party page shows.
    pub(super) fn party_ui(&self) {
        let b = self.backend();
        let Some(party) = &self.party else {
            b.set_party_mode(0);
            b.set_party_phase(s(""));
            b.set_party_peers(ModelRc::new(VecModel::from(Vec::<PartyPeer>::new())));
            b.set_party_queue(ModelRc::new(VecModel::from(Vec::<PartyEntry>::new())));
            b.set_party_unavailable(s(""));
            b.set_party_notice(s(""));
            return;
        };
        b.set_party_code(s(code::display(&party.code)));
        b.set_party_has_password(party.has_password);
        let (mode, peers, queue, me, caps, current): (i32, Vec<PeerInfo>, Vec<QueueEntry>, PeerId, Vec<Cap>, Option<TrackRef>) =
            match &party.role {
                PartyRole::Host(room) => (
                    1,
                    room.peer_list(),
                    room.queue().to_vec(),
                    room.me.clone(),
                    Cap::ALL.to_vec(),
                    self.queue.current().map(track_ref),
                ),
                PartyRole::Guest(room) => (
                    2,
                    room.peers.clone(),
                    room.queue.clone(),
                    room.me.clone(),
                    room.caps.clone(),
                    room.current.as_ref().map(|c| c.track.clone()),
                ),
            };
        b.set_party_mode(mode);
        let phase = match party.phase {
            Phase::Connecting => "connecting",
            Phase::Live => "live",
            Phase::Reconnecting => "reconnecting",
        };
        b.set_party_phase(s(phase));
        let guests = peers.iter().filter(|p| p.role == Role::Guest && p.online).count();
        let status = match (mode, party.phase) {
            (_, Phase::Connecting) => "Looking for the party…".to_string(),
            (_, Phase::Reconnecting) => "Connection lost — reconnecting…".to_string(),
            (1, _) if party.relays == 0 => "Connecting to the party network…".to_string(),
            (1, _) if guests == 0 => "Waiting for guests — share the code".to_string(),
            (1, _) => format!("{guests} guest{} listening", if guests == 1 { "" } else { "s" }),
            _ => {
                let room = self.guest_room();
                let host = room.map(|r| r.host_name()).unwrap_or_default();
                match room.and_then(|r| r.clock.round_trip()) {
                    Some(rtt) => format!("Listening with {host} · {rtt} ms"),
                    None => format!("Listening with {host}"),
                }
            }
        };
        b.set_party_status(s(status));
        let rows: Vec<PartyPeer> = peers
            .iter()
            .map(|p| PartyPeer {
                id: s(&p.peer_id),
                name: s(&p.name),
                host: p.role == Role::Host,
                me: p.peer_id == me,
                online: p.online,
                skip: p.caps.contains(&Cap::Skip),
                seek: p.caps.contains(&Cap::Seek),
                pause: p.caps.contains(&Cap::Pause),
                add: p.caps.contains(&Cap::QueueAdd),
                remove: p.caps.contains(&Cap::QueueRemove),
                caps_text: s(caps_text(&p.caps)),
            })
            .collect();
        b.set_party_peers(ModelRc::new(VecModel::from(rows)));
        let pending_removals = self.guest_room().map(|r| r.pending_removals()).unwrap_or_default();
        let mut entries: Vec<PartyEntry> = queue
            .iter()
            .map(|e| PartyEntry {
                id: s(&e.entry_id),
                title: s(&e.track.title),
                artist: s(&e.track.artist),
                added_by: s(&e.added_by_name),
                mine: e.added_by == me,
                pending: pending_removals.contains(&e.entry_id),
                playable: !e.track.fingerprint.is_empty(),
            })
            .collect();
        // Own additions the host hasn't confirmed yet.
        for t in self.guest_room().map(|r| r.pending_adds()).unwrap_or_default() {
            entries.push(PartyEntry {
                id: s(""),
                title: s(&t.title),
                artist: s(&t.artist),
                added_by: s("You"),
                mine: true,
                pending: true,
                playable: true,
            });
        }
        b.set_party_queue(ModelRc::new(VecModel::from(entries)));
        b.set_party_now_title(s(current.as_ref().map(|t| t.title.clone()).unwrap_or_default()));
        b.set_party_now_artist(s(current.as_ref().map(|t| t.artist.clone()).unwrap_or_default()));
        let notice = match (mode, &current) {
            (1, Some(t)) if t.fingerprint.is_empty() => "Guests can't hear songs from your PC or Nextcloud — they stay in sync but silent.",
            _ => "",
        };
        b.set_party_notice(s(notice));
        b.set_party_unavailable(s(&party.unavailable));
        b.set_party_can_skip(caps.contains(&Cap::Skip));
        b.set_party_can_seek(caps.contains(&Cap::Seek));
        b.set_party_can_pause(caps.contains(&Cap::Pause));
        b.set_party_can_add(caps.contains(&Cap::QueueAdd));
        b.set_party_can_remove(caps.contains(&Cap::QueueRemove));
        self.party_ui_light();
    }

    /// Pending markers (refreshed every tick).
    fn party_ui_light(&self) {
        let b = self.backend();
        let room = self.guest_room();
        b.set_party_skip_pending(room.is_some_and(|r| r.is_pending(Cap::Skip)));
        b.set_party_pause_pending(room.is_some_and(|r| r.is_pending(Cap::Pause)));
        b.set_party_seek_pending(room.is_some_and(|r| r.is_pending(Cap::Seek)));
    }
}

/// Backend callbacks for the party page.
pub(super) fn wire(b: &Backend) {
    use super::with_app;
    b.on_party_create(|pw| {
        with_app(|a| a.party_create(&pw));
    });
    b.on_party_join(|code, pw| {
        with_app(|a| a.party_join(&code, &pw));
    });
    b.on_party_leave(|| {
        with_app(|a| a.party_leave(None));
    });
    b.on_party_copy_code(|| {
        with_app(|a| a.party_copy_code());
    });
    b.on_party_set_cap(|peer, cap, on| {
        with_app(|a| a.party_set_cap(&peer, &cap, on));
    });
    b.on_party_grant_all(|peer, on| {
        with_app(|a| a.party_grant_all(&peer, on));
    });
    b.on_party_kick(|peer| {
        with_app(|a| a.party_kick(&peer));
    });
    b.on_party_save_settings(|| {
        with_app(|a| a.save_party_settings());
    });
    b.on_party_remove_entry(|entry| {
        with_app(|a| {
            let Some(cur) = a.queue.index else { return };
            match a.party.as_ref().map(|p| &p.role) {
                // Host: remove it from the own queue; the change is broadcast as usual.
                Some(PartyRole::Host(room)) => {
                    if let Some(i) = room.queue().iter().position(|e| e.entry_id == entry.as_str()) {
                        a.queue.remove(cur + 1 + i);
                        a.refresh_queue();
                    }
                }
                Some(PartyRole::Guest(_)) => {
                    let i = a.party.as_ref().and_then(|p| p.mirror_ids.iter().position(|id| *id == entry.as_str()));
                    if let Some(i) = i {
                        a.party_intercept(Intercept::QueueRemove(cur + 1 + i));
                    }
                }
                None => {}
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_refs_round_trip() {
        let mut yt = Track::new(SourceKind::YouTube, "dQw4w9WgXcQ");
        yt.title = "Song".into();
        let r = track_ref(&yt);
        assert_eq!(r.fingerprint, "yt:dQw4w9WgXcQ");
        assert_eq!(track_from_ref(&r).unwrap().uri, "dQw4w9WgXcQ");
        let sc = Track::new(SourceKind::SoundCloud, "https://soundcloud.com/a/b");
        let r = track_ref(&sc);
        assert_eq!(r.fingerprint, "sc:a/b");
        assert_eq!(track_from_ref(&r).unwrap().uri, "https://soundcloud.com/a/b");
        let local = Track::new(SourceKind::Local, r"D:\a.mp3");
        let r = track_ref(&local);
        assert!(r.fingerprint.is_empty());
        assert!(track_from_ref(&r).is_none());
    }
}
