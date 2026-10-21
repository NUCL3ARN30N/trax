//! Direct connections between host and guests: one WebRTC DataChannel per guest, set up
//! through [`signaling`](super::signaling).
//!
//! Runs on its own thread with a tokio runtime. The App talks to it through [`Party`]
//! (commands) and gets [`Event`]s back through a callback. Messages are opaque strings here;
//! their meaning lives in `protocol`.

use super::identity::{self, Identity};
use super::protocol::{MAX_GUESTS, MAX_MESSAGE_BYTES, PeerId};
use super::signaling::{Envelope, Incoming, Room, Signaling};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState,
    RTCIceServer, RTCPeerConnectionState, RTCSessionDescription,
};

/// Public STUN servers; TURN comes from settings. Three, from three operators: a DNS
/// blocker that swallows one of the names (they are on tracker lists) leaves the others.
const STUN: &[&str] =
    &["stun:stun.cloudflare.com:3478", "stun:stun.l.google.com:19302", "stun:stun.nextcloud.com:443"];
/// No DataChannel within this time: the network blocks a direct connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// A guest sends `join` this often until the host answers, and gives up after this long.
const JOIN_INTERVAL: Duration = Duration::from_secs(3);
const JOIN_TIMEOUT: Duration = Duration::from_secs(20);
/// Reconnect delays after losing the host; after the last one the party counts as ended.
const BACKOFF: &[u64] = &[1, 2, 4, 8, 15, 15, 15, 15, 15, 15, 15];

/// Optional TURN relay for networks that block direct connections.
#[derive(Clone, Default, Debug)]
pub struct Turn {
    pub url: String,
    pub username: String,
    pub credential: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub code: String,
    pub password: String,
    pub relays: Vec<String>,
    pub turn: Turn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No relay could be reached (offline, or all blocked).
    RelaysUnreachable,
    WrongPassword,
    /// Nobody answered: wrong code or the party isn't running.
    NotFound,
    /// Found the other side but no direct connection (strict NAT without TURN).
    CantConnect,
    /// The connection never got as far as trying: bad offer/answer, or TURN details that
    /// WebRTC refuses. Nothing to do with NAT.
    SetupFailed,
    /// The host stayed away after all reconnect attempts.
    HostGone,
}

impl Failure {
    pub fn message(self) -> &'static str {
        match self {
            Failure::RelaysUnreachable => "Can't reach the party network. Check your internet connection.",
            Failure::WrongPassword => "Wrong password",
            Failure::NotFound => "No party with this code found",
            Failure::CantConnect => {
                "Can't connect: your network or the host's blocks direct connections (strict NAT). In Settings → Party, run “Check my network” — if your side is fine, the host's network is the strict one and they need a TURN server. With a TURN server filled in, run “Test TURN server” too: one that refuses or can't be reached is the same as none at all."
            }
            Failure::SetupFailed => {
                "Couldn't set up the connection. If you filled in a TURN server in Settings → Party, check its address, user and password — a wrong one stops the party before it starts."
            }
            Failure::HostGone => "Host disconnected — the party has ended",
        }
    }
}

#[derive(Debug)]
pub enum Event {
    /// Number of relays connected (shown as network status).
    Relays(usize),
    /// Host: a guest's DataChannel is open; its identity was checked against DTLS.
    PeerOpen(PeerId),
    PeerMessage(PeerId, String),
    PeerClosed(PeerId),
    /// Guest: connected to the host (its identity was checked against DTLS).
    HostOpen,
    HostMessage(String),
    /// Guest: lost the host; reconnecting.
    HostLost,
    Failed(Failure),
}

enum Command {
    SendTo(PeerId, String),
    Broadcast(String),
    Disconnect(PeerId),
    /// Guest: drop the connection and join again (missed heartbeats).
    Reconnect,
    Shutdown,
}

/// A running party connection (host or guest side).
pub struct Party {
    cmd: mpsc::UnboundedSender<Command>,
    pub peer_id: PeerId,
}

impl Party {
    pub fn host(config: Config, events: impl Fn(Event) + Send + Sync + 'static) -> anyhow::Result<Party> {
        Self::start(config, events, true)
    }

    pub fn guest(config: Config, events: impl Fn(Event) + Send + Sync + 'static) -> anyhow::Result<Party> {
        Self::start(config, events, false)
    }

    fn start(config: Config, events: impl Fn(Event) + Send + Sync + 'static, host: bool) -> anyhow::Result<Party> {
        Self::start_as(identity::load()?, config, events, host)
    }

    fn start_as(
        identity: Identity,
        config: Config,
        events: impl Fn(Event) + Send + Sync + 'static,
        host: bool,
    ) -> anyhow::Result<Party> {
        let peer_id = identity.peer_id.clone();
        let (cmd, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new().name("party".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("party: no runtime: {e}");
                    return;
                }
            };
            let events: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(events);
            if host {
                rt.block_on(run_host(config, identity, rx, events));
            } else {
                rt.block_on(run_guest(config, identity, rx, events));
            }
            rt.shutdown_timeout(Duration::from_secs(2));
        })?;
        Ok(Party { cmd, peer_id })
    }

    /// Host: send to one guest.
    pub fn send_to(&self, peer: &str, text: String) {
        let _ = self.cmd.send(Command::SendTo(peer.to_string(), text));
    }

    /// Host: send to every connected guest. Guest: send to the host.
    pub fn broadcast(&self, text: String) {
        let _ = self.cmd.send(Command::Broadcast(text));
    }

    pub fn disconnect(&self, peer: &str) {
        let _ = self.cmd.send(Command::Disconnect(peer.to_string()));
    }

    pub fn reconnect(&self) {
        let _ = self.cmd.send(Command::Reconnect);
    }

    pub fn shutdown(&self) {
        let _ = self.cmd.send(Command::Shutdown);
    }
}

impl Drop for Party {
    fn drop(&mut self) {
        self.shutdown();
    }
}

type Events = Arc<dyn Fn(Event) + Send + Sync>;

// ------------------------------------------------------------------ one peer connection

/// What a PeerConnection reports to its owner.
enum PcEvent {
    Gathered,
    State(RTCPeerConnectionState),
    DataChannel(Arc<dyn DataChannel>),
}

struct Handler(mpsc::UnboundedSender<PcEvent>);

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.0.send(PcEvent::Gathered);
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.0.send(PcEvent::State(state));
    }
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let _ = self.0.send(PcEvent::DataChannel(dc));
    }
}

async fn new_connection(
    identity: &Identity,
    turn: &Turn,
) -> anyhow::Result<(Arc<dyn PeerConnection>, mpsc::UnboundedReceiver<PcEvent>)> {
    let mut servers = vec![RTCIceServer { urls: STUN.iter().map(|s| s.to_string()).collect(), ..Default::default() }];
    if !turn.url.trim().is_empty() {
        servers.push(RTCIceServer {
            urls: vec![turn.url.trim().to_string()],
            username: turn.username.clone(),
            credential: turn.credential.clone(),
        });
    }
    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(servers)
        .with_certificates(vec![identity.certificate.clone()])
        .build();
    let (tx, rx) = mpsc::unbounded_channel();
    let runtime = webrtc::runtime::default_runtime().ok_or_else(|| anyhow::anyhow!("no WebRTC runtime"))?;
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_handler(Arc::new(Handler(tx)))
        .with_runtime(runtime)
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((Arc::new(pc), rx))
}

/// Wait until ICE gathering finished (or a few seconds passed) and return our description
/// with all candidates in it, so one signaling message is enough.
async fn gathered_description(
    pc: &Arc<dyn PeerConnection>,
    events: &mut mpsc::UnboundedReceiver<PcEvent>,
    early_channel: &mut Option<Arc<dyn DataChannel>>,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(PcEvent::Gathered)) | Err(_) => break,
            Ok(Some(PcEvent::DataChannel(dc))) => *early_channel = Some(dc),
            Ok(Some(PcEvent::State(_))) => {}
            Ok(None) => return None,
        }
    }
    let desc = pc.local_description().await?;
    serde_json::to_string(&desc).ok()
}

/// The identity of whoever is on the other end of the DTLS connection.
async fn remote_peer_id(pc: &Arc<dyn PeerConnection>) -> Option<PeerId> {
    let sctp = pc.sctp().await?;
    let certs = sctp.transport().get_remote_certificates().await.ok()?;
    certs.first().map(|der| identity::peer_id_for(der))
}

fn text_of(data: &[u8]) -> Option<String> {
    (data.len() <= MAX_MESSAGE_BYTES).then(|| String::from_utf8(data.to_vec()).ok()).flatten()
}

// ------------------------------------------------------------------ host

/// Messages from per-guest tasks to the host loop.
enum HostInternal {
    Open(PeerId, Arc<dyn DataChannel>),
    Message(PeerId, String),
    Closed(PeerId, u64),
}

struct Guest {
    pc: Arc<dyn PeerConnection>,
    channel: Option<Arc<dyn DataChannel>>,
    /// Tells apart a guest's old and new connection after a reconnect.
    generation: u64,
    answer: Option<mpsc::UnboundedSender<String>>,
}

async fn run_host(config: Config, identity: Identity, mut commands: mpsc::UnboundedReceiver<Command>, events: Events) {
    let identity = Arc::new(identity);
    let (sig_tx, mut sig_rx) = mpsc::unbounded_channel();
    let signaling = Signaling::start(Room::new(&config.code, &config.password), config.relays.clone(), sig_tx);
    let (int_tx, mut int_rx) = mpsc::unbounded_channel::<HostInternal>();
    let mut guests: HashMap<PeerId, Guest> = HashMap::new();
    let mut generation = 0u64;
    let mut relay_tick = tokio::time::interval(Duration::from_secs(2));
    let mut relays_seen = usize::MAX;
    let mut answered_wrong_pw = std::collections::HashSet::new();
    loop {
        tokio::select! {
            msg = sig_rx.recv() => match msg {
                Some(Incoming::Message(env)) => match env.t.as_str() {
                    "join" if !env.from.is_empty() && env.from != identity.peer_id => {
                        let reconnect = guests.contains_key(&env.from);
                        if !reconnect && guests.len() >= MAX_GUESTS {
                            continue;
                        }
                        if let Some(old) = guests.remove(&env.from) {
                            let _ = old.pc.close().await;
                        }
                        generation += 1;
                        let (answer_tx, answer_rx) = mpsc::unbounded_channel();
                        match new_connection(&identity, &config.turn).await {
                            Ok((pc, pc_events)) => {
                                guests.insert(env.from.clone(), Guest { pc: pc.clone(), channel: None, generation, answer: Some(answer_tx) });
                                tokio::spawn(host_connect(
                                    pc, pc_events, env.from.clone(), identity.peer_id.clone(), signaling.clone(), answer_rx, int_tx.clone(), generation,
                                ));
                            }
                            Err(e) => log::warn!("party: can't create connection: {e:#}"),
                        }
                    }
                    "answer" if env.to == identity.peer_id => {
                        if let Some(tx) = guests.get(&env.from).and_then(|g| g.answer.as_ref()) {
                            let _ = tx.send(env.sdp);
                        }
                    }
                    _ => {}
                },
                Some(Incoming::WrongPasswordJoin { nonce }) => {
                    if answered_wrong_pw.insert(nonce.clone()) {
                        signaling.send(&Envelope::new("auth_failed", "", &nonce, ""));
                    }
                }
                Some(Incoming::AuthFailed { .. }) => {}
                None => break,
            },
            msg = int_rx.recv() => match msg {
                Some(HostInternal::Open(peer, dc)) => {
                    if let Some(g) = guests.get_mut(&peer) {
                        g.channel = Some(dc);
                        g.answer = None;
                        events(Event::PeerOpen(peer));
                    }
                }
                Some(HostInternal::Message(peer, text)) => events(Event::PeerMessage(peer, text)),
                Some(HostInternal::Closed(peer, conn_gen)) => {
                    // An old connection of a guest that already reconnected doesn't count.
                    if guests.get(&peer).map(|g| g.generation) == Some(conn_gen) {
                        if let Some(g) = guests.remove(&peer) {
                            let _ = g.pc.close().await;
                            if g.channel.is_some() {
                                events(Event::PeerClosed(peer));
                            }
                        }
                    }
                }
                None => break,
            },
            cmd = commands.recv() => match cmd {
                Some(Command::SendTo(peer, text)) => {
                    if let Some(dc) = guests.get(&peer).and_then(|g| g.channel.clone()) {
                        let _ = dc.send_text(&text).await;
                    }
                }
                Some(Command::Broadcast(text)) => {
                    for dc in guests.values().filter_map(|g| g.channel.clone()) {
                        let _ = dc.send_text(&text).await;
                    }
                }
                Some(Command::Disconnect(peer)) => {
                    if let Some(g) = guests.remove(&peer) {
                        // Let the last message (e.g. KICKED) go out first.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        let _ = g.pc.close().await;
                    }
                }
                Some(Command::Reconnect) => {}
                Some(Command::Shutdown) | None => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    for g in guests.values() {
                        let _ = g.pc.close().await;
                    }
                    break;
                }
            },
            _ = relay_tick.tick() => {
                let n = signaling.connected();
                if n != relays_seen {
                    relays_seen = n;
                    events(Event::Relays(n));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn host_connect(
    pc: Arc<dyn PeerConnection>,
    mut pc_events: mpsc::UnboundedReceiver<PcEvent>,
    guest: PeerId,
    me: PeerId,
    signaling: Signaling,
    mut answers: mpsc::UnboundedReceiver<String>,
    internal: mpsc::UnboundedSender<HostInternal>,
    generation: u64,
) {
    let closed = |internal: &mpsc::UnboundedSender<HostInternal>| {
        let _ = internal.send(HostInternal::Closed(guest.clone(), generation));
    };
    let Ok(dc) = pc.create_data_channel("control", None).await else { return closed(&internal) };
    let Ok(offer) = pc.create_offer(None).await else { return closed(&internal) };
    if pc.set_local_description(offer).await.is_err() {
        return closed(&internal);
    }
    let mut unused = None;
    let Some(sdp) = gathered_description(&pc, &mut pc_events, &mut unused).await else { return closed(&internal) };
    signaling.send(&Envelope::new("offer", &me, &guest, &sdp));

    // Wait for the answer, then for the channel to open.
    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    let answer = tokio::time::timeout_at(deadline, answers.recv()).await;
    let Some(desc) = answer.ok().flatten().and_then(|s| serde_json::from_str::<RTCSessionDescription>(&s).ok()) else {
        return closed(&internal);
    };
    if pc.set_remote_description(desc).await.is_err() {
        return closed(&internal);
    }
    let mut open = false;
    loop {
        tokio::select! {
            ev = dc.poll() => match ev {
                Some(DataChannelEvent::OnOpen) => {
                    // The guest must be who it said it was in the join.
                    if remote_peer_id(&pc).await.as_deref() != Some(guest.as_str()) {
                        log::warn!("party: identity mismatch for {guest}, closing");
                        break;
                    }
                    open = true;
                    let _ = internal.send(HostInternal::Open(guest.clone(), dc.clone()));
                }
                Some(DataChannelEvent::OnMessage(msg)) if open => {
                    if let Some(text) = text_of(&msg.data) {
                        let _ = internal.send(HostInternal::Message(guest.clone(), text));
                    }
                }
                Some(DataChannelEvent::OnClose) | None => break,
                Some(_) => {}
            },
            ev = pc_events.recv() => match ev {
                Some(PcEvent::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed)) | None => break,
                _ => {}
            },
            _ = tokio::time::sleep_until(deadline), if !open => break,
        }
    }
    closed(&internal);
}

// ------------------------------------------------------------------ guest

enum Attempt {
    /// The connection was lost after it worked.
    Lost,
    Failed(Failure),
    Shutdown,
}

async fn run_guest(config: Config, identity: Identity, mut commands: mpsc::UnboundedReceiver<Command>, events: Events) {
    let (sig_tx, mut sig_rx) = mpsc::unbounded_channel();
    let signaling = Signaling::start(Room::new(&config.code, &config.password), config.relays.clone(), sig_tx);
    let mut ever_connected = false;
    let mut retries = 0usize;
    loop {
        let result = guest_attempt(&config, &identity, &signaling, &mut sig_rx, &mut commands, &events).await;
        match result {
            Attempt::Shutdown => return,
            Attempt::Lost => {
                ever_connected = true;
                retries = 0;
                events(Event::HostLost);
            }
            Attempt::Failed(f) if !ever_connected => {
                events(Event::Failed(f));
                return;
            }
            Attempt::Failed(_) => {
                let Some(delay) = BACKOFF.get(retries) else {
                    events(Event::Failed(Failure::HostGone));
                    return;
                };
                retries += 1;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(*delay)) => {}
                    cmd = commands.recv() => if matches!(cmd, Some(Command::Shutdown) | None) { return },
                }
            }
        }
    }
}

/// One connection attempt; returns when it failed or, after working, was lost.
async fn guest_attempt(
    config: &Config,
    identity: &Identity,
    signaling: &Signaling,
    sig_rx: &mut mpsc::UnboundedReceiver<Incoming>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &Events,
) -> Attempt {
    let me = identity.peer_id.clone();
    let mut join = Envelope::new("join", &me, "", "");
    let mut my_nonces = vec![join.nonce.clone()];
    signaling.send(&join);
    let started = tokio::time::Instant::now();
    let mut join_tick = tokio::time::interval_at(started + JOIN_INTERVAL, JOIN_INTERVAL);
    let mut wrong_password = false;
    // Wait for an offer.
    let (host_id, offer) = loop {
        tokio::select! {
            msg = sig_rx.recv() => match msg {
                Some(Incoming::Message(env)) if env.t == "offer" && env.to == me => break (env.from, env.sdp),
                Some(Incoming::AuthFailed { to }) if my_nonces.contains(&to) => wrong_password = true,
                Some(_) => {}
                None => return Attempt::Shutdown,
            },
            _ = join_tick.tick() => {
                if started.elapsed() > JOIN_TIMEOUT {
                    return Attempt::Failed(if wrong_password {
                        Failure::WrongPassword
                    } else if signaling.connected() == 0 {
                        Failure::RelaysUnreachable
                    } else {
                        Failure::NotFound
                    });
                }
                if wrong_password {
                    return Attempt::Failed(Failure::WrongPassword);
                }
                join = Envelope::new("join", &me, "", "");
                my_nonces.push(join.nonce.clone());
                signaling.send(&join);
            }
            cmd = commands.recv() => if matches!(cmd, Some(Command::Shutdown) | None) { return Attempt::Shutdown },
        }
    };
    // Everything up to here is negotiation: a failure means bad data or bad settings, not a
    // network that can't be crossed.
    let Some(offer) = serde_json::from_str::<RTCSessionDescription>(&offer).ok() else {
        log::warn!("party: the host's offer could not be read");
        return Attempt::Failed(Failure::SetupFailed);
    };
    let (pc, mut pc_events) = match new_connection(identity, &config.turn).await {
        Ok(c) => c,
        Err(e) => {
            log::warn!("party: can't create connection (check the TURN settings): {e:#}");
            return Attempt::Failed(Failure::SetupFailed);
        }
    };
    let fail = |pc: Arc<dyn PeerConnection>, f: Failure| async move {
        let _ = pc.close().await;
        Attempt::Failed(f)
    };
    if pc.set_remote_description(offer).await.is_err() {
        return fail(pc, Failure::SetupFailed).await;
    }
    let Ok(answer) = pc.create_answer(None).await else { return fail(pc, Failure::SetupFailed).await };
    if pc.set_local_description(answer).await.is_err() {
        return fail(pc, Failure::SetupFailed).await;
    }
    let mut channel = None;
    let Some(sdp) = gathered_description(&pc, &mut pc_events, &mut channel).await else {
        return fail(pc, Failure::SetupFailed).await;
    };
    signaling.send(&Envelope::new("answer", &me, &host_id, &sdp));

    let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
    // The host opens the channel; wait for it.
    let dc = loop {
        if let Some(dc) = channel.take() {
            break dc;
        }
        tokio::select! {
            ev = pc_events.recv() => match ev {
                Some(PcEvent::DataChannel(dc)) => channel = Some(dc),
                Some(PcEvent::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed)) | None => {
                    return fail(pc, Failure::CantConnect).await;
                }
                _ => {}
            },
            _ = tokio::time::sleep_until(deadline) => return fail(pc, Failure::CantConnect).await,
            cmd = commands.recv() => if matches!(cmd, Some(Command::Shutdown) | None) {
                let _ = pc.close().await;
                return Attempt::Shutdown;
            },
        }
    };
    let mut open = false;
    let result = loop {
        tokio::select! {
            ev = dc.poll() => match ev {
                Some(DataChannelEvent::OnOpen) => {
                    // The host must be who signed the offer.
                    if remote_peer_id(&pc).await.as_deref() != Some(host_id.as_str()) {
                        log::warn!("party: host identity mismatch");
                        break if open { Attempt::Lost } else { Attempt::Failed(Failure::CantConnect) };
                    }
                    open = true;
                    events(Event::HostOpen);
                }
                Some(DataChannelEvent::OnMessage(msg)) if open => {
                    if let Some(text) = text_of(&msg.data) {
                        events(Event::HostMessage(text));
                    }
                }
                Some(DataChannelEvent::OnClose) | None => break if open { Attempt::Lost } else { Attempt::Failed(Failure::CantConnect) },
                Some(_) => {}
            },
            ev = pc_events.recv() => match ev {
                Some(PcEvent::State(RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed)) | None => {
                    break if open { Attempt::Lost } else { Attempt::Failed(Failure::CantConnect) };
                }
                _ => {}
            },
            _ = tokio::time::sleep_until(deadline), if !open => break Attempt::Failed(Failure::CantConnect),
            cmd = commands.recv() => match cmd {
                Some(Command::Broadcast(text)) | Some(Command::SendTo(_, text)) => {
                    let _ = dc.send_text(&text).await;
                }
                Some(Command::Reconnect) => break Attempt::Lost,
                Some(Command::Disconnect(_)) => {}
                Some(Command::Shutdown) | None => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let _ = pc.close().await;
                    return Attempt::Shutdown;
                }
            },
        }
    };
    let _ = pc.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn config(code: &str, password: &str) -> Config {
        Config {
            code: code.into(),
            password: password.into(),
            relays: super::super::signaling::DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            turn: Turn::default(),
        }
    }

    fn wait_for(log: &Mutex<Vec<String>>, needle: &str, secs: u64) -> bool {
        let end = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < end {
            if log.lock().unwrap().iter().any(|l| l.contains(needle)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Host and guest in one process, through the real relays and a real WebRTC connection:
    /// `cargo test host_and_guest_connect -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn host_and_guest_connect() {
        let code = super::super::code::generate();
        let host_log = Arc::new(Mutex::new(Vec::new()));
        let guest_log = Arc::new(Mutex::new(Vec::new()));
        let hl = host_log.clone();
        let host = Party::start_as(identity::ephemeral(), config(&code, "pw"), move |e| hl.lock().unwrap().push(format!("{e:?}")), true).unwrap();
        std::thread::sleep(Duration::from_secs(3));

        // Wrong password: refused quickly with a clear reason.
        let wl = Arc::new(Mutex::new(Vec::new()));
        let w = wl.clone();
        let _wrong = Party::start_as(identity::ephemeral(), config(&code, "nope"), move |e| w.lock().unwrap().push(format!("{e:?}")), false).unwrap();
        assert!(wait_for(&wl, "WrongPassword", 25), "wrong password not reported: {:?}", wl.lock().unwrap());

        let gl = guest_log.clone();
        let guest_identity = identity::ephemeral();
        let guest_id = guest_identity.peer_id.clone();
        let guest = Party::start_as(guest_identity, config(&code, "pw"), move |e| gl.lock().unwrap().push(format!("{e:?}")), false).unwrap();
        assert!(wait_for(&guest_log, "HostOpen", 30), "guest log: {:?}", guest_log.lock().unwrap());
        assert!(wait_for(&host_log, &format!("PeerOpen(\"{guest_id}\")"), 5), "host log: {:?}", host_log.lock().unwrap());

        guest.broadcast("hello host".into());
        assert!(wait_for(&host_log, "hello host", 5));
        host.send_to(&guest_id, "hello guest".into());
        assert!(wait_for(&guest_log, "hello guest", 5));
        host.broadcast("to everyone".into());
        assert!(wait_for(&guest_log, "to everyone", 5));

        // The host leaving is noticed.
        host.shutdown();
        assert!(wait_for(&guest_log, "HostLost", 15), "guest log: {:?}", guest_log.lock().unwrap());
        println!("host: {:?}\nguest: {:?}", host_log.lock().unwrap(), guest_log.lock().unwrap());
    }

    /// Nobody hosts this code.
    #[test]
    #[ignore]
    fn unknown_code_is_reported() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        let _g = Party::start_as(identity::ephemeral(), config(&super::super::code::generate(), ""), move |e| l.lock().unwrap().push(format!("{e:?}")), false).unwrap();
        assert!(wait_for(&log, "NotFound", 30), "log: {:?}", log.lock().unwrap());
    }
}
