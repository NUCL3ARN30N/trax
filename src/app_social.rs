//! Friends inside the app: runs the friends service while "Friends" is on, applies what the
//! friend list decides (`crate::social::friends`), publishes what you're listening to, and
//! turns invites and join requests into banners and party joins.

use super::party_app::{track_from_ref, track_ref};
use super::{App, post, s};
use crate::party::protocol::clean_name;
use crate::social::friends::{Action, Book, Friend, State};
use crate::social::messages::{Body, LIVE_SECS, Message, Replay};
use crate::social::presence::{self, HEARTBEAT_SECS, Playing, Presence};
use crate::social::profile;
use crate::social::service::{Config, Event, Service};
use crate::{Backend, FriendItem, FriendRequestItem};
use nostr::prelude::{Keys, PublicKey};
use slint::{ModelRc, VecModel};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Changes to what you play are published after this, so skipping through songs sends one
/// update instead of many.
const DEBOUNCE: Duration = Duration::from_secs(3);
/// Avatar colours in the UI (`AVATAR-COLORS` in friends.slint).
const AVATAR_COLORS: usize = 8;

/// An invite or join request waiting in the banner.
#[derive(Clone)]
pub(super) struct Pending {
    from: String,
    name: String,
    /// Some(code, password) for invites; None for join requests.
    invite: Option<(String, String)>,
    at: i64,
}

pub(super) struct Social {
    id: u64,
    service: Service,
    book: Book,
    replay: Replay,
    relays: usize,
    /// What was published last (`updated` set to 0), to see when something changed.
    published: Option<Presence>,
    published_at: Instant,
    publish_due: Option<Instant>,
    banners: Vec<Pending>,
    /// Friends we asked to join, and when.
    asked: HashMap<String, i64>,
}

fn now() -> i64 {
    crate::net::relays::now()
}

fn pubkey(hex: &str) -> Option<PublicKey> {
    PublicKey::from_hex(hex).ok()
}

/// "5 min ago", "3 h ago", "2 days ago".
fn ago(secs: i64) -> String {
    match secs {
        ..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86_400),
    }
}

impl App {
    // ------------------------------------------------------------------ profile

    /// The profile keys, created and saved on first use.
    fn profile_keys(&mut self) -> Keys {
        if let Some(k) = &self.profile {
            return k.clone();
        }
        let db = self.db.lock().unwrap();
        let (keys, created) = profile::load_or_create(&db.setting_or("profile_secret", ""));
        if created {
            db.set_setting("profile_secret", &profile::secret_hex(&keys));
        }
        drop(db);
        self.profile = Some(keys.clone());
        keys
    }

    fn my_name(&self) -> String {
        self.backend().get_party_name().trim().to_string()
    }

    pub(super) fn load_social_settings(&mut self) {
        let keys = self.profile_keys();
        let (enabled, share, offline) = {
            let db = self.db.lock().unwrap();
            (
                db.setting_bool("friends_enabled", false),
                db.setting_bool("friends_share_playing", true),
                db.setting_bool("friends_appear_offline", false),
            )
        };
        let b = self.backend();
        let pk = keys.public_key();
        b.set_profile_code(s(profile::friend_code(&pk)));
        b.set_profile_tag(s(profile::tag(&pk)));
        b.set_profile_color(profile::color_index(&pk, AVATAR_COLORS) as i32);
        b.set_friends_enabled(enabled);
        b.set_friends_share_playing(share);
        b.set_friends_appear_offline(offline);
        if enabled {
            self.social_start();
        }
        self.social_ui();
    }

    pub(super) fn social_copy_code(&mut self) {
        let code = self.backend().get_profile_code().to_string();
        match super::share_ui::copy_text(&code) {
            Ok(()) => self.toast("Friend code copied"),
            Err(e) => self.toast(format!("Couldn't copy: {e}")),
        }
    }

    /// New keys: friends no longer know you and have to add you again.
    pub(super) fn social_reset_profile(&mut self) {
        let running = self.social.is_some();
        // Tell friends, so you don't linger in their lists.
        let keys: Vec<String> = self.social.as_ref().map(|s| s.book.friends.keys().cloned().collect()).unwrap_or_default();
        for pk in keys {
            let actions = self.social.as_mut().map(|s| s.book.remove(&pk)).unwrap_or_default();
            self.social_apply(actions);
        }
        // Waits a moment, so the goodbyes go out.
        self.social_stop(true);
        {
            let db = self.db.lock().unwrap();
            for f in db.friends() {
                db.delete_friend(&f.pubkey);
            }
            db.set_setting("profile_secret", "");
            db.set_setting("presence_key", "");
        }
        self.profile = None;
        self.load_social_settings();
        if running && self.social.is_none() {
            self.social_start();
        }
        self.toast("You have a new profile. Friends need your new friend code.");
    }

    /// Our profile for a party, while friends are on (without friends it stays private).
    pub(super) fn party_profile(&mut self) -> Option<Keys> {
        self.backend().get_friends_enabled().then(|| self.profile_keys())
    }

    pub(super) fn social_is_friend(&self, profile: &str) -> bool {
        !profile.is_empty() && self.social.as_ref().is_some_and(|s| s.book.is_friend(profile))
    }

    /// The tag of a proven party member's profile.
    pub(super) fn social_tag(&self, profile: &str) -> String {
        pubkey(profile).map(|pk| profile::tag(&pk)).unwrap_or_default()
    }

    // ------------------------------------------------------------------ on / off

    pub(super) fn social_set_enabled(&mut self, on: bool) {
        if on && self.my_name().is_empty() {
            self.backend().set_friends_enabled(false);
            self.backend().set_friends_error(s("Set your display name first"));
            return;
        }
        self.backend().set_friends_error(s(""));
        self.db.lock().unwrap().set_setting_bool("friends_enabled", on);
        self.backend().set_friends_enabled(on);
        if on {
            self.social_start();
        } else {
            self.social_stop(true);
        }
        self.social_ui();
    }

    pub(super) fn social_save_prefs(&mut self) {
        let b = self.backend();
        let db = self.db.lock().unwrap();
        db.set_setting_bool("friends_share_playing", b.get_friends_share_playing());
        db.set_setting_bool("friends_appear_offline", b.get_friends_appear_offline());
        drop(db);
        // Publish right away; "appear offline" shouldn't wait.
        if let Some(social) = self.social.as_mut() {
            social.publish_due = Some(Instant::now());
        }
    }

    fn social_start(&mut self) {
        if self.social.is_some() || self.my_name().is_empty() {
            return;
        }
        let keys = self.profile_keys();
        let t = now();
        let (my_key, friends, since) = {
            let db = self.db.lock().unwrap();
            let key = presence::key_from_string(&db.setting_or("presence_key", "")).unwrap_or_else(|| {
                let k = presence::new_key();
                db.set_setting("presence_key", &presence::key_to_string(&k));
                k
            });
            // Ask for messages since we were last online. NIP-59 back-dates gift wraps by
            // up to two days, so look that much further back (at most 30 days in all).
            let last: i64 = db.setting_or("social_last_online", "0").parse().unwrap_or(0);
            let since = (last.min(t) - 2 * 86_400 - 3600).max(t - 30 * 86_400);
            db.prune_seen(t - 35 * 86_400);
            (key, db.friends(), since)
        };
        self.social_ids += 1;
        let id = self.social_ids;
        let config = Config { keys, relays: self.relay_list(), since };
        let service = Service::start(config, move |e| post(move |a| a.social_event(id, e)));
        let mut social = Social {
            id,
            service,
            book: Book::new(&self.my_name(), my_key, friends),
            replay: Replay::default(),
            relays: 0,
            published: None,
            published_at: Instant::now(),
            publish_due: Some(Instant::now()),
            banners: vec![],
            asked: HashMap::new(),
        };
        // Cached presence is from the last run: shown as "last seen", never as online.
        for f in social.book.friends.values_mut() {
            if let Some(p) = f.presence.as_mut() {
                p.online = p.online && p.is_online(t);
            }
        }
        social.service.follow(follow_list(&social.book));
        self.social = Some(social);
    }

    /// `offline`: tell friends we're gone (switching off, quitting).
    pub(super) fn social_stop(&mut self, offline: bool) {
        let Some(social) = self.social.take() else { return };
        let last = offline.then(|| {
            let mut p = self.current_presence();
            p.online = false;
            p.playing = None;
            p.party = None;
            (p, social.book.my_key)
        });
        social.service.stop(last, Duration::from_millis(1500));
        self.db.lock().unwrap().set_setting("social_last_online", &now().to_string());
    }

    /// The display name changed.
    pub(super) fn social_renamed(&mut self) {
        let name = self.my_name();
        let Some(social) = self.social.as_mut() else { return };
        let actions = social.book.rename(&name);
        social.publish_due = Some(Instant::now());
        self.social_apply(actions);
    }

    // ------------------------------------------------------------------ presence

    fn current_presence(&self) -> Presence {
        let b = self.backend();
        let share = b.get_friends_share_playing();
        let playing = self.queue.current().filter(|_| share).map(|t| {
            let r = track_ref(t);
            Playing {
                title: t.title.clone(),
                artist: t.display_artist().to_string(),
                source: r.source.clone(),
                paused: !b.get_playing(),
                track: (!r.fingerprint.is_empty()).then_some(r),
            }
        });
        Presence {
            name: self.my_name(),
            online: !b.get_friends_appear_offline(),
            playing,
            party: self.party_role_name().map(str::to_string),
            updated: 0,
        }
    }

    /// Timer hook: publish presence when it changed (debounced) and as a heartbeat.
    pub(super) fn social_tick(&mut self) {
        if self.social.is_none() {
            return;
        }
        let current = self.current_presence();
        let social = self.social.as_mut().unwrap();
        let t = Instant::now();
        if social.published.as_ref() != Some(&current) && social.publish_due.is_none() {
            social.publish_due = Some(t + DEBOUNCE);
        }
        // Appearing offline means no heartbeat either.
        let heartbeat = current.online && social.published_at.elapsed() >= Duration::from_secs(HEARTBEAT_SECS as u64);
        if social.publish_due.is_some_and(|due| t >= due) || heartbeat {
            let mut p = current.clone();
            p.updated = now();
            social.service.publish(p, social.book.my_key);
            social.published = Some(current);
            social.published_at = t;
            social.publish_due = None;
            self.db.lock().unwrap().set_setting("social_last_online", &now().to_string());
        }
        #[cfg(debug_assertions)]
        self.social_debug_tick();
        // Expired banners and "asked" marks, and "last seen" texts, every 10 seconds.
        if self.ticks % 40 == 0 {
            let social = self.social.as_mut().unwrap();
            let t = now();
            social.banners.retain(|b| t - b.at < LIVE_SECS);
            social.asked.retain(|_, at| t - *at < LIVE_SECS);
            self.social_ui();
        }
    }

    // ------------------------------------------------------------------ events

    fn social_event(&mut self, id: u64, event: Event) {
        if self.social.as_ref().map(|s| s.id) != Some(id) {
            return;
        }
        match event {
            Event::Message { from, msg, id } => {
                if !self.db.lock().unwrap().mark_seen(&id, now()) {
                    return;
                }
                let social = self.social.as_mut().unwrap();
                if !social.replay.fresh(&msg, now()) {
                    return;
                }
                let actions = social.book.receive(&from, &msg, now());
                self.social_apply(actions);
            }
            Event::Presence { from, presence } => {
                let social = self.social.as_mut().unwrap();
                if social.book.set_presence(&from, presence) {
                    // Hosting ended: an open "asked" mark no longer applies.
                    if !social.book.friends[&from].presence.as_ref().is_some_and(|p| p.is_hosting(now())) {
                        social.asked.remove(&from);
                    }
                    if let Some(f) = social.book.friends.get(&from) {
                        self.db.lock().unwrap().save_friend(f);
                    }
                }
            }
            Event::Relays(n) => {
                self.social.as_mut().unwrap().relays = n;
            }
        }
        self.social_ui();
    }

    fn social_send(&self, to: &str, body: Body) {
        if let (Some(social), Some(pk)) = (&self.social, pubkey(to)) {
            social.service.send(pk, Message::new(body));
        }
    }

    fn social_apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send(to, body) => self.social_send(&to, body),
                Action::Save(pk) => {
                    let Some(social) = &self.social else { continue };
                    let db = self.db.lock().unwrap();
                    match social.book.friends.get(&pk) {
                        Some(f) => db.save_friend(f),
                        None => db.delete_friend(&pk),
                    }
                }
                Action::KeyRotated => {
                    let Some(social) = self.social.as_mut() else { continue };
                    self.db.lock().unwrap().set_setting("presence_key", &presence::key_to_string(&social.book.my_key));
                    social.published = None;
                    social.publish_due = Some(Instant::now());
                }
                Action::FriendsChanged => {
                    if let Some(social) = &self.social {
                        social.service.follow(follow_list(&social.book));
                    }
                }
                Action::Notify(text) => self.toast(text),
                Action::Invite { from, code, password, host, asked } => {
                    let we_asked = self.social.as_mut().is_some_and(|s| s.asked.remove(&from).is_some());
                    if asked && we_asked && self.party.is_none() {
                        self.toast(format!("{host} let you in. Joining the party…"));
                        self.party_join(&code, &password);
                        self.show_page(6);
                    } else if let Some(social) = self.social.as_mut() {
                        social.banners.retain(|b| b.from != from);
                        social.banners.push(Pending { from, name: host.clone(), invite: Some((code, password)), at: now() });
                        self.toast(format!("{host} invites you to a listening party"));
                    }
                }
                Action::JoinRequest { from, name } => {
                    if self.party_hosted().is_none() {
                        self.social_send(&from, Body::JoinReply { accepted: false });
                        continue;
                    }
                    if let Some(social) = self.social.as_mut() {
                        social.banners.retain(|b| b.from != from);
                        social.banners.push(Pending { from, name: name.clone(), invite: None, at: now() });
                        self.toast(format!("{name} wants to join your party"));
                    }
                }
                Action::JoinReply { from, accepted } => {
                    let Some(social) = self.social.as_mut() else { continue };
                    social.asked.remove(&from);
                    let name = social.book.friends.get(&from).map(|f| f.name.clone()).unwrap_or_default();
                    if !accepted {
                        self.toast(format!("{name} can't let you in right now"));
                    }
                }
            }
        }
        self.social_ui();
    }

    // ------------------------------------------------------------------ user actions

    /// True if the request went out (the field is cleared then).
    pub(super) fn social_add(&mut self, code: &str) -> bool {
        let Some(pk) = profile::parse_code(code) else {
            self.backend().set_friends_error(s("That isn't a friend code (it starts with npub1)"));
            return false;
        };
        let me = self.profile_keys().public_key().to_hex();
        let Some(social) = self.social.as_mut() else { return false };
        match social.book.add(&pk.to_hex(), &me, now()) {
            Ok(actions) => {
                self.backend().set_friends_error(s(""));
                self.social_apply(actions);
                self.toast("Friend request sent");
                true
            }
            Err(e) => {
                self.backend().set_friends_error(s(e));
                false
            }
        }
    }

    pub(super) fn social_friend_action(&mut self, pk: &str, what: &str) {
        let Some(social) = self.social.as_mut() else { return };
        let actions = match what {
            "accept" => social.book.accept(pk, now()),
            "decline" => social.book.decline(pk),
            "remove" => social.book.remove(pk),
            "block" => social.book.block(pk, now()),
            "unblock" => social.book.unblock(pk),
            _ => vec![],
        };
        self.social_apply(actions);
    }

    pub(super) fn social_invite(&mut self, pk: &str) {
        let Some((code, password)) = self.party_hosted() else { return };
        let host = self.my_name();
        self.social_send(pk, Body::PartyInvite { code, password, host, asked: false });
        let name = self.social.as_ref().and_then(|s| s.book.friends.get(pk)).map(|f| f.name.clone()).unwrap_or_default();
        self.toast(format!("Invite sent to {name}"));
    }

    pub(super) fn social_ask_join(&mut self, pk: &str) {
        let name = self.my_name();
        self.social_send(pk, Body::JoinRequest { name });
        if let Some(social) = self.social.as_mut() {
            social.asked.insert(pk.to_string(), now());
        }
        self.social_ui();
    }

    /// Play the song a friend is listening to, next in the queue.
    pub(super) fn social_play(&mut self, pk: &str) {
        let track = self
            .social
            .as_ref()
            .and_then(|s| s.book.friends.get(pk))
            .and_then(|f| f.presence.as_ref())
            .and_then(|p| p.playing.as_ref())
            .and_then(|p| p.track.as_ref())
            .and_then(track_from_ref);
        let Some(track) = track else { return };
        if self.party_is_guest() {
            self.toast("You're in a party; the host picks the music");
            return;
        }
        let start = match self.queue.index {
            Some(i) => {
                self.queue.tracks.insert(i + 1, track);
                i + 1
            }
            None => {
                self.queue.set(vec![track], 0);
                0
            }
        };
        self.queue.index = Some(start);
        self.start_current_with(false);
    }

    /// The banner's buttons. `accept` false = dismiss / decline.
    pub(super) fn social_banner(&mut self, accept: bool) {
        let Some(social) = self.social.as_mut() else { return };
        if social.banners.is_empty() {
            return;
        }
        let pending = social.banners.remove(0);
        match (pending.invite, accept) {
            (Some((code, password)), true) => {
                if self.party.is_some() {
                    self.toast("Leave your current party first");
                    self.social.as_mut().unwrap().banners.insert(0, Pending { invite: Some((code, password)), ..pending });
                } else {
                    self.party_join(&code, &password);
                    self.show_page(6);
                }
            }
            (Some(_), false) => {}
            (None, true) => match self.party_hosted() {
                Some((code, password)) => {
                    let host = self.my_name();
                    self.social_send(&pending.from, Body::PartyInvite { code, password, host, asked: true });
                }
                None => self.toast("You're not hosting a party any more"),
            },
            (None, false) => self.social_send(&pending.from, Body::JoinReply { accepted: false }),
        }
        self.social_ui();
    }

    // ------------------------------------------------------------------ test automation

    /// Debug builds only, for testing two instances (each with its own `TRAX_DATA_DIR`):
    /// `TRAX_FRIEND_NAME=<name>` sets the name and turns friends on, `TRAX_FRIEND_CODE_FILE`
    /// writes the friend code to a file, `TRAX_FRIEND_ADD=<code>` sends a request.
    #[cfg(debug_assertions)]
    pub(super) fn social_debug_autostart(&mut self) {
        if let Ok(name) = std::env::var("TRAX_FRIEND_NAME") {
            self.backend().set_party_name(s(&name));
            self.db.lock().unwrap().set_setting("party_name", &name);
            self.social_set_enabled(true);
        }
        if let Ok(path) = std::env::var("TRAX_FRIEND_CODE_FILE") {
            let _ = std::fs::write(path, self.backend().get_profile_code().as_str());
        }
        if let Ok(code) = std::env::var("TRAX_FRIEND_ADD") {
            self.social_add(&code);
        }
    }

    /// `TRAX_FRIEND_ACCEPT_ALL=1` accepts requests and answers banners with yes;
    /// `TRAX_FRIEND_ASK=1` asks to join every friend who hosts. Logs the friend list.
    #[cfg(debug_assertions)]
    fn social_debug_tick(&mut self) {
        let Some(social) = self.social.as_ref() else { return };
        if std::env::var("TRAX_FRIEND_ACCEPT_ALL").is_ok() {
            let pending: Vec<String> =
                social.book.friends.values().filter(|f| f.state == State::PendingIn).map(|f| f.pubkey.clone()).collect();
            for pk in pending {
                log::info!("friends-debug: accepting {pk}");
                self.social_friend_action(&pk, "accept");
            }
            if self.social.as_ref().is_some_and(|s| !s.banners.is_empty()) {
                log::info!("friends-debug: answering the banner with yes");
                self.social_banner(true);
            }
        }
        if std::env::var("TRAX_FRIEND_ASK").is_ok() && self.party.is_none() {
            let t = now();
            let hosts: Vec<String> = self
                .social
                .as_ref()
                .map(|s| {
                    s.book
                        .friends
                        .values()
                        .filter(|f| f.presence.as_ref().is_some_and(|p| p.is_hosting(t)) && !s.asked.contains_key(&f.pubkey))
                        .map(|f| f.pubkey.clone())
                        .collect()
                })
                .unwrap_or_default();
            for pk in hosts {
                log::info!("friends-debug: asking {pk} to join");
                self.social_ask_join(&pk);
            }
        }
        if self.ticks % 20 == 0 {
            let b = self.backend();
            use slint::Model;
            for f in b.get_friends().iter() {
                log::info!("friends-debug: friend {} · {} — {}", f.name, f.tag, f.status);
            }
            for r in b.get_friend_requests().iter() {
                log::info!("friends-debug: request {} kind {}", r.name, r.kind);
            }
            log::info!("friends-debug: {} · party mode {}", b.get_friends_status(), b.get_party_mode());
        }
    }

    // ------------------------------------------------------------------ UI

    pub(super) fn social_ui(&self) {
        let b = self.backend();
        let Some(social) = &self.social else {
            b.set_friends_running(false);
            b.set_friends_status(s(""));
            b.set_friends(ModelRc::new(VecModel::from(Vec::<FriendItem>::new())));
            b.set_friend_requests(ModelRc::new(VecModel::from(Vec::<FriendRequestItem>::new())));
            b.set_banner_kind(0);
            return;
        };
        b.set_friends_running(true);
        b.set_profile_initials(s(profile::initials(&self.my_name())));
        b.set_friends_status(s(match social.relays {
            0 => "Connecting to relays…".to_string(),
            1 => "Connected to 1 relay".to_string(),
            n => format!("Connected to {n} relays"),
        }));
        let t = now();
        let hosting = self.party_hosted().is_some();
        let in_party = self.party.is_some();
        let guest = self.party_is_guest();

        let mut friends: Vec<&Friend> = social.book.friends.values().filter(|f| f.state == State::Friend).collect();
        let online = |f: &Friend| f.presence.as_ref().is_some_and(|p| p.is_online(t));
        friends.sort_by_key(|f| (!online(f), f.name.to_lowercase()));
        let items: Vec<FriendItem> = friends
            .into_iter()
            .map(|f| {
                let pk = pubkey(&f.pubkey);
                let p = f.presence.as_ref();
                let is_online = online(f);
                let is_hosting = p.is_some_and(|p| p.is_hosting(t));
                let song = p.and_then(|p| p.playing.as_ref()).map(|pl| {
                    let what = if pl.artist.is_empty() { pl.title.clone() } else { format!("{} — {}", pl.title, pl.artist) };
                    if pl.paused { format!("Paused: {what}") } else { format!("Listening to {what}") }
                });
                let status = if !is_online {
                    match p {
                        Some(p) if p.updated > 0 => format!("Last seen {}", ago(t - p.updated)),
                        _ => "Offline".into(),
                    }
                } else if is_hosting {
                    match &song {
                        Some(song) => format!("Hosting a party · {song}"),
                        None => "Hosting a party".into(),
                    }
                } else if p.and_then(|p| p.party.as_deref()) == Some("guest") {
                    "In a listening party".into()
                } else {
                    song.unwrap_or_else(|| "Online".into())
                };
                let can_play = is_online
                    && !guest
                    && p.and_then(|p| p.playing.as_ref()).is_some_and(|pl| pl.track.is_some());
                FriendItem {
                    id: s(&f.pubkey),
                    name: s(display_name(f)),
                    tag: s(pk.map(|pk| profile::tag(&pk)).unwrap_or_default()),
                    initials: s(profile::initials(&display_name(f))),
                    color: pk.map(|pk| profile::color_index(&pk, AVATAR_COLORS) as i32).unwrap_or(0),
                    online: is_online,
                    status: s(status),
                    can_invite: hosting && is_online,
                    can_ask: is_hosting && !in_party,
                    asked: social.asked.contains_key(&f.pubkey),
                    can_play,
                }
            })
            .collect();
        b.set_friends(ModelRc::new(VecModel::from(items)));

        let mut requests: Vec<&Friend> = social
            .book
            .friends
            .values()
            .filter(|f| matches!(f.state, State::PendingIn | State::PendingOut | State::Blocked))
            .collect();
        requests.sort_by_key(|f| (f.state != State::PendingIn, f.state == State::Blocked, std::cmp::Reverse(f.changed)));
        let requests: Vec<FriendRequestItem> = requests
            .into_iter()
            .map(|f| {
                let pk = pubkey(&f.pubkey);
                FriendRequestItem {
                    id: s(&f.pubkey),
                    name: s(display_name(f)),
                    tag: s(pk.map(|pk| profile::tag(&pk)).unwrap_or_default()),
                    // No name yet (a request you sent): no letters from the code.
                    initials: s(if f.name.is_empty() { "?".into() } else { profile::initials(&f.name) }),
                    color: pk.map(|pk| profile::color_index(&pk, AVATAR_COLORS) as i32).unwrap_or(0),
                    kind: match f.state {
                        State::PendingIn => 0,
                        State::PendingOut => 1,
                        _ => 2,
                    },
                }
            })
            .collect();
        b.set_friend_requests(ModelRc::new(VecModel::from(requests)));

        match social.banners.first() {
            Some(p) => {
                b.set_banner_kind(if p.invite.is_some() { 1 } else { 2 });
                b.set_banner_text(s(match p.invite {
                    Some(_) => format!("{} invites you to their listening party", p.name),
                    None => format!("{} wants to join your party", p.name),
                }));
                let pk = pubkey(&p.from);
                b.set_banner_initials(s(profile::initials(&p.name)));
                b.set_banner_color(pk.map(|pk| profile::color_index(&pk, AVATAR_COLORS) as i32).unwrap_or(0));
            }
            None => b.set_banner_kind(0),
        }
    }
}

/// A request you sent shows the code until the other side answers with a name.
fn display_name(f: &Friend) -> String {
    if f.name.is_empty() {
        let code = pubkey(&f.pubkey).map(|pk| profile::friend_code(&pk)).unwrap_or_default();
        format!("{}…", code.chars().take(14).collect::<String>())
    } else {
        clean_name(&f.name)
    }
}

/// Friends whose presence we can read.
fn follow_list(book: &Book) -> Vec<(PublicKey, [u8; 32])> {
    book.friends
        .values()
        .filter(|f| f.state == State::Friend)
        .filter_map(|f| Some((pubkey(&f.pubkey)?, f.their_key?)))
        .collect()
}

pub(super) fn wire(b: &Backend) {
    use super::with_app;
    b.on_friends_set_enabled(|on| {
        with_app(|a| a.social_set_enabled(on));
    });
    b.on_friends_save_prefs(|| {
        with_app(|a| a.social_save_prefs());
    });
    b.on_friend_add(|code| with_app(|a| a.social_add(&code)).unwrap_or(false));
    b.on_friend_action(|pk, what| {
        with_app(|a| a.social_friend_action(&pk, &what));
    });
    b.on_friend_invite(|pk| {
        with_app(|a| a.social_invite(&pk));
    });
    b.on_friend_ask_join(|pk| {
        with_app(|a| a.social_ask_join(&pk));
    });
    b.on_friend_play(|pk| {
        with_app(|a| a.social_play(&pk));
    });
    b.on_profile_copy_code(|| {
        with_app(|a| a.social_copy_code());
    });
    b.on_profile_reset(|| {
        with_app(|a| a.social_reset_profile());
    });
    b.on_banner_answer(|accept| {
        with_app(|a| a.social_banner(accept));
    });
}
