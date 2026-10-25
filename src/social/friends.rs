//! The friend list and what messages do to it. Pure state: messages and user actions in,
//! [`Action`]s out (messages to send, rows to save, things to show). The service carries
//! the actions out.

use super::messages::{Body, Message};
use super::presence::{self, Presence};
use crate::party::protocol::clean_name;
use std::collections::BTreeMap;

/// Strangers' requests waiting for an answer; more are ignored.
pub const MAX_PENDING_IN: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// They asked to be friends; waiting for you.
    PendingIn,
    /// You asked; waiting for them.
    PendingOut,
    Friend,
    Blocked,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::PendingIn => "pending_in",
            State::PendingOut => "pending_out",
            State::Friend => "friend",
            State::Blocked => "blocked",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "pending_in" => State::PendingIn,
            "pending_out" => State::PendingOut,
            "friend" => State::Friend,
            "blocked" => State::Blocked,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Friend {
    /// Public key, hex.
    pub pubkey: String,
    pub name: String,
    pub state: State,
    /// Their presence key, once they sent it.
    pub their_key: Option<[u8; 32]>,
    pub presence: Option<Presence>,
    pub added: i64,
    /// Last change of the friendship itself (older remove messages are ignored).
    pub changed: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Send a message to this profile (hex key).
    Send(String, Body),
    /// This row changed or was deleted: write it to the database.
    Save(String),
    /// Our presence key changed: save it and publish presence again.
    KeyRotated,
    /// The set of friends changed (presence subscriptions must follow).
    FriendsChanged,
    /// Something to tell the user.
    Notify(String),
    Invite { from: String, code: String, password: String, host: String, asked: bool },
    JoinRequest { from: String, name: String },
    JoinReply { from: String, accepted: bool },
}

pub struct Book {
    /// Our display name, sent with requests.
    pub name: String,
    /// Our presence key.
    pub my_key: [u8; 32],
    pub friends: BTreeMap<String, Friend>,
}

impl Book {
    pub fn new(name: &str, my_key: [u8; 32], friends: Vec<Friend>) -> Book {
        Book { name: name.into(), my_key, friends: friends.into_iter().map(|f| (f.pubkey.clone(), f)).collect() }
    }

    fn state(&self, pk: &str) -> Option<State> {
        self.friends.get(pk).map(|f| f.state)
    }

    pub fn is_friend(&self, pk: &str) -> bool {
        self.state(pk) == Some(State::Friend)
    }

    /// Keys whose presence we follow.
    pub fn friend_keys(&self) -> Vec<String> {
        self.friends.values().filter(|f| f.state == State::Friend).map(|f| f.pubkey.clone()).collect()
    }

    fn set(&mut self, pk: &str, name: &str, state: State, now: i64) {
        let f = self.friends.entry(pk.to_string()).or_insert_with(|| Friend {
            pubkey: pk.to_string(),
            name: String::new(),
            state,
            their_key: None,
            presence: None,
            added: now,
            changed: now,
        });
        if !name.is_empty() {
            f.name = clean_name(name);
        }
        if f.state != state {
            f.state = state;
            f.changed = now;
        }
        if state != State::Friend {
            f.presence = None;
        }
    }

    fn rotate(&mut self) -> Vec<Action> {
        self.my_key = presence::new_key();
        let key = presence::key_to_string(&self.my_key);
        let mut out: Vec<Action> =
            self.friend_keys().into_iter().map(|pk| Action::Send(pk, Body::PresenceKey { key: key.clone() })).collect();
        out.push(Action::KeyRotated);
        out
    }

    fn befriend(&mut self, pk: &str, name: &str, now: i64) -> Vec<Action> {
        self.set(pk, name, State::Friend, now);
        vec![
            Action::Send(pk.into(), Body::FriendAccept { name: self.name.clone() }),
            Action::Send(pk.into(), Body::PresenceKey { key: presence::key_to_string(&self.my_key) }),
            Action::Save(pk.into()),
            Action::FriendsChanged,
        ]
    }

    // ------------------------------------------------------------------ user actions

    /// You entered someone's friend code.
    pub fn add(&mut self, pk: &str, me: &str, now: i64) -> Result<Vec<Action>, String> {
        if pk == me {
            return Err("That's your own friend code".into());
        }
        match self.state(pk) {
            Some(State::Friend) => Err("You're already friends".into()),
            Some(State::PendingIn) => Ok(self.befriend(pk, "", now)),
            Some(State::PendingOut) | Some(State::Blocked) | None => {
                self.set(pk, "", State::PendingOut, now);
                Ok(vec![Action::Send(pk.into(), Body::FriendRequest { name: self.name.clone() }), Action::Save(pk.into())])
            }
        }
    }

    pub fn accept(&mut self, pk: &str, now: i64) -> Vec<Action> {
        if self.state(pk) != Some(State::PendingIn) {
            return vec![];
        }
        self.befriend(pk, "", now)
    }

    pub fn decline(&mut self, pk: &str) -> Vec<Action> {
        if self.state(pk) != Some(State::PendingIn) {
            return vec![];
        }
        self.friends.remove(pk);
        vec![Action::Send(pk.into(), Body::FriendDecline), Action::Save(pk.into())]
    }

    /// Remove a friend, or withdraw a request you sent.
    pub fn remove(&mut self, pk: &str) -> Vec<Action> {
        let Some(f) = self.friends.remove(pk) else { return vec![] };
        let mut out = vec![Action::Save(pk.into())];
        if matches!(f.state, State::Friend | State::PendingOut) {
            out.insert(0, Action::Send(pk.into(), Body::FriendRemove));
        }
        if f.state == State::Friend {
            out.extend(self.rotate());
            out.push(Action::FriendsChanged);
        }
        out
    }

    pub fn block(&mut self, pk: &str, now: i64) -> Vec<Action> {
        let was = self.state(pk);
        if was == Some(State::Blocked) {
            return vec![];
        }
        self.set(pk, "", State::Blocked, now);
        let mut out = vec![Action::Save(pk.into())];
        if matches!(was, Some(State::Friend) | Some(State::PendingOut)) {
            out.insert(0, Action::Send(pk.into(), Body::FriendRemove));
        }
        if was == Some(State::Friend) {
            out.extend(self.rotate());
            out.push(Action::FriendsChanged);
        }
        out
    }

    pub fn unblock(&mut self, pk: &str) -> Vec<Action> {
        if self.state(pk) != Some(State::Blocked) {
            return vec![];
        }
        self.friends.remove(pk);
        vec![Action::Save(pk.into())]
    }

    /// Our name changed: friends learn it from presence, pending requests get it again.
    pub fn rename(&mut self, name: &str) -> Vec<Action> {
        self.name = name.into();
        self.friends
            .values()
            .filter(|f| f.state == State::PendingOut)
            .map(|f| Action::Send(f.pubkey.clone(), Body::FriendRequest { name: name.into() }))
            .collect()
    }

    // ------------------------------------------------------------------ incoming

    pub fn receive(&mut self, from: &str, msg: &Message, now: i64) -> Vec<Action> {
        let state = self.state(from);
        if state == Some(State::Blocked) {
            return vec![];
        }
        let changed = self.friends.get(from).map(|f| f.changed).unwrap_or(0);
        match &msg.body {
            Body::FriendRequest { name } => match state {
                // Both asked: that's a yes from both.
                Some(State::PendingOut) => {
                    let mut out = self.befriend(from, name, now);
                    out.push(Action::Notify(format!("{} is now your friend", clean_name(name))));
                    out
                }
                // They lost track of us (e.g. restored an older backup): confirm again.
                Some(State::Friend) => self.befriend(from, name, now),
                Some(State::PendingIn) => {
                    self.set(from, name, State::PendingIn, now);
                    vec![Action::Save(from.into())]
                }
                _ => {
                    let pending = self.friends.values().filter(|f| f.state == State::PendingIn).count();
                    if pending >= MAX_PENDING_IN {
                        return vec![];
                    }
                    self.set(from, name, State::PendingIn, now);
                    vec![Action::Save(from.into()), Action::Notify(format!("{} wants to be friends", clean_name(name)))]
                }
            },
            Body::FriendAccept { name } => match state {
                Some(State::PendingOut) => {
                    self.set(from, name, State::Friend, now);
                    vec![
                        Action::Send(from.into(), Body::PresenceKey { key: presence::key_to_string(&self.my_key) }),
                        Action::Save(from.into()),
                        Action::FriendsChanged,
                        Action::Notify(format!("{} accepted your friend request", clean_name(name))),
                    ]
                }
                Some(State::Friend) => {
                    self.set(from, name, State::Friend, now);
                    vec![Action::Save(from.into())]
                }
                // Nobody becomes a friend without being asked.
                _ => vec![],
            },
            Body::FriendDecline => {
                if state == Some(State::PendingOut) {
                    self.friends.remove(from);
                    return vec![Action::Save(from.into())];
                }
                vec![]
            }
            Body::FriendRemove => {
                if state.is_none() || msg.ts < changed {
                    return vec![];
                }
                let was_friend = state == Some(State::Friend);
                self.friends.remove(from);
                let mut out = vec![Action::Save(from.into())];
                if was_friend {
                    out.extend(self.rotate());
                    out.push(Action::FriendsChanged);
                }
                out
            }
            Body::PresenceKey { key } => {
                // Can arrive just before the accept that goes with it.
                if !matches!(state, Some(State::Friend) | Some(State::PendingOut)) {
                    return vec![];
                }
                let Some(key) = presence::key_from_string(key) else { return vec![] };
                let f = self.friends.get_mut(from).unwrap();
                if f.their_key == Some(key) {
                    return vec![];
                }
                f.their_key = Some(key);
                f.presence = None;
                vec![Action::Save(from.into()), Action::FriendsChanged]
            }
            _ if state != Some(State::Friend) => vec![],
            Body::PartyInvite { code, password, host, asked } => vec![Action::Invite {
                from: from.into(),
                code: code.clone(),
                password: password.clone(),
                host: clean_name(host),
                asked: *asked,
            }],
            Body::JoinRequest { name } => vec![Action::JoinRequest { from: from.into(), name: clean_name(name) }],
            Body::JoinReply { accepted } => vec![Action::JoinReply { from: from.into(), accepted: *accepted }],
        }
    }

    /// A friend's presence arrived. Returns true if it changed what we show.
    pub fn set_presence(&mut self, from: &str, presence: Presence) -> bool {
        let Some(f) = self.friends.get_mut(from).filter(|f| f.state == State::Friend) else { return false };
        if f.presence.as_ref().is_some_and(|p| p.updated > presence.updated) {
            return false;
        }
        if !presence.name.is_empty() {
            f.name = clean_name(&presence.name);
        }
        let changed = f.presence.as_ref() != Some(&presence);
        f.presence = Some(presence);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(body: Body) -> Message {
        Message::new(body)
    }

    fn sends(actions: &[Action]) -> Vec<(String, Body)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send(to, b) => Some((to.clone(), b.clone())),
                _ => None,
            })
            .collect()
    }

    /// Delivers everything `actions` sends to `to` (from `from`) and returns `to`'s actions.
    fn deliver(actions: &[Action], from: &str, to: &mut Book, to_pk: &str) -> Vec<Action> {
        sends(actions)
            .into_iter()
            .filter(|(pk, _)| pk == to_pk)
            .flat_map(|(_, b)| to.receive(from, &msg(b), 100))
            .collect()
    }

    fn pair() -> (Book, Book) {
        (Book::new("Alice", presence::new_key(), vec![]), Book::new("Bob", presence::new_key(), vec![]))
    }

    #[test]
    fn request_and_accept() {
        let (mut alice, mut bob) = pair();
        let out = alice.add("bob", "alice", 1).unwrap();
        assert_eq!(alice.friends["bob"].state, State::PendingOut);
        let back = deliver(&out, "alice", &mut bob, "bob");
        assert_eq!(bob.friends["alice"].state, State::PendingIn);
        assert_eq!(bob.friends["alice"].name, "Alice");
        assert!(back.contains(&Action::Notify("Alice wants to be friends".into())));

        let out = bob.accept("alice", 2);
        assert!(bob.is_friend("alice"));
        let back = deliver(&out, "bob", &mut alice, "alice");
        assert!(alice.is_friend("bob"));
        assert_eq!(alice.friends["bob"].name, "Bob");
        // Both now hold the other's presence key.
        assert_eq!(alice.friends["bob"].their_key, Some(bob.my_key));
        deliver(&back, "alice", &mut bob, "bob");
        assert_eq!(bob.friends["alice"].their_key, Some(alice.my_key));
        assert_eq!(alice.friend_keys(), vec!["bob".to_string()]);
    }

    #[test]
    fn both_asking_makes_friends() {
        let (mut alice, mut bob) = pair();
        let a = alice.add("bob", "alice", 1).unwrap();
        let b = bob.add("alice", "bob", 1).unwrap();
        deliver(&a, "alice", &mut bob, "bob");
        deliver(&b, "bob", &mut alice, "alice");
        assert!(alice.is_friend("bob") && bob.is_friend("alice"));
    }

    #[test]
    fn unasked_accepts_and_keys_are_ignored() {
        let (mut alice, _) = pair();
        assert!(alice.receive("mallory", &msg(Body::FriendAccept { name: "M".into() }), 1).is_empty());
        assert!(alice.receive("mallory", &msg(Body::PresenceKey { key: presence::key_to_string(&[1; 32]) }), 1).is_empty());
        let invite = Body::PartyInvite { code: "C".into(), password: String::new(), host: "M".into(), asked: false };
        assert!(alice.receive("mallory", &msg(invite), 1).is_empty());
        assert!(alice.friends.is_empty());
    }

    #[test]
    fn decline_and_own_code() {
        let (mut alice, mut bob) = pair();
        assert!(alice.add("alice", "alice", 1).is_err());
        let out = alice.add("bob", "alice", 1).unwrap();
        deliver(&out, "alice", &mut bob, "bob");
        let out = bob.decline("alice");
        assert!(!bob.friends.contains_key("alice"));
        deliver(&out, "bob", &mut alice, "alice");
        assert!(!alice.friends.contains_key("bob"));
    }

    #[test]
    fn removing_rotates_the_key() {
        let (mut alice, mut bob) = pair();
        let out = alice.add("bob", "alice", 1).unwrap();
        deliver(&out, "alice", &mut bob, "bob");
        let out = bob.accept("alice", 2);
        let back = deliver(&out, "bob", &mut alice, "alice");
        deliver(&back, "alice", &mut bob, "bob");
        // Carol is a friend of Alice too.
        alice.set("carol", "Carol", State::Friend, 3);

        let old = alice.my_key;
        let out = alice.remove("bob");
        assert_ne!(alice.my_key, old);
        let s = sends(&out);
        assert!(s.contains(&("bob".into(), Body::FriendRemove)));
        // Only remaining friends get the new key.
        assert!(s.iter().any(|(pk, b)| pk == "carol" && matches!(b, Body::PresenceKey { .. })));
        assert!(!s.iter().any(|(pk, b)| pk == "bob" && matches!(b, Body::PresenceKey { .. })));
        assert!(out.contains(&Action::KeyRotated));

        // Bob's side forgets Alice and rotates his own key.
        let bob_old = bob.my_key;
        deliver(&out, "alice", &mut bob, "bob");
        assert!(!bob.friends.contains_key("alice"));
        assert_ne!(bob.my_key, bob_old);
    }

    #[test]
    fn old_removes_are_ignored() {
        let (mut alice, _) = pair();
        alice.set("bob", "Bob", State::Friend, 1000);
        let mut remove = msg(Body::FriendRemove);
        remove.ts = 999;
        assert!(alice.receive("bob", &remove, 1001).is_empty());
        assert!(alice.is_friend("bob"));
    }

    #[test]
    fn blocking() {
        let (mut alice, _) = pair();
        alice.set("bob", "Bob", State::Friend, 1);
        let out = alice.block("bob", 2);
        assert!(sends(&out).contains(&("bob".into(), Body::FriendRemove)));
        assert_eq!(alice.friends["bob"].state, State::Blocked);
        assert!(alice.receive("bob", &msg(Body::FriendRequest { name: "Bob".into() }), 3).is_empty());
        assert_eq!(alice.friends["bob"].state, State::Blocked);
        alice.unblock("bob");
        assert!(!alice.friends.contains_key("bob"));
    }

    #[test]
    fn pending_requests_are_capped() {
        let (mut alice, _) = pair();
        for i in 0..MAX_PENDING_IN + 5 {
            alice.receive(&format!("p{i}"), &msg(Body::FriendRequest { name: "x".into() }), 1);
        }
        assert_eq!(alice.friends.len(), MAX_PENDING_IN);
    }

    #[test]
    fn party_messages_from_friends() {
        let (mut alice, _) = pair();
        alice.set("bob", "Bob", State::Friend, 1);
        let out = alice.receive("bob", &msg(Body::JoinRequest { name: "Bob".into() }), 2);
        assert_eq!(out, vec![Action::JoinRequest { from: "bob".into(), name: "Bob".into() }]);
        let invite = Body::PartyInvite { code: "C".into(), password: "p".into(), host: "Bob".into(), asked: true };
        let out = alice.receive("bob", &msg(invite), 2);
        assert!(matches!(&out[0], Action::Invite { asked: true, .. }));
    }

    #[test]
    fn presence_only_from_friends_and_newer() {
        let (mut alice, _) = pair();
        alice.set("bob", "Bob", State::Friend, 1);
        let p = |updated| Presence { name: "Bobby".into(), online: true, updated, ..Default::default() };
        assert!(alice.set_presence("bob", p(10)));
        assert_eq!(alice.friends["bob"].name, "Bobby");
        assert!(!alice.set_presence("bob", p(5)));
        assert!(!alice.set_presence("stranger", p(20)));
    }
}
