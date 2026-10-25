//! The relay side of friends: runs on its own thread with a tokio runtime, like the party
//! transport. It sends and receives gift wraps and publishes and reads presence. The friend
//! list itself lives in the App ([`super::friends::Book`]); the service only knows whose
//! presence to follow and with which keys.

use super::messages::{self, Message};
use super::presence::{self, Presence};
use crate::net::relays::{Filters, RelayPool};
use nostr::prelude::{Event as NostrEvent, Keys, Kind, PublicKey};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub struct Config {
    pub keys: Keys,
    pub relays: Vec<String>,
    /// Gift wraps older than this are not asked for (NIP-59 back-dates them up to 2 days).
    pub since: i64,
}

#[derive(Debug)]
pub enum Event {
    /// A message for us. `id` is the gift wrap's event id, for remembering it was handled.
    Message { from: String, msg: Message, id: String },
    Presence { from: String, presence: Presence },
    Relays(usize),
}

enum Command {
    Send(PublicKey, Message),
    Publish(Presence, [u8; 32]),
    /// Friends whose presence we read, with their keys.
    Follow(Vec<(PublicKey, [u8; 32])>),
    /// Publish this last presence (if any), then stop. The sender is told when it's out.
    Stop(Option<(Presence, [u8; 32])>, std::sync::mpsc::Sender<()>),
}

pub struct Service {
    cmd: mpsc::UnboundedSender<Command>,
}

impl Service {
    pub fn start(config: Config, events: impl Fn(Event) + Send + Sync + 'static) -> Service {
        let (cmd, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("trax-friends".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::error!("friends: no runtime: {e}");
                        return;
                    }
                };
                rt.block_on(run(config, rx, Arc::new(events)));
            })
            .ok();
        Service { cmd }
    }

    pub fn send(&self, to: PublicKey, msg: Message) {
        let _ = self.cmd.send(Command::Send(to, msg));
    }

    pub fn publish(&self, presence: Presence, key: [u8; 32]) {
        let _ = self.cmd.send(Command::Publish(presence, key));
    }

    pub fn follow(&self, friends: Vec<(PublicKey, [u8; 32])>) {
        let _ = self.cmd.send(Command::Follow(friends));
    }

    /// Stop, publishing `last` first. Waits up to `wait` for it to go out.
    pub fn stop(&self, last: Option<(Presence, [u8; 32])>, wait: Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        if self.cmd.send(Command::Stop(last, tx)).is_ok() {
            let _ = rx.recv_timeout(wait);
        }
    }
}

fn filters(me: PublicKey, since: i64, follow: &HashMap<PublicKey, [u8; 32]>) -> Filters {
    let mut list = vec![serde_json::json!({"kinds": [Kind::GiftWrap.as_u16()], "#p": [me.to_hex()], "since": since})];
    if !follow.is_empty() {
        let authors: Vec<String> = follow.keys().map(|pk| pk.to_hex()).collect();
        let tags: Vec<String> = follow.keys().map(presence::d_tag).collect();
        list.push(serde_json::json!({"kinds": [presence::KIND], "authors": authors, "#d": tags}));
    }
    Arc::new(move || list.clone())
}

async fn run(config: Config, mut rx: mpsc::UnboundedReceiver<Command>, events: Arc<dyn Fn(Event) + Send + Sync>) {
    let me = config.keys.public_key();
    let mut follow: HashMap<PublicKey, [u8; 32]> = HashMap::new();
    let (tx, mut incoming) = mpsc::unbounded_channel::<NostrEvent>();
    let pool = RelayPool::start(config.relays, filters(me, config.since, &follow), tx);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut relays = usize::MAX;
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break,
                Some(Command::Send(to, msg)) => match messages::wrap(&config.keys, &to, &msg) {
                    Ok(event) => pool.publish(&event),
                    Err(e) => log::warn!("friends: can't wrap a message: {e}"),
                },
                Some(Command::Publish(p, key)) => match presence::event(&config.keys, &key, &p) {
                    Ok(event) => pool.publish(&event),
                    Err(e) => log::warn!("friends: can't build presence: {e}"),
                },
                Some(Command::Follow(list)) => {
                    let next: HashMap<_, _> = list.into_iter().collect();
                    if next != follow {
                        follow = next;
                        pool.set_filters(filters(me, config.since, &follow));
                    }
                }
                Some(Command::Stop(last, done)) => {
                    if let Some((p, key)) = last {
                        if let Ok(event) = presence::event(&config.keys, &key, &p) {
                            pool.publish(&event);
                            // Give the relay tasks a moment to send it.
                            tokio::time::sleep(Duration::from_millis(800)).await;
                        }
                    }
                    pool.close();
                    let _ = done.send(());
                    break;
                }
            },
            event = incoming.recv() => {
                let Some(event) = event else { break };
                if event.kind == Kind::GiftWrap {
                    if let Some((from, msg)) = messages::unwrap(&config.keys, &event) {
                        events(Event::Message { from: from.to_hex(), msg, id: event.id.to_hex() });
                    }
                } else if let Some(key) = follow.get(&event.pubkey) {
                    if let Some(presence) = presence::read(&event, key) {
                        events(Event::Presence { from: event.pubkey.to_hex(), presence });
                    }
                }
            }
            _ = tick.tick() => {
                let n = pool.connected();
                if n != relays {
                    relays = n;
                    events(Event::Relays(n));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::messages::Body;
    use crate::party::signaling::DEFAULT_RELAYS;
    use std::sync::Mutex;

    /// Uses the real public relays: `cargo test friends_over_relays -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn friends_over_relays() {
        let relays: Vec<String> = DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect();
        let since = crate::net::relays::now() - 3 * 86_400;
        let (alice, bob) = (Keys::generate(), Keys::generate());
        let got: Arc<Mutex<Vec<Event>>> = Arc::default();
        let sink = got.clone();
        let a = Service::start(Config { keys: alice.clone(), relays: relays.clone(), since }, |_| {});
        let b = Service::start(Config { keys: bob.clone(), relays, since }, move |e| sink.lock().unwrap().push(e));
        std::thread::sleep(Duration::from_secs(4));

        let key = presence::new_key();
        b.follow(vec![(alice.public_key(), key)]);
        a.send(bob.public_key(), Message::new(Body::FriendRequest { name: "Alice".into() }));
        let now = crate::net::relays::now();
        a.publish(Presence { name: "Alice".into(), online: true, updated: now, ..Default::default() }, key);

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let (mut message, mut presence) = (false, false);
        while std::time::Instant::now() < deadline && !(message && presence) {
            std::thread::sleep(Duration::from_millis(300));
            for e in got.lock().unwrap().iter() {
                match e {
                    Event::Message { from, msg, .. } => {
                        message |= *from == alice.public_key().to_hex()
                            && msg.body == Body::FriendRequest { name: "Alice".into() }
                    }
                    Event::Presence { from, presence: p } => presence |= *from == alice.public_key().to_hex() && p.online,
                    Event::Relays(n) => println!("bob relays: {n}"),
                }
            }
        }
        println!("message: {message}, presence: {presence}");
        a.stop(None, Duration::from_secs(1));
        b.stop(None, Duration::from_secs(1));
        assert!(message && presence);
    }
}
