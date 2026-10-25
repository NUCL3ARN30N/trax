//! A pool of Nostr relay connections: publish to all of them, receive from any.
//!
//! Each relay runs in its own task and reconnects with backoff. What arrives is checked
//! (signature, one copy per event id) and handed on; what it means is up to the caller
//! (party signaling, friends). Must be started inside a tokio runtime.

use nostr::prelude::Event;
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};

/// Builds the subscription filters, called on every (re)connect so `since` stays current.
pub type Filters = Arc<dyn Fn() -> Vec<serde_json::Value> + Send + Sync>;

#[derive(Clone)]
pub struct RelayPool {
    out: broadcast::Sender<String>,
    connected: Arc<Mutex<HashSet<String>>>,
    filters: Arc<Mutex<Filters>>,
    resubscribe: Arc<watch::Sender<u64>>,
}

/// Remembers recent event ids, so the copy from every relay is delivered once.
#[derive(Default)]
struct Seen {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl Seen {
    fn insert(&mut self, id: String) -> bool {
        if !self.ids.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > 5000 {
            if let Some(old) = self.order.pop_front() {
                self.ids.remove(&old);
            }
        }
        true
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl RelayPool {
    /// Connect to all relays (in the background) and deliver verified events to `incoming`.
    /// The pool stops once `incoming`'s receiver is dropped and the last handle is gone.
    pub fn start(relays: Vec<String>, filters: Filters, incoming: mpsc::UnboundedSender<Event>) -> RelayPool {
        // Several TLS backends are compiled in (WebRTC and HTTP pull in different ones), so
        // rustls needs one picked for the relay websockets.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let (out, _) = broadcast::channel(256);
        let (resubscribe, _) = watch::channel(0u64);
        let pool = RelayPool {
            out,
            connected: Arc::new(Mutex::new(HashSet::new())),
            filters: Arc::new(Mutex::new(filters)),
            resubscribe: Arc::new(resubscribe),
        };
        let seen = Arc::new(Mutex::new(Seen::default()));
        for url in relays {
            let task = RelayTask {
                url,
                out: pool.out.subscribe(),
                resubscribe: pool.resubscribe.subscribe(),
                filters: pool.filters.clone(),
                incoming: incoming.clone(),
                connected: pool.connected.clone(),
                seen: seen.clone(),
            };
            tokio::spawn(task.run());
        }
        pool
    }

    pub fn publish(&self, event: &Event) {
        let _ = self.out.send(format!(r#"["EVENT",{}]"#, event.as_json()));
    }

    /// Replace the subscription filters; every relay subscribes again.
    pub fn set_filters(&self, filters: Filters) {
        *self.filters.lock().unwrap() = filters;
        self.resubscribe.send_modify(|g| *g += 1);
    }

    /// Relays currently connected.
    pub fn connected(&self) -> usize {
        self.connected.lock().unwrap().len()
    }

    /// Close all connections.
    pub fn close(&self) {
        let _ = self.out.send(CLOSE.to_string());
    }
}

/// Internal message on the outgoing channel that makes the relay tasks quit.
const CLOSE: &str = "\0close";

struct RelayTask {
    url: String,
    out: broadcast::Receiver<String>,
    resubscribe: watch::Receiver<u64>,
    filters: Arc<Mutex<Filters>>,
    incoming: mpsc::UnboundedSender<Event>,
    connected: Arc<Mutex<HashSet<String>>>,
    seen: Arc<Mutex<Seen>>,
}

impl RelayTask {
    fn request(&self, sub: &str) -> Option<String> {
        let filters = (self.filters.lock().unwrap())();
        if filters.is_empty() {
            return None;
        }
        let mut req = vec![serde_json::json!("REQ"), serde_json::json!(sub)];
        req.extend(filters);
        Some(serde_json::Value::Array(req).to_string())
    }

    async fn run(mut self) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let mut backoff = 2;
        loop {
            if self.incoming.is_closed() {
                return;
            }
            let conn =
                tokio::time::timeout(Duration::from_secs(10), tokio_tungstenite::connect_async(self.url.as_str())).await;
            let Ok(Ok((mut ws, _))) = conn else {
                log::info!("relays: {} not reachable, retrying in {backoff}s", self.url);
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
                continue;
            };
            backoff = 2;
            let mut sub = format!("trax{:08x}", rand::random::<u32>());
            if let Some(req) = self.request(&sub) {
                if ws.send(Message::text(req)).await.is_err() {
                    continue;
                }
            }
            self.resubscribe.mark_unchanged();
            self.connected.lock().unwrap().insert(self.url.clone());
            loop {
                tokio::select! {
                    msg = self.out.recv() => match msg {
                        Ok(text) if text == CLOSE => {
                            self.connected.lock().unwrap().remove(&self.url);
                            let _ = ws.close(None).await;
                            return;
                        }
                        Ok(text) => {
                            if ws.send(Message::text(text)).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => {
                            self.connected.lock().unwrap().remove(&self.url);
                            let _ = ws.close(None).await;
                            return;
                        }
                    },
                    changed = self.resubscribe.changed() => {
                        if changed.is_err() {
                            continue;
                        }
                        let _ = ws.send(Message::text(serde_json::json!(["CLOSE", sub]).to_string())).await;
                        sub = format!("trax{:08x}", rand::random::<u32>());
                        if let Some(req) = self.request(&sub) {
                            if ws.send(Message::text(req)).await.is_err() {
                                break;
                            }
                        }
                    }
                    msg = ws.next() => match msg {
                        Some(Ok(Message::Text(text))) => self.handle(&text, &sub),
                        Some(Ok(Message::Ping(p))) => {
                            let _ = ws.send(Message::Pong(p)).await;
                        }
                        Some(Ok(_)) => {}
                        _ => break,
                    },
                }
                if self.incoming.is_closed() {
                    self.connected.lock().unwrap().remove(&self.url);
                    let _ = ws.close(None).await;
                    return;
                }
            }
            self.connected.lock().unwrap().remove(&self.url);
            tokio::time::sleep(Duration::from_secs(backoff)).await;
        }
    }

    fn handle(&self, text: &str, sub: &str) {
        let Ok(serde_json::Value::Array(parts)) = serde_json::from_str::<serde_json::Value>(text) else { return };
        match parts.first().and_then(|v| v.as_str()) {
            Some("EVENT") if parts.get(1).and_then(|v| v.as_str()) == Some(sub) => {}
            Some("OK") if parts.get(2).and_then(|v| v.as_bool()) == Some(false) => {
                log::info!("relays: {} refused an event: {}", self.url, parts.get(3).and_then(|v| v.as_str()).unwrap_or(""));
                return;
            }
            _ => return,
        }
        let Some(raw) = parts.get(2) else { return };
        let Ok(event) = Event::from_json(raw.to_string()) else { return };
        // The same event arrives once per relay.
        if !self.seen.lock().unwrap().insert(event.id.to_hex()) {
            return;
        }
        if event.verify().is_err() {
            return;
        }
        let _ = self.incoming.send(event);
    }
}
