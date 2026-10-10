use super::crypto::{self, Cipher, Envelope};
use super::store::{Credential, Store};
use super::{AdminReply, AdminRequest, Device, Pairing, Pending, Status};
use crate::daemon::pane::{DaemonPane, MobileAgentState, OutputGate};
use crate::daemon::protocol::{DaemonMsg, WinSize};
use futures_util::{SinkExt, StreamExt};
use parking_lot::{Condvar, Mutex};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc as async_mpsc;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

const MAX_PEERS: usize = 32;
const MAX_SUBSCRIPTIONS: usize = 8;
const MOBILE_PROTOCOL: u64 = 2;
const OUTPUT_WINDOW: u64 = 4;
const MAX_OUTPUT_BYTES: usize = 48 * 1024;
const PAIR_LIFETIME: u64 = 300;
const HANDSHAKE_LIFETIME: Duration = Duration::from_secs(10);
const MAX_CONNECTION_LIFETIME: Duration = Duration::from_secs(24 * 3600);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) trait Backend: Send + Sync {
    fn request(&self, request: &Value) -> io::Result<Value>;
    fn observe(&self, workspace: &str, pane_id: u64) -> io::Result<Observation>;
}
pub(crate) struct Observation {
    pub pane: Arc<DaemonPane>,
    pub observer_id: u64,
    pub receiver: mpsc::Receiver<DaemonMsg>,
    pub gate: Arc<OutputGate>,
    pub size: WinSize,
    pub initial: Vec<DaemonMsg>,
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.pane.unobserve(self.observer_id);
    }
}
struct Command {
    request: AdminRequest,
    reply: mpsc::SyncSender<io::Result<AdminReply>>,
}
static ADMIN: OnceLock<async_mpsc::Sender<Command>> = OnceLock::new();

pub(crate) fn dispatch(request: AdminRequest) -> io::Result<AdminReply> {
    let sender = ADMIN
        .get()
        .ok_or_else(|| io::Error::other("mobile service is unavailable"))?;
    let (reply, receive) = mpsc::sync_channel(1);
    sender
        .try_send(Command { request, reply })
        .map_err(|_| io::Error::other("mobile admin queue is full or closed"))?;
    receive
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "mobile admin timed out"))?
}
pub(crate) fn start(backend: Arc<dyn Backend>) -> io::Result<()> {
    if ADMIN.get().is_some() {
        return Ok(());
    }
    let store = Store::load();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let (send, receive) = async_mpsc::channel(32);
    std::thread::Builder::new()
        .name("tty7-mobile".into())
        .spawn(move || {
            runtime.block_on(Service::new(store, backend).run(receive));
        })?;
    ADMIN
        .set(send)
        .map_err(|_| io::Error::other("mobile service already started"))
}

struct Bootstrap {
    id: String,
    secret: [u8; 32],
    pairing: Pairing,
    attempts: usize,
}
enum Phase {
    Hello,
    AwaitAuth { credential: String },
    AwaitPair,
    Pending { request_id: String, name: String },
    Authenticated { credential: String },
}
struct Peer {
    phase: Phase,
    cipher: Option<Cipher>,
    code: String,
    deadline: Instant,
    subscriptions: HashMap<u64, Subscription>,
    alive: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    jobs: Arc<AtomicUsize>,
    request_queue: Option<mpsc::SyncSender<(Value, u64, OperationGuard)>>,
}
impl Peer {
    fn new(phase: Phase) -> Self {
        Self {
            phase,
            cipher: None,
            code: String::new(),
            deadline: Instant::now() + HANDSHAKE_LIFETIME,
            subscriptions: HashMap::new(),
            alive: Arc::new(AtomicBool::new(true)),
            failed: Arc::new(AtomicBool::new(false)),
            jobs: Arc::new(AtomicUsize::new(0)),
            request_queue: None,
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}
struct OperationGuard {
    jobs: Arc<AtomicUsize>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.jobs.fetch_sub(1, Ordering::Release);
    }
}
struct RelayConfig {
    relay_url: String,
    tls_fingerprint: Option<String>,
    host_id: String,
    token: String,
}
struct Subscription {
    id: String,
    workspace: String,
    observer: Option<(Arc<DaemonPane>, u64)>,
    lease_started: bool,
    credit: Arc<OutputCredit>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.credit.cancel();
    }
}

struct CreditState {
    active: bool,
    reserved: u64,
    sent: u64,
    acknowledged: u64,
}
struct OutputCredit {
    state: Mutex<CreditState>,
    changed: Condvar,
    cancelled: tokio::sync::Notify,
}
impl OutputCredit {
    fn new() -> Self {
        Self {
            state: Mutex::new(CreditState {
                active: true,
                reserved: 0,
                sent: 0,
                acknowledged: 0,
            }),
            changed: Condvar::new(),
            cancelled: tokio::sync::Notify::new(),
        }
    }
    fn active(&self) -> bool {
        self.state.lock().active
    }
    fn cancel(&self) {
        self.state.lock().active = false;
        self.changed.notify_all();
        self.cancelled.notify_waiters();
    }
    fn reserve(&self) -> io::Result<u64> {
        let mut state = self.state.lock();
        while state.active && state.reserved - state.acknowledged >= OUTPUT_WINDOW {
            self.changed.wait(&mut state);
        }
        if !state.active {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "subscription cancelled",
            ));
        }
        state.reserved = state
            .reserved
            .checked_add(1)
            .ok_or_else(|| io::Error::other("output sequence exhausted"))?;
        Ok(state.reserved)
    }
    // Queued frames consume credit too, but cannot be acknowledged before the
    // actor exposes them to the socket. No service/registry lock is held waiting.
    fn sent(&self, sequence: u64) -> io::Result<()> {
        let mut state = self.state.lock();
        if !state.active || sequence > state.reserved || state.sent.checked_add(1) != Some(sequence)
        {
            return Err(io::Error::other("invalid queued output sequence"));
        }
        state.sent = sequence;
        Ok(())
    }
    fn acknowledge(&self, sequence: u64) -> io::Result<()> {
        let mut state = self.state.lock();
        if sequence == 0 || sequence > state.sent {
            return Err(crypto::denied());
        }
        if sequence > state.acknowledged {
            state.acknowledged = sequence;
            self.changed.notify_one();
        }
        Ok(())
    }
}
struct Output {
    client: String,
    pane: u64,
    subscription: String,
    value: Value,
    sequence: Option<u64>,
    finished: bool,
}
struct Service {
    store: Option<Store>,
    backend: Arc<dyn Backend>,
    error: Option<String>,
    connected: bool,
    bootstrap: Option<Bootstrap>,
    peers: HashMap<String, Peer>,
    output: async_mpsc::Sender<Output>,
    outputs: async_mpsc::Receiver<Output>,
    replies: async_mpsc::Sender<(String, Value)>,
    reply_receiver: async_mpsc::Receiver<(String, Value)>,
    operation_slots: Arc<tokio::sync::Semaphore>,
    rate_window: Instant,
    attempts: usize,
}
impl Service {
    fn new(store: io::Result<Store>, backend: Arc<dyn Backend>) -> Self {
        let (output, outputs) = async_mpsc::channel(128);
        let (replies, reply_receiver) = async_mpsc::channel(256);
        let (store, error) = match store {
            Ok(store) => (Some(store), None),
            Err(_) => (
                None,
                Some("private mobile credential store could not be opened; access disabled".into()),
            ),
        };
        Self {
            store,
            backend,
            error,
            connected: false,
            bootstrap: None,
            peers: HashMap::new(),
            output,
            outputs,
            replies,
            reply_receiver,
            operation_slots: Arc::new(tokio::sync::Semaphore::new(128)),
            rate_window: Instant::now(),
            attempts: 0,
        }
    }
    fn enabled(&self) -> bool {
        self.store.as_ref().is_some_and(|store| store.enabled)
    }
    fn status(&self) -> Status {
        Status {
            enabled: self.enabled(),
            relay_url: self.store.as_ref().map(|s| s.relay_url.clone()).unwrap_or_else(|| super::DEFAULT_RELAY_URL.into()),
            tls_fingerprint: self.store.as_ref().and_then(|s| s.tls_fingerprint.clone()),
            connected: self.connected,
            error: self.error.clone(),
            devices: self.store.as_ref().map(|s| s.credentials.iter().map(|credential| Device {
                credential_id: credential.id.clone(), name: credential.name.clone(),
                connected: self.peers.values().any(|peer| matches!(&peer.phase, Phase::Authenticated { credential: id } if id == &credential.id)),
            }).collect()).unwrap_or_default(),
            pending: self.peers.values().filter_map(|peer| match &peer.phase {
                Phase::Pending { request_id, name } => Some(Pending { request_id: request_id.clone(), device_name: name.clone(), code: peer.code.clone() }),
                _ => None,
            }).collect(),
            pairing: self.bootstrap.as_ref().map(|b| b.pairing.clone()),
        }
    }
    async fn run(mut self, mut commands: async_mpsc::Receiver<Command>) {
        let mut socket: Option<Socket> = None;
        let mut retry_at = Instant::now();
        let mut backoff = 1u64;
        let mut last_received = Instant::now();
        let mut ping_at = Instant::now();
        let mut ticks = tokio::time::interval(Duration::from_millis(250));
        loop {
            enum Event {
                Admin(Option<Command>),
                Frame(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
                Output(Option<Output>),
                Reply(Option<(String, Value)>),
                Tick,
            }
            let event = tokio::select! {
                biased;
                command = commands.recv() => Event::Admin(command),
                reply = self.reply_receiver.recv() => Event::Reply(reply),
                _ = ticks.tick() => Event::Tick,
                frame = async { match socket.as_mut() { Some(socket) => socket.next().await, None => std::future::pending().await } } => Event::Frame(frame),
                output = self.outputs.recv() => Event::Output(output),
            };
            let mut outgoing = Vec::new();
            let mut reconnect = false;
            match event {
                Event::Admin(Some(command)) => {
                    let result = self.admin(command.request, &mut outgoing, &mut reconnect);
                    // Complete network invalidation before acknowledging disable/revoke/approval.
                    let sent = send_all(&mut socket, outgoing).await;
                    outgoing = Vec::new();
                    if reconnect || sent.is_err() {
                        close(&mut socket).await;
                        self.disconnected();
                        retry_at = Instant::now();
                    }
                    let _ = command.reply.send(if sent.is_err() && result.is_ok() {
                        Err(io::Error::other(
                            "mobile relay disconnected during admin action",
                        ))
                    } else {
                        result
                    });
                }
                Event::Admin(None) => break,
                Event::Frame(Some(Ok(message))) => {
                    last_received = Instant::now();
                    match message {
                        Message::Text(text) => {
                            if text.len() > 1024 * 1024
                                || self.frame(&text, &mut outgoing).await.is_err()
                            {
                                reconnect = true;
                            }
                        }
                        Message::Ping(bytes) => outgoing.push(Message::Pong(bytes)),
                        Message::Pong(_) => {}
                        Message::Close(_) | Message::Binary(_) => reconnect = true,
                        _ => {}
                    }
                }
                Event::Frame(_) => reconnect = true,
                Event::Output(Some(output)) => self.output_frame(output, &mut outgoing),
                Event::Output(None) => break,
                Event::Reply(Some((client, response))) => {
                    if let Some(peer) = self.peers.get_mut(&client)
                        && matches!(&peer.phase, Phase::Authenticated { .. })
                    {
                        match encrypted(&client, peer, &response) {
                            Ok(frame) => outgoing.push(frame),
                            Err(_) => {
                                outgoing.push(disconnect(&client));
                                self.peers.remove(&client);
                            }
                        }
                    }
                }
                Event::Reply(None) => break,
                Event::Tick => {
                    if self.rate_window.elapsed() >= Duration::from_secs(60) {
                        self.attempts = 0;
                        self.rate_window = Instant::now();
                    }
                    if self
                        .bootstrap
                        .as_ref()
                        .is_some_and(|b| b.pairing.expires_at <= unix_now())
                    {
                        self.invalidate_bootstrap("pairing expired", &mut outgoing);
                    }
                    let expired: Vec<_> = self
                        .peers
                        .iter()
                        .filter(|(_, peer)| {
                            peer.deadline <= Instant::now() || peer.failed.load(Ordering::Acquire)
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    for client in expired {
                        self.peers.remove(&client);
                        outgoing.push(disconnect(&client));
                    }
                    if socket.is_some() {
                        if last_received.elapsed() > Duration::from_secs(60) {
                            reconnect = true;
                        }
                        if ping_at.elapsed() > Duration::from_secs(20) {
                            outgoing.push(Message::Ping(Vec::new().into()));
                            ping_at = Instant::now();
                        }
                    } else if self.enabled() && retry_at <= Instant::now() {
                        let store = self.store.as_ref().expect("enabled store");
                        let config = RelayConfig {
                            relay_url: store.relay_url.clone(),
                            tls_fingerprint: store.tls_fingerprint.clone(),
                            host_id: store.host_id().expect("validated host token"),
                            token: store.host_token.clone(),
                        };
                        // Admin status and disabling remain responsive during DNS/TLS attempts.
                        let connect =
                            tokio::time::timeout(Duration::from_secs(10), connect(&config));
                        tokio::pin!(connect);
                        loop {
                            tokio::select! {
                                result = &mut connect => {
                                    match result {
                                        Ok(Ok(connected)) => { socket = Some(connected); self.connected = true; self.error = None; last_received = Instant::now(); ping_at = Instant::now(); backoff = 1; },
                                        failure => {
                                            self.error = Some(match failure {
                                                Ok(Err(error)) => error.to_string(),
                                                _ => "relay connection timed out".into(),
                                            });
                                            retry_at = Instant::now() + Duration::from_secs(backoff);
                                            backoff = (backoff * 2).min(30);
                                        },
                                    }
                                    break;
                                }
                                command = commands.recv() => {
                                    let Some(command) = command else { return; };
                                    let mut restart = false;
                                    let result = self.admin(command.request, &mut Vec::new(), &mut restart);
                                    let _ = command.reply.send(result);
                                    if restart { retry_at = Instant::now(); break; }
                                }
                            }
                        }
                    }
                }
            }
            if send_all(&mut socket, outgoing).await.is_err() {
                reconnect = true;
            }
            if reconnect {
                close(&mut socket).await;
                self.disconnected();
                retry_at = Instant::now() + Duration::from_secs(backoff);
                backoff = (backoff * 2).min(30);
            }
        }
        close(&mut socket).await;
    }
    fn disconnected(&mut self) {
        self.connected = false;
        self.peers.clear();
    }
    fn output_frame(&mut self, output: Output, outgoing: &mut Vec<Message>) {
        let Some(peer) = self.peers.get_mut(&output.client) else {
            return;
        };
        let Some(subscription) = peer
            .subscriptions
            .get(&output.pane)
            .filter(|s| s.id == output.subscription)
        else {
            return;
        };
        if let Some(sequence) = output.sequence
            && subscription.credit.sent(sequence).is_err()
        {
            return;
        }
        match encrypted(&output.client, peer, &output.value) {
            Ok(frame) => {
                outgoing.push(frame);
                if output.finished {
                    peer.subscriptions.remove(&output.pane);
                }
            }
            Err(error) => {
                peer.subscriptions.remove(&output.pane);
                if let Ok(frame) = encrypted(
                    &output.client,
                    peer,
                    &json!({"type":"terminal_error",
                    "pane_id":output.pane, "subscription_id":output.subscription, "message":error.to_string()}),
                ) {
                    outgoing.push(frame);
                }
            }
        }
    }
    fn invalidate_bootstrap(&mut self, message: &str, outgoing: &mut Vec<Message>) {
        self.bootstrap = None;
        let ids: Vec<_> = self
            .peers
            .iter()
            .filter(|(_, peer)| matches!(&peer.phase, Phase::AwaitPair | Phase::Pending { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(mut peer) = self.peers.remove(&id)
                && let Ok(frame) = encrypted(
                    &id,
                    &mut peer,
                    &json!({"type":"pair_rejected", "message":message}),
                )
            {
                outgoing.push(frame);
            }
            outgoing.push(disconnect(&id));
        }
    }
    fn admin(
        &mut self,
        request: AdminRequest,
        outgoing: &mut Vec<Message>,
        reconnect: &mut bool,
    ) -> io::Result<AdminReply> {
        match request {
            AdminRequest::Status => Ok(AdminReply::Status(self.status())),
            AdminRequest::Configure {
                relay_url,
                tls_fingerprint,
                enabled,
            } => {
                let (relay_url, tls_fingerprint) =
                    validate_endpoint(&relay_url, tls_fingerprint.as_deref())?;
                let mut store = self
                    .store
                    .as_ref()
                    .ok_or_else(|| io::Error::other("private mobile credentials unavailable"))?
                    .clone();
                store.relay_url = relay_url;
                store.tls_fingerprint = tls_fingerprint;
                store.enabled = enabled;
                store.save()?;
                self.invalidate_bootstrap("configuration changed", outgoing);
                self.peers.clear();
                self.connected = false;
                self.error = None;
                self.store = Some(store);
                *reconnect = true;
                Ok(AdminReply::Status(self.status()))
            }
            AdminRequest::BeginPairing => {
                if !self.enabled() {
                    return Err(io::Error::other("enable mobile access before pairing"));
                }
                if !self.connected {
                    return Err(io::Error::other(
                        "wait for a trusted relay connection before pairing",
                    ));
                }
                let store = self.store.as_ref().expect("enabled store");
                let secret = crypto::random()?;
                let id = uuid::Uuid::new_v4().to_string();
                let expires_at = unix_now() + PAIR_LIFETIME;
                let data = json!({ "v":1, "relay_url":store.relay_url, "tls_fingerprint":store.tls_fingerprint,
                    "host_id":store.host_id()?, "host_name":host_name(), "pair_id":id,
                    "secret":crypto::encode(&secret), "expires_at":expires_at });
                let pairing = Pairing {
                    uri: format!(
                        "tty7://pair?data={}",
                        crypto::encode(&serde_json::to_vec(&data).map_err(io::Error::other)?)
                    ),
                    expires_at,
                };
                self.invalidate_bootstrap("pairing replaced", outgoing);
                self.bootstrap = Some(Bootstrap {
                    id,
                    secret,
                    pairing: pairing.clone(),
                    attempts: 0,
                });
                Ok(AdminReply::Pairing(pairing))
            }
            AdminRequest::CancelPairing => {
                self.invalidate_bootstrap("pairing cancelled", outgoing);
                Ok(AdminReply::Unit)
            }
            AdminRequest::Approve { request_id } => {
                if self
                    .bootstrap
                    .as_ref()
                    .is_none_or(|b| b.pairing.expires_at <= unix_now())
                {
                    return Err(io::Error::other("pairing expired"));
                }
                let client = self.peers.iter().find(|(_, peer)| matches!(&peer.phase, Phase::Pending { request_id: id, .. } if id == &request_id)).map(|(id, _)| id.clone())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pairing request is no longer connected"))?;
                let name = match &self.peers[&client].phase {
                    Phase::Pending { name, .. } => name.clone(),
                    _ => unreachable!(),
                };
                let credential = Credential {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    secret: crypto::encode(&crypto::random()?),
                };
                let mut store = self
                    .store
                    .as_ref()
                    .ok_or_else(|| io::Error::other("private mobile credentials unavailable"))?
                    .clone();
                if store.credentials.len() >= 64 {
                    return Err(io::Error::other("maximum paired devices reached"));
                }
                store.credentials.push(credential.clone());
                store.save()?;
                self.store = Some(store);
                let mut peer = self.peers.remove(&client).expect("pending peer");
                outgoing.push(encrypted(&client, &mut peer, &json!({"type":"paired", "credential_id":credential.id, "secret":credential.secret, "device_name":credential.name}))?);
                outgoing.push(disconnect(&client));
                self.invalidate_bootstrap("pairing consumed", outgoing);
                Ok(AdminReply::Unit)
            }
            AdminRequest::Reject { request_id } => {
                let client = self.peers.iter().find(|(_, peer)| matches!(&peer.phase, Phase::Pending { request_id: id, .. } if id == &request_id)).map(|(id, _)| id.clone())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pairing request is no longer connected"))?;
                let mut peer = self.peers.remove(&client).expect("pending peer");
                outgoing.push(encrypted(
                    &client,
                    &mut peer,
                    &json!({"type":"pair_rejected", "message":"desktop rejected pairing"}),
                )?);
                outgoing.push(disconnect(&client));
                Ok(AdminReply::Unit)
            }
            AdminRequest::Revoke { credential_id } => {
                let mut store = self
                    .store
                    .as_ref()
                    .ok_or_else(|| io::Error::other("private mobile credentials unavailable"))?
                    .clone();
                let before = store.credentials.len();
                store.credentials.retain(|c| c.id != credential_id);
                if store.credentials.len() == before {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "device is not paired",
                    ));
                }
                store.save()?;
                self.store = Some(store);
                let ids: Vec<_> = self.peers.iter().filter(|(_, peer)| matches!(&peer.phase,
                    Phase::Authenticated { credential } | Phase::AwaitAuth { credential } if credential == &credential_id)).map(|(id, _)| id.clone()).collect();
                for client in ids {
                    self.peers.remove(&client);
                    outgoing.push(disconnect(&client));
                }
                Ok(AdminReply::Unit)
            }
        }
    }
    async fn frame(&mut self, text: &str, outgoing: &mut Vec<Message>) -> io::Result<()> {
        let frame: Value = serde_json::from_str(text).map_err(|_| crypto::denied())?;
        match frame.get("type").and_then(Value::as_str) {
            Some("ready") => {
                self.connected = true;
            }
            Some("peer_joined") => {
                let client = field(&frame, "client_id")?.to_string();
                self.attempts += 1;
                if client.len() > 128
                    || self.peers.contains_key(&client)
                    || self.peers.len() >= MAX_PEERS
                    || self.attempts > 128
                {
                    outgoing.push(disconnect(&client));
                    return Ok(());
                }
                self.peers.insert(client, Peer::new(Phase::Hello));
            }
            Some("peer_left") => {
                self.peers.remove(field(&frame, "client_id")?);
            }
            Some("data") => {
                let client = field(&frame, "client_id")?.to_string();
                let Some(mut peer) = self.peers.remove(&client) else {
                    outgoing.push(disconnect(&client));
                    return Ok(());
                };
                let result = match field(&frame, "payload") {
                    Ok(payload) => self.peer_frame(&client, &mut peer, payload, outgoing).await,
                    Err(error) => Err(error),
                };
                if result.is_ok() {
                    self.peers.insert(client, peer);
                } else {
                    outgoing.push(disconnect(&client));
                }
            }
            Some("error") => return Err(io::Error::other("relay registration refused")),
            _ => return Err(crypto::denied()),
        }
        Ok(())
    }
    async fn peer_frame(
        &mut self,
        client: &str,
        peer: &mut Peer,
        payload: &str,
        outgoing: &mut Vec<Message>,
    ) -> io::Result<()> {
        if payload.len() > 384 * 1024 || peer.deadline <= Instant::now() {
            return Err(crypto::denied());
        }
        if matches!(&peer.phase, Phase::Hello) {
            let hello: Value = serde_json::from_str(payload).map_err(|_| crypto::denied())?;
            if field(&hello, "kind")? != "hello"
                || hello.get("protocol").and_then(Value::as_u64) != Some(MOBILE_PROTOCOL)
            {
                return Err(crypto::denied());
            }
            let id = field(&hello, "credential_id")?;
            if id.len() > 128 {
                return Err(crypto::denied());
            }
            let client_nonce = crypto::secret(field(&hello, "nonce")?)?;
            let psk = if let Some(pair_id) = id.strip_prefix("pair:") {
                let bootstrap = self.bootstrap.as_mut().ok_or_else(crypto::denied)?;
                if bootstrap.id != pair_id
                    || bootstrap.pairing.expires_at <= unix_now()
                    || bootstrap.attempts >= 16
                {
                    return Err(crypto::denied());
                }
                bootstrap.attempts += 1;
                peer.phase = Phase::AwaitPair;
                bootstrap.secret
            } else {
                let store = self.store.as_ref().ok_or_else(crypto::denied)?;
                let credential = store
                    .credentials
                    .iter()
                    .find(|c| c.id == id)
                    .ok_or_else(crypto::denied)?;
                peer.phase = Phase::AwaitAuth {
                    credential: id.to_string(),
                };
                crypto::secret(&credential.secret)?
            };
            let host_nonce = crypto::random()?;
            let mut nonces = [0; 64];
            nonces[..32].copy_from_slice(&client_nonce);
            nonces[32..].copy_from_slice(&host_nonce);
            peer.code = crypto::digest(&nonces)[..8].to_string();
            peer.cipher = Some(Cipher::host(&psk, &client_nonce, &host_nonce));
            outgoing.push(data(client, serde_json::to_string(&json!({"kind":"challenge", "protocol":MOBILE_PROTOCOL, "nonce":crypto::encode(&host_nonce)})).map_err(io::Error::other)?));
            return Ok(());
        }
        let envelope: Envelope = serde_json::from_str(payload).map_err(|_| crypto::denied())?;
        let request = peer
            .cipher
            .as_mut()
            .ok_or_else(crypto::denied)?
            .open(envelope)?;
        match &peer.phase {
            Phase::AwaitPair => {
                if field(&request, "type")? != "pair" {
                    return Err(crypto::denied());
                }
                let name = field(&request, "device_name")?.trim();
                if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
                    return Err(crypto::denied());
                }
                if self
                    .peers
                    .values()
                    .filter(|p| matches!(&p.phase, Phase::Pending { .. }))
                    .count()
                    >= 4
                {
                    return Err(crypto::denied());
                }
                let request_id = uuid::Uuid::new_v4().to_string();
                peer.phase = Phase::Pending {
                    request_id: request_id.clone(),
                    name: name.into(),
                };
                let remaining = self
                    .bootstrap
                    .as_ref()
                    .ok_or_else(crypto::denied)?
                    .pairing
                    .expires_at
                    .saturating_sub(unix_now());
                peer.deadline = Instant::now() + Duration::from_secs(remaining);
                let code = peer.code.clone();
                outgoing.push(encrypted(
                    client,
                    peer,
                    &json!({"type":"pair_pending", "request_id":request_id, "code":code}),
                )?);
            }
            Phase::AwaitAuth { credential } => {
                if field(&request, "type")? != "auth" {
                    return Err(crypto::denied());
                }
                let credential = credential.clone();
                if !self
                    .store
                    .as_ref()
                    .is_some_and(|s| s.credentials.iter().any(|c| c.id == credential))
                {
                    return Err(crypto::denied());
                }
                peer.phase = Phase::Authenticated { credential };
                peer.deadline = Instant::now() + MAX_CONNECTION_LIFETIME;
                outgoing.push(encrypted(
                    client,
                    peer,
                    &json!({"type":"authenticated", "host_name":host_name()}),
                )?);
            }
            Phase::Authenticated { .. } => {
                if field(&request, "type")? == "output_ack" {
                    let pane = number(&request, "pane_id")?;
                    let id = field(&request, "subscription_id")?;
                    // Unsubscribe/reopen races leave legitimate ACKs in flight.
                    // An old generation must never release a new one's credit.
                    if let Some(subscription) = peer.subscriptions.get(&pane).filter(|s| s.id == id)
                    {
                        subscription
                            .credit
                            .acknowledge(number(&request, "sequence")?)?;
                    }
                    return Ok(());
                }
                if field(&request, "type")? != "request" {
                    return Err(crypto::denied());
                }
                let id = request
                    .get("id")
                    .and_then(Value::as_u64)
                    .filter(|id| *id > 0)
                    .ok_or_else(crypto::denied)?;
                let result = if matches!(
                    field(&request, "op")?,
                    "list" | "create_workspace" | "create_terminal" | "close_terminal" | "input"
                ) {
                    match self.schedule(client, peer, request, id) {
                        Ok(()) => return Ok(()),
                        Err(error) => Err(error),
                    }
                } else {
                    self.operation(client, peer, request).await
                };
                let response = response(id, result);
                outgoing.push(encrypted(client, peer, &response)?);
            }
            _ => return Err(crypto::denied()),
        }
        Ok(())
    }
    fn schedule(&self, client: &str, peer: &mut Peer, request: Value, id: u64) -> io::Result<()> {
        let permit = self
            .operation_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| io::Error::other("mobile operation queue is full"))?;
        if peer.jobs.fetch_add(1, Ordering::AcqRel) >= 32 {
            peer.jobs.fetch_sub(1, Ordering::Release);
            return Err(io::Error::other("too many pending mobile operations"));
        }
        let guard = OperationGuard {
            jobs: peer.jobs.clone(),
            _permit: permit,
        };
        if peer.request_queue.is_none() {
            let backend = self.backend.clone();
            let alive = peer.alive.clone();
            let failed = peer.failed.clone();
            let replies = self.replies.clone();
            let client = client.to_string();
            let (send, receive) = mpsc::sync_channel::<(Value, u64, OperationGuard)>(32);
            // Preserve input order, without parking revoke/disable/keepalive
            // behind a blocked PTY write. Capacity stays charged until completion.
            std::thread::Builder::new()
                .name("tty7-mobile-operations".into())
                .spawn(move || {
                    while alive.load(Ordering::Acquire) {
                        let (request, id, _guard) =
                            match receive.recv_timeout(Duration::from_millis(200)) {
                                Ok(job) => job,
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                Err(_) => break,
                            };
                        if !alive.load(Ordering::Acquire) {
                            break;
                        }
                        let result = backend.request(&request);
                        if alive.load(Ordering::Acquire)
                            && replies
                                .try_send((client.clone(), response(id, result)))
                                .is_err()
                        {
                            failed.store(true, Ordering::Release);
                            break;
                        }
                    }
                })?;
            peer.request_queue = Some(send);
        }
        peer.request_queue
            .as_ref()
            .expect("operation worker")
            .try_send((request, id, guard))
            .map_err(|_| io::Error::other("mobile operation queue is full or closed"))?;
        Ok(())
    }
    async fn operation(&self, client: &str, peer: &mut Peer, request: Value) -> io::Result<Value> {
        match field(&request, "op")? {
            "subscribe" => {
                let workspace = field(&request, "workspace_id")?.to_string();
                let pane = number(&request, "pane_id")?;
                if peer.subscriptions.len() >= MAX_SUBSCRIPTIONS
                    && !peer.subscriptions.contains_key(&pane)
                {
                    return Err(io::Error::other("maximum mobile subscriptions reached"));
                }
                let backend = self.backend.clone();
                let observation =
                    tokio::task::spawn_blocking({
                        let workspace = workspace.clone();
                        move || backend.observe(&workspace, pane)
                    })
                        .await
                        .map_err(|_| io::Error::other("observer failed"))??;
                let size = observation.size;
                peer.subscriptions.remove(&pane);
                let id = uuid::Uuid::new_v4().to_string();
                let credit = Arc::new(OutputCredit::new());
                let subscription = Subscription {
                    id: id.clone(),
                    workspace,
                    observer: Some((observation.pane.clone(), observation.observer_id)),
                    lease_started: false,
                    credit: credit.clone(),
                };
                let sink = MobileOutput {
                    output: self.output.clone(),
                    runtime: tokio::runtime::Handle::current(),
                    client: client.to_string(),
                    pane,
                    id: id.clone(),
                    credit,
                };
                std::thread::Builder::new()
                    .name("tty7-mobile-output".into())
                    .spawn(move || {
                        forward_output(observation, sink);
                    })?;
                peer.subscriptions.insert(pane, subscription);
                // The actor sends this response before it can dequeue output.
                Ok(json!({"subscription_id":id, "cols":size.cols, "rows":size.rows}))
            }
            "terminal_size" => {
                let size = terminal_size(&request)?;
                let pane_id = number(&request, "pane_id")?;
                let subscription_id = field(&request, "subscription_id")?;
                let workspace = field(&request, "workspace_id")?;
                let subscription = peer.subscriptions.get_mut(&pane_id).filter(|subscription| {
                    subscription.id == subscription_id
                        && subscription.workspace == workspace
                        && subscription.credit.active()
                }).ok_or_else(|| io::Error::other("terminal subscription is no longer active"))?;
                let (pane, observer_id) = subscription.observer.as_ref()
                    .ok_or_else(|| io::Error::other("terminal observer is unavailable"))?;
                let pane = pane.clone();
                let observer_id = *observer_id;
                let initial = !subscription.lease_started;
                tokio::task::spawn_blocking(move || pane.mobile_observer_resize(observer_id, size, initial))
                    .await
                    .map_err(|_| io::Error::other("terminal resize failed"))??;
                subscription.lease_started = true;
                Ok(Value::Null)
            }
            "unsubscribe" => {
                peer.subscriptions.remove(&number(&request, "pane_id")?);
                Ok(Value::Null)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "operation is not permitted for mobile access",
            )),
        }
    }
}
fn response(id: u64, result: io::Result<Value>) -> Value {
    match result {
        Ok(result) => json!({"type":"response", "id":id, "ok":true, "result":result}),
        Err(error) => json!({"type":"response", "id":id, "ok":false, "error":error.to_string()}),
    }
}

struct MobileOutput {
    output: async_mpsc::Sender<Output>,
    runtime: tokio::runtime::Handle,
    client: String,
    pane: u64,
    id: String,
    credit: Arc<OutputCredit>,
}
impl MobileOutput {
    fn send(&self, value: Value, sequence: Option<u64>, finished: bool) -> bool {
        let frame = Output {
            client: self.client.clone(),
            pane: self.pane,
            subscription: self.id.clone(),
            value,
            sequence,
            finished,
        };
        self.runtime.block_on(async {
            // Register the waiter before checking active, so cancellation cannot
            // fall between the check and a full output queue's wait.
            let cancelled = self.credit.cancelled.notified();
            tokio::pin!(cancelled);
            cancelled.as_mut().enable();
            if !self.credit.active() {
                return false;
            }
            tokio::select! {
                biased;
                _ = &mut cancelled => false,
                result = self.output.send(frame) => result.is_ok(),
            }
        })
    }
    fn sequence(&self) -> Option<u64> {
        match self.credit.reserve() {
            Ok(sequence) => Some(sequence),
            Err(error) => {
                if self.credit.active() {
                    self.error(&error.to_string());
                }
                None
            }
        }
    }
    fn frame(&self, sequence: u64, snapshot: bool, bytes: &[u8], size: WinSize) -> bool {
        self.send(
            json!({"type":"output", "pane_id":self.pane, "subscription_id":self.id,
            "sequence":sequence, "snapshot":snapshot, "data":crypto::encode(bytes),
            "cols":size.cols, "rows":size.rows}),
            Some(sequence),
            false,
        )
    }
    fn chunks(&self, snapshot: bool, bytes: &[u8], size: WinSize) -> bool {
        if bytes.is_empty() {
            return self
                .sequence()
                .is_some_and(|sequence| self.frame(sequence, snapshot, bytes, size));
        }
        for chunk in bytes.chunks(MAX_OUTPUT_BYTES) {
            let Some(sequence) = self.sequence() else {
                return false;
            };
            if !self.frame(sequence, snapshot, chunk, size) {
                return false;
            }
        }
        true
    }
    fn snapshot_complete(&self) -> bool {
        let sequence = self.credit.state.lock().reserved;
        self.send(
            json!({"type":"snapshot_complete", "pane_id":self.pane,
            "subscription_id":self.id, "sequence":sequence}),
            None,
            false,
        )
    }
    fn error(&self, message: &str) {
        // Error strings are runtime data too; keep a failed pane from producing
        // an oversized encrypted frame and disrupting the authenticated peer.
        let mut end = message.len().min(4096);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        self.send(
            json!({"type":"terminal_error", "pane_id":self.pane,
            "subscription_id":self.id, "message":&message[..end]}),
            None,
            true,
        );
    }
}

fn forward_output(mut observation: Observation, sink: MobileOutput) {
    // Move replay vectors out while retaining the RAII unobserve guard through
    // every credit wait. The gate accounts only for messages in the live FIFO.
    let initial = std::mem::take(&mut observation.initial);
    forward_observer(
        &observation.receiver,
        &observation.gate,
        observation.size,
        initial,
        || observation.pane.mobile_agent(),
        &sink,
    );
}

fn forward_observer(
    receiver: &mpsc::Receiver<DaemonMsg>,
    gate: &OutputGate,
    mut size: WinSize,
    initial: Vec<DaemonMsg>,
    agent_state: impl Fn() -> MobileAgentState,
    sink: &MobileOutput,
) {
    let mut initial = initial.into_iter();
    let mut pending = None;
    let mut staging = Vec::new();
    let mut last_agent = None;
    if initial.len() == 0 && !sink.snapshot_complete() {
        return;
    }
    while sink.credit.active() {
        let replaying = initial.len() > 0;
        let message = if let Some(message) = initial.next().or_else(|| pending.take()) {
            message
        } else {
            match receiver.recv_timeout(Duration::from_millis(200)) {
                Ok(message) => {
                    gate.sub(mobile_drained_bytes(&message));
                    message
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    sink.error("terminal observer disconnected unexpectedly");
                    return;
                }
            }
        };
        match message {
            DaemonMsg::Size(new) => {
                let changed = size != new;
                size = new;
                // Geometry is itself ordered output. Even a quiet shell must
                // resize both the screen mirror and renderer at this boundary.
                if !replaying && changed && !sink.chunks(false, &[], size) {
                    return;
                }
            }
            // Replay vectors are chunked directly; never copy the 8 MiB history
            // into a second buffer. Snapshot and dimension boundaries are exact.
            DaemonMsg::Snapshot(bytes) => {
                if !sink.chunks(true, &bytes, size) {
                    return;
                }
            }
            DaemonMsg::Output(bytes) if bytes.len() >= MAX_OUTPUT_BYTES => {
                if !sink.chunks(false, &bytes, size) {
                    return;
                }
            }
            DaemonMsg::Output(bytes) => {
                // Wait first: tiny PTY writes arriving under credit pressure
                // can then share one native parser round trip. Reuse this buffer.
                let Some(sequence) = sink.sequence() else {
                    return;
                };
                staging.clear();
                if staging.capacity() < MAX_OUTPUT_BYTES {
                    staging.reserve(MAX_OUTPUT_BYTES);
                }
                staging.extend_from_slice(&bytes);
                // Initial messages must not absorb the first live bytes into
                // their frame, or completion would no longer name the replay.
                while !replaying && staging.len() < MAX_OUTPUT_BYTES {
                    let message = match receiver.try_recv() {
                        Ok(message) => message,
                        Err(_) => break,
                    };
                    gate.sub(mobile_drained_bytes(&message));
                    match message {
                        DaemonMsg::Output(bytes)
                            if bytes.len() <= MAX_OUTPUT_BYTES - staging.len() =>
                        {
                            staging.extend_from_slice(&bytes);
                        }
                        boundary => {
                            pending = Some(boundary);
                            break;
                        }
                    }
                }
                if !sink.frame(sequence, false, &staging, size) {
                    return;
                }
            }
            DaemonMsg::Lease(holder) => {
                if !sink.send(
                    json!({"type":"terminal_lease", "pane_id":sink.pane,
                        "subscription_id":sink.id, "held":holder.is_some()}),
                    None,
                    false,
                ) {
                    return;
                }
            }
            DaemonMsg::Exited { code } => {
                sink.send(
                    json!({"type":"exited", "pane_id":sink.pane,
                    "subscription_id":sink.id, "code":code}),
                    None,
                    true,
                );
                return;
            }
            DaemonMsg::Error(message) => {
                sink.error(&message);
                return;
            }
            DaemonMsg::Agent(_) | DaemonMsg::AgentStatus(_) => {
                // Replay and queued status frames are wakeups, not current
                // state. Read the same atomic badge as list and the desktop.
                let state = agent_state();
                if last_agent != Some(state) {
                    last_agent = Some(state);
                    if !sink.send(
                        json!({"type":"agent", "pane_id":sink.pane,
                        "subscription_id":sink.id, "agent_revision":state.revision,
                        "agent":state.agent.map(|(name, status)| json!({
                            "name":name.display_name(), "status":status}))}),
                        None,
                        false,
                    ) {
                        return;
                    }
                }
            }
            _ => {}
        }
        if replaying && initial.len() == 0 && !sink.snapshot_complete() {
            return;
        }
    }
}

fn mobile_drained_bytes(message: &DaemonMsg) -> usize {
    match message {
        DaemonMsg::Output(bytes)
        | DaemonMsg::Snapshot(bytes)
        | DaemonMsg::Image(bytes)
        | DaemonMsg::ClipboardWrite(bytes) => bytes.len(),
        _ => 0,
    }
}

fn encrypted(client: &str, peer: &mut Peer, value: &Value) -> io::Result<Message> {
    Ok(data(
        client,
        peer.cipher
            .as_mut()
            .ok_or_else(crypto::denied)?
            .seal(value)?,
    ))
}
fn data(client: &str, payload: String) -> Message {
    Message::Text(
        json!({"type":"data", "client_id":client, "payload":payload})
            .to_string()
            .into(),
    )
}
fn disconnect(client: &str) -> Message {
    Message::Text(
        json!({"type":"disconnect", "client_id":client})
            .to_string()
            .into(),
    )
}
fn field<'a>(value: &'a Value, key: &str) -> io::Result<&'a str> {
    value.get(key).and_then(Value::as_str).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("missing or invalid {key}"),
        )
    })
}
fn number(value: &Value, key: &str) -> io::Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("missing or invalid {key}"),
            )
        })
}
fn terminal_size(request: &Value) -> io::Result<WinSize> {
    let cols = number(request, "cols")?;
    let rows = number(request, "rows")?;
    if !(20..=1000).contains(&cols) || !(5..=1000).contains(&rows) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal size must be 20..1000 columns and 5..1000 rows",
        ));
    }
    Ok(WinSize {
        cols: cols as u16,
        rows: rows as u16,
        cell_w: 0,
        cell_h: 0,
    })
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn host_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "tty7 computer".into())
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect()
}
async fn send_all(socket: &mut Option<Socket>, messages: Vec<Message>) -> io::Result<()> {
    if messages.is_empty() {
        return Ok(());
    }
    let socket = socket
        .as_mut()
        .ok_or_else(|| io::Error::other("relay is disconnected"))?;
    for message in messages {
        tokio::time::timeout(SEND_TIMEOUT, socket.send(message))
            .await
            .map_err(|_| io::Error::other("relay send timed out"))?
            .map_err(|_| io::Error::other("relay disconnected"))?;
    }
    Ok(())
}
async fn close(socket: &mut Option<Socket>) {
    if let Some(mut socket) = socket.take() {
        let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
    }
}

pub(super) fn validate_endpoint(
    address: &str,
    fingerprint: Option<&str>,
) -> io::Result<(String, Option<String>)> {
    let url = url::Url::parse(address.trim())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid relay URL"))?;
    if url.username() != ""
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
        || url.path() != "/v1/ws"
        || url.port_or_known_default().is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relay URL must end in /v1/ws with no credentials, query or fragment",
        ));
    }
    let pin = fingerprint
        .map(str::trim)
        .filter(|pin| !pin.is_empty())
        .map(str::to_ascii_lowercase);
    if pin
        .as_ref()
        .is_some_and(|pin| pin.len() != 64 || !pin.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "certificate pin must be 64 SHA256 hexadecimal characters",
        ));
    }
    match url.scheme() {
        "wss" => {
            if url.host_str().is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "relay host is missing",
                ));
            }
            // Saving a no-pin configuration is allowed, but connecting always fails closed.
        }
        "ws" => {
            let local = match url.host() {
                Some(url::Host::Domain(host)) => host == "localhost",
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            };
            if !local {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unencrypted relay connections are restricted to loopback",
                ));
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "relay scheme must be wss (or loopback ws for development)",
            ));
        }
    }
    Ok((url.to_string(), pin))
}
async fn connect(config: &RelayConfig) -> io::Result<Socket> {
    let (address, fingerprint) =
        validate_endpoint(&config.relay_url, config.tls_fingerprint.as_deref())?;
    let connector = if address.starts_with("wss:") {
        let pin = fingerprint.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a trusted relay certificate pin is required",
            )
        })?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(PinnedCertificate {
            pin,
            provider: provider.clone(),
        });
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        Some(Connector::Rustls(Arc::new(config)))
    } else {
        None
    };
    let websocket_config = WebSocketConfig::default()
        .max_message_size(Some(1024 * 1024))
        .max_frame_size(Some(1024 * 1024))
        .max_write_buffer_size(2 * 1024 * 1024);
    let (mut socket, _) = tokio_tungstenite::connect_async_tls_with_config(
        address,
        Some(websocket_config),
        true,
        connector,
    )
    .await
    .map_err(|error| match error {
        tokio_tungstenite::tungstenite::Error::Tls(error) => {
            io::Error::other(format!("relay TLS validation failed: {error}"))
        }
        tokio_tungstenite::tungstenite::Error::Io(error) => error,
        tokio_tungstenite::tungstenite::Error::Protocol(error) => {
            io::Error::other(format!("relay WebSocket protocol rejected: {error}"))
        }
        tokio_tungstenite::tungstenite::Error::Url(error) => {
            io::Error::other(format!("relay WebSocket URL rejected: {error}"))
        }
        tokio_tungstenite::tungstenite::Error::Http(response) => io::Error::other(format!(
            "relay WebSocket upgrade failed: HTTP {}",
            response.status()
        )),
        _ => io::Error::other("relay WebSocket handshake failed"),
    })?;
    let register =
        json!({"type":"register", "role":"host", "host_id":config.host_id, "token":config.token});
    tokio::time::timeout(
        SEND_TIMEOUT,
        socket.send(Message::Text(register.to_string().into())),
    )
    .await
    .map_err(|_| io::Error::other("relay registration timed out"))?
    .map_err(|_| io::Error::other("relay registration failed"))?;
    // Ready is mandatory: an open TCP/TLS connection is not a registered host.
    let ready = tokio::time::timeout(HANDSHAKE_LIFETIME, socket.next())
        .await
        .map_err(|_| io::Error::other("relay registration timed out"))?
        .ok_or_else(|| io::Error::other("relay closed during registration"))?
        .map_err(|_| io::Error::other("relay registration failed"))?;
    let Message::Text(ready) = ready else {
        return Err(io::Error::other("invalid relay registration reply"));
    };
    let ready: Value = serde_json::from_str(&ready)
        .map_err(|_| io::Error::other("invalid relay registration reply"))?;
    if ready.get("type").and_then(Value::as_str) != Some("ready") {
        return Err(io::Error::other("relay refused host registration"));
    }
    Ok(socket)
}

#[derive(Debug)]
struct PinnedCertificate {
    pin: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl rustls::client::danger::ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        certificate: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        name: &rustls::pki_types::ServerName<'_>,
        ocsp: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if crypto::digest(certificate.as_ref()) != self.pin {
            return Err(rustls::Error::General(
                "relay certificate pin mismatch".into(),
            ));
        }
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(certificate.clone())
            .map_err(|_| rustls::Error::General("invalid pinned relay certificate".into()))?;
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            self.provider.clone(),
        )
        .build()
        .map_err(|_| rustls::Error::General("invalid pinned certificate verifier".into()))?;
        // Trust only the pinned leaf, but still check validity, IP/DNS SAN and key usage.
        verifier.verify_server_cert(certificate, intermediates, name, ocsp, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_grid_bounds_reject_nonintegers_and_do_not_truncate() {
        for (cols, rows) in [(20, 5), (1000, 1000), (42, 28)] {
            assert_eq!(terminal_size(&json!({"cols":cols, "rows":rows})).unwrap(), size(cols, rows));
        }
        for request in [
            json!({"cols":19, "rows":5}),
            json!({"cols":20, "rows":4}),
            json!({"cols":1001, "rows":5}),
            json!({"cols":20, "rows":1001}),
            json!({"cols":65556, "rows":5}),
            json!({"cols":-20, "rows":5}),
            json!({"cols":20.5, "rows":5}),
            json!({"cols":"20", "rows":5}),
            json!({"cols":20}),
        ] {
            assert_eq!(terminal_size(&request).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[tokio::test]
    async fn phone_resize_requires_current_subscription_workspace_and_active_credit() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "current");
        let mut peer = service.peers.remove("phone").unwrap();
        for (pane, subscription, workspace) in [
            (7, "retired", "workspace"),
            (8, "current", "workspace"),
            (7, "current", "different"),
        ] {
            let error = service.operation("phone", &mut peer, json!({
                "op":"terminal_size", "pane_id":pane, "subscription_id":subscription,
                "workspace_id":workspace, "cols":42, "rows":28,
            })).await.unwrap_err();
            assert_eq!(error.to_string(), "terminal subscription is no longer active");
            assert!(!peer.subscriptions[&7].lease_started);
            assert!(credit.active());
        }
        credit.cancel();
        let error = service.operation("phone", &mut peer, json!({
            "op":"terminal_size", "pane_id":7, "subscription_id":"current",
            "workspace_id":"workspace", "cols":42, "rows":28,
        })).await.unwrap_err();
        assert_eq!(error.to_string(), "terminal subscription is no longer active");
    }

    #[tokio::test]
    async fn phone_lease_and_restore_keep_output_and_empty_grid_markers_ordered() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let (sender, completed, producer) = worker(
            service.output.clone(), credit, Vec::new(),
            vec![
                DaemonMsg::Output(b"desk-before".to_vec()),
                DaemonMsg::Size(size(42, 28)),
                DaemonMsg::Lease(Some("phone".into())),
                DaemonMsg::Output(b"phone".to_vec()),
                DaemonMsg::Lease(None),
                DaemonMsg::Size(size(120, 40)),
                DaemonMsg::Output(b"desk-after".to_vec()),
                DaemonMsg::Exited { code: Some(0) },
            ],
        );
        receive_completion(&mut service, 0).await;
        let mut grids = Vec::new();
        let mut held = Vec::new();
        let mut sequence = 0;
        loop {
            let output = receive_output(&mut service).await;
            match output.value["type"].as_str().unwrap() {
                "output" => {
                    sequence += 1;
                    assert_eq!(output.sequence, Some(sequence));
                    grids.push((
                        output.value["cols"].as_u64().unwrap(),
                        output.value["rows"].as_u64().unwrap(),
                        crypto::decode(output.value["data"].as_str().unwrap()).unwrap(),
                    ));
                    service.output_frame(output, &mut Vec::new());
                    acknowledge(&mut service, sequence - 1, "feed", sequence).await;
                }
                "terminal_lease" => {
                    assert_eq!(output.value["subscription_id"], "feed");
                    assert_eq!(output.value["pane_id"], 7);
                    assert!(output.sequence.is_none() && !output.finished);
                    held.push((sequence, output.value["held"].as_bool().unwrap()));
                    service.output_frame(output, &mut Vec::new());
                }
                "exited" => { service.output_frame(output, &mut Vec::new()); break; }
                kind => panic!("unexpected frame {kind}"),
            }
        }
        assert_eq!(grids, vec![
            (80, 24, b"desk-before".to_vec()),
            (42, 28, Vec::new()),
            (42, 28, b"phone".to_vec()),
            (120, 40, Vec::new()),
            (120, 40, b"desk-after".to_vec()),
        ]);
        assert_eq!(held, vec![(2, true), (3, false)]);
        completed.await.unwrap();
        producer.join().unwrap();
        drop(sender);
    }

    fn size(cols: u16, rows: u16) -> WinSize {
        WinSize {
            cols,
            rows,
            cell_w: 0,
            cell_h: 0,
        }
    }

    fn authenticated_peer() -> Peer {
        let mut peer = Peer::new(Phase::Authenticated {
            credential: "approved".into(),
        });
        peer.cipher = Some(Cipher::host(&[1; 32], &[2; 32], &[2; 32]));
        peer
    }

    fn test_service() -> Service {
        Service::new(
            Err(io::Error::other("test")),
            Arc::new(NoOperations(Arc::new(AtomicUsize::new(0)))),
        )
    }

    fn add_subscription(
        service: &mut Service,
        client: &str,
        pane: u64,
        id: &str,
    ) -> Arc<OutputCredit> {
        let credit = Arc::new(OutputCredit::new());
        service
            .peers
            .entry(client.into())
            .or_insert_with(authenticated_peer)
            .subscriptions
            .insert(
                pane,
                Subscription {
                    id: id.into(),
                    workspace: "workspace".into(),
                    observer: None,
                    lease_started: false,
                    credit: credit.clone(),
                },
            );
        credit
    }

    fn worker(
        output: async_mpsc::Sender<Output>,
        credit: Arc<OutputCredit>,
        initial: Vec<DaemonMsg>,
        messages: Vec<DaemonMsg>,
    ) -> (
        mpsc::Sender<DaemonMsg>,
        tokio::sync::oneshot::Receiver<()>,
        std::thread::JoinHandle<()>,
    ) {
        let (send, receive) = mpsc::channel();
        let gate = Arc::new(OutputGate::new());
        for message in messages {
            gate.add(mobile_drained_bytes(&message));
            send.send(message).unwrap();
        }
        let (completed, producer) = observer_worker(output, credit, initial, receive, gate);
        (send, completed, producer)
    }

    fn observer_worker(
        output: async_mpsc::Sender<Output>,
        credit: Arc<OutputCredit>,
        initial: Vec<DaemonMsg>,
        receive: mpsc::Receiver<DaemonMsg>,
        gate: Arc<OutputGate>,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::thread::JoinHandle<()>,
    ) {
        let sink = MobileOutput {
            output,
            runtime: tokio::runtime::Handle::current(),
            client: "phone".into(),
            pane: 7,
            id: "feed".into(),
            credit,
        };
        let (finished, completed) = tokio::sync::oneshot::channel();
        let producer = std::thread::spawn(move || {
            forward_observer(
                &receive, &gate, size(80, 24), initial, MobileAgentState::default, &sink,
            );
            let _ = finished.send(());
        });
        (completed, producer)
    }

    async fn receive_output(service: &mut Service) -> Output {
        tokio::time::timeout(Duration::from_secs(1), service.outputs.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn receive_completion(service: &mut Service, sequence: u64) {
        let marker = receive_output(service).await;
        assert_eq!(
            marker.value,
            json!({"type":"snapshot_complete", "pane_id":7,
            "subscription_id":"feed", "sequence":sequence})
        );
        assert_eq!(
            marker.sequence, None,
            "completion must not reserve output credit"
        );
        assert!(!marker.finished);
        let mut outgoing = Vec::new();
        service.output_frame(marker, &mut outgoing);
        assert_eq!(outgoing.len(), 1);
        assert!(service.peers["phone"].subscriptions.contains_key(&7));
    }

    async fn client_frame(service: &mut Service, seq: u64, value: Value) -> Vec<Message> {
        let mut outgoing = Vec::new();
        service
            .frame(
                &json!({"type":"data", "client_id":"phone",
            "payload":client_payload_at(value, seq)})
                .to_string(),
                &mut outgoing,
            )
            .await
            .unwrap();
        outgoing
    }

    async fn acknowledge(service: &mut Service, envelope: u64, id: &str, sequence: u64) {
        assert!(
            client_frame(
                service,
                envelope,
                json!({"type":"output_ack", "pane_id":7,
            "subscription_id":id, "sequence":sequence})
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn parsed_credit_pauses_replay_and_cumulative_ack_releases_fifo_without_blocking_control()
    {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let replay: Vec<u8> = (0..7)
            .flat_map(|index| std::iter::repeat_n(index, MAX_OUTPUT_BYTES))
            .collect();
        let (sender, completed, producer) = worker(
            service.output.clone(),
            credit,
            vec![DaemonMsg::Snapshot(replay.clone())],
            Vec::new(),
        );
        let mut bytes = Vec::new();
        for sequence in 1..=4 {
            let output = receive_output(&mut service).await;
            assert_eq!(output.sequence, Some(sequence));
            bytes.extend(crypto::decode(output.value["data"].as_str().unwrap()).unwrap());
            let mut outgoing = Vec::new();
            service.output_frame(output, &mut outgoing);
            assert_eq!(outgoing.len(), 1);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), service.outputs.recv())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                client_frame(
                    &mut service,
                    0,
                    json!({"type":"request", "id":19, "op":"list"})
                )
            )
            .await
            .unwrap()
            .is_empty()
        );
        let (client, reply) =
            tokio::time::timeout(Duration::from_secs(1), service.reply_receiver.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(client, "phone");
        assert_eq!(reply["id"], 19);
        assert_eq!(reply["ok"], true);
        assert!(service.peers.contains_key("phone"));
        acknowledge(&mut service, 1, "feed", 2).await;
        for sequence in 5..=6 {
            let output = receive_output(&mut service).await;
            assert_eq!(output.sequence, Some(sequence));
            bytes.extend(crypto::decode(output.value["data"].as_str().unwrap()).unwrap());
            service.output_frame(output, &mut Vec::new());
        }
        acknowledge(&mut service, 2, "feed", 2).await;
        acknowledge(&mut service, 3, "feed", 1).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), service.outputs.recv())
                .await
                .is_err()
        );
        acknowledge(&mut service, 4, "feed", 6).await;
        let output = receive_output(&mut service).await;
        assert_eq!(output.sequence, Some(7));
        bytes.extend(crypto::decode(output.value["data"].as_str().unwrap()).unwrap());
        service.output_frame(output, &mut Vec::new());
        assert_eq!(bytes, replay);
        receive_completion(&mut service, 7).await;
        client_frame(
            &mut service,
            5,
            json!({"type":"request", "id":20, "op":"unsubscribe", "pane_id":7}),
        )
        .await;
        tokio::time::timeout(Duration::from_millis(250), completed)
            .await
            .unwrap()
            .unwrap();
        producer.join().unwrap();
        drop(sender);
    }

    #[tokio::test]
    async fn full_owned_replay_preserves_startup_live_fifo_under_blocked_credit() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let budget = crate::daemon::pane::OBSERVER_BUDGET as usize;
        let initial = vec![
            DaemonMsg::Snapshot(b"\x1b[?1049h".to_vec()),
            DaemonMsg::Size(size(100, 30)),
            DaemonMsg::Snapshot(vec![b'x'; budget]),
            DaemonMsg::Cwd("/work".into()),
        ];
        let (sender, receiver) = mpsc::channel();
        let gate = Arc::new(OutputGate::new());
        // Simulate PTY metadata and its first output before the worker starts.
        sender.send(DaemonMsg::Size(size(120, 40))).unwrap();
        gate.add(b"first live".len());
        sender
            .send(DaemonMsg::Output(b"first live".to_vec()))
            .unwrap();
        let (completed, producer) = observer_worker(
            service.output.clone(),
            credit.clone(),
            initial,
            receiver,
            gate.clone(),
        );
        let frames = 1 + budget.div_ceil(MAX_OUTPUT_BYTES) as u64;
        let mut replay_bytes = 0;
        let mut envelope = 1;
        for sequence in 1..=frames {
            let output = receive_output(&mut service).await;
            assert_eq!(output.sequence, Some(sequence));
            assert_eq!(output.value["snapshot"], true);
            let bytes = crypto::decode(output.value["data"].as_str().unwrap()).unwrap();
            if sequence == 1 {
                assert_eq!(bytes, b"\x1b[?1049h");
                assert_eq!(output.value["cols"], 80);
                assert_eq!(output.value["rows"], 24);
            } else {
                assert_eq!(output.value["cols"], 100);
                assert_eq!(output.value["rows"], 30);
                assert!(bytes.iter().all(|byte| *byte == b'x'));
                replay_bytes += bytes.len();
            }
            service.output_frame(output, &mut Vec::new());
            if sequence == OUTPUT_WINDOW {
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), service.outputs.recv())
                        .await
                        .is_err()
                );
                assert_eq!(
                    gate.queued_bytes(),
                    b"first live".len() as i64,
                    "replay neither charges nor drains live bytes"
                );
                assert!(
                    client_frame(
                        &mut service,
                        0,
                        json!({"type":"request", "id":19, "op":"list"})
                    )
                    .await
                    .is_empty()
                );
                let (_, reply) =
                    tokio::time::timeout(Duration::from_secs(1), service.reply_receiver.recv())
                        .await
                        .unwrap()
                        .unwrap();
                assert_eq!(
                    reply["ok"], true,
                    "control remains usable with saturated replay credit"
                );
            }
            if sequence >= OUTPUT_WINDOW {
                acknowledge(&mut service, envelope, "feed", sequence).await;
                envelope += 1;
            }
        }
        assert_eq!(
            replay_bytes, budget,
            "all retained history reaches the consumer exactly once"
        );
        receive_completion(&mut service, frames).await;
        let resize = receive_output(&mut service).await;
        assert_eq!(resize.sequence, Some(frames + 1));
        assert_eq!(resize.value["cols"], 120);
        assert_eq!(resize.value["rows"], 40);
        assert_eq!(resize.value["data"], "");
        service.output_frame(resize, &mut Vec::new());
        let live = receive_output(&mut service).await;
        assert_eq!(live.sequence, Some(frames + 2));
        assert_eq!(live.value["snapshot"], false);
        assert_eq!(live.value["cols"], 120);
        assert_eq!(live.value["rows"], 40);
        assert_eq!(
            crypto::decode(live.value["data"].as_str().unwrap()).unwrap(),
            b"first live"
        );
        service.output_frame(live, &mut Vec::new());
        assert_eq!(gate.queued_bytes(), 0);
        assert!(service.peers["phone"].subscriptions.contains_key(&7));
        service.peers.remove("phone");
        tokio::time::timeout(Duration::from_millis(250), completed)
            .await
            .unwrap()
            .unwrap();
        producer.join().unwrap();
        drop(sender);
    }

    #[tokio::test]
    async fn tiny_live_writes_accumulated_under_credit_pressure_share_one_frame() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let (sender, completed, producer) = worker(
            service.output.clone(),
            credit,
            vec![DaemonMsg::Snapshot(vec![b'x'; 4 * MAX_OUTPUT_BYTES])],
            vec![DaemonMsg::Output(b"a".to_vec())],
        );
        for _ in 0..4 {
            let output = receive_output(&mut service).await;
            service.output_frame(output, &mut Vec::new());
        }
        // Completion bypasses the saturated four-frame parsed-credit window.
        receive_completion(&mut service, 4).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), service.outputs.recv())
                .await
                .is_err()
        );
        sender.send(DaemonMsg::Output(b"b".to_vec())).unwrap();
        sender.send(DaemonMsg::Output(b"c".to_vec())).unwrap();
        sender.send(DaemonMsg::Size(size(100, 30))).unwrap();
        sender
            .send(DaemonMsg::Output(b"next-size".to_vec()))
            .unwrap();
        acknowledge(&mut service, 0, "feed", 1).await;
        let output = receive_output(&mut service).await;
        assert_eq!(output.sequence, Some(5));
        assert_eq!(output.value["snapshot"], false);
        assert_eq!(output.value["cols"], 80);
        assert_eq!(
            crypto::decode(output.value["data"].as_str().unwrap()).unwrap(),
            b"abc"
        );
        service.output_frame(output, &mut Vec::new());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), service.outputs.recv())
                .await
                .is_err()
        );
        acknowledge(&mut service, 1, "feed", 2).await;
        let output = receive_output(&mut service).await;
        assert_eq!(output.sequence, Some(6));
        assert_eq!(output.value["cols"], 100);
        assert_eq!(output.value["data"], "");
        service.output_frame(output, &mut Vec::new());
        acknowledge(&mut service, 2, "feed", 3).await;
        let output = receive_output(&mut service).await;
        assert_eq!(output.sequence, Some(7));
        assert_eq!(output.value["cols"], 100);
        assert_eq!(
            crypto::decode(output.value["data"].as_str().unwrap()).unwrap(),
            b"next-size"
        );
        service.output_frame(output, &mut Vec::new());
        service.peers.remove("phone");
        tokio::time::timeout(Duration::from_millis(250), completed)
            .await
            .unwrap()
            .unwrap();
        producer.join().unwrap();
        drop(sender);
    }

    #[tokio::test]
    async fn owned_replay_preserves_modes_dimensions_empty_tail_and_completion_before_live() {
        for metadata in [false, true] {
            let mut service = test_service();
            let credit = add_subscription(&mut service, "phone", 7, "feed");
            let history = vec![b'x'; MAX_OUTPUT_BYTES + 1];
            let mut messages = vec![
                DaemonMsg::Snapshot(b"\x1b[?1049h".to_vec()),
                DaemonMsg::Size(size(100, 30)),
                DaemonMsg::Snapshot(history.clone()),
                DaemonMsg::Size(size(90, 20)),
                DaemonMsg::Snapshot(Vec::new()),
            ];
            if metadata {
                messages.push(DaemonMsg::Cwd("/work".into()));
                messages.push(DaemonMsg::Prompt {
                    active: true,
                    at_prompt: false,
                    last_exit: None,
                });
            }
            let (sender, completed, producer) = worker(
                service.output.clone(),
                credit.clone(),
                messages,
                vec![
                    DaemonMsg::Output(b"a".to_vec()),
                    DaemonMsg::Output(b"b".to_vec()),
                ],
            );
            for (index, (cols, rows, bytes)) in [
                (80, 24, b"\x1b[?1049h".to_vec()),
                (100, 30, history[..MAX_OUTPUT_BYTES].to_vec()),
                (100, 30, history[MAX_OUTPUT_BYTES..].to_vec()),
                (90, 20, Vec::new()),
            ]
            .into_iter()
            .enumerate()
            {
                let output = receive_output(&mut service).await;
                assert_eq!(output.sequence, Some(index as u64 + 1));
                assert_eq!(output.value["snapshot"], true);
                assert_eq!(output.value["cols"], cols);
                assert_eq!(output.value["rows"], rows);
                assert_eq!(
                    crypto::decode(output.value["data"].as_str().unwrap()).unwrap(),
                    bytes
                );
                service.output_frame(output, &mut Vec::new());
            }
            receive_completion(&mut service, 4).await;
            assert_eq!(credit.state.lock().reserved, 4);
            assert_eq!(credit.state.lock().acknowledged, 0);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), service.outputs.recv())
                    .await
                    .is_err()
            );
            acknowledge(&mut service, 0, "feed", 1).await;
            let live = receive_output(&mut service).await;
            assert_eq!(live.sequence, Some(5));
            assert_eq!(live.value["snapshot"], false);
            assert_eq!(live.value["cols"], 90);
            assert_eq!(live.value["rows"], 20);
            assert_eq!(
                crypto::decode(live.value["data"].as_str().unwrap()).unwrap(),
                b"ab"
            );
            service.output_frame(live, &mut Vec::new());
            service.peers.remove("phone");
            tokio::time::timeout(Duration::from_millis(250), completed)
                .await
                .unwrap()
                .unwrap();
            producer.join().unwrap();
            drop(sender);
        }
    }

    #[tokio::test]
    async fn empty_replay_keeps_dimension_reset_frame_before_completion() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let (sender, completed, producer) = worker(
            service.output.clone(),
            credit,
            vec![
                DaemonMsg::Size(size(110, 40)),
                DaemonMsg::Snapshot(Vec::new()),
            ],
            Vec::new(),
        );
        let output = receive_output(&mut service).await;
        assert_eq!(output.sequence, Some(1));
        assert_eq!(output.value["snapshot"], true);
        assert_eq!(output.value["data"], "");
        assert_eq!(output.value["cols"], 110);
        assert_eq!(output.value["rows"], 40);
        service.output_frame(output, &mut Vec::new());
        receive_completion(&mut service, 1).await;
        service.peers.remove("phone");
        tokio::time::timeout(Duration::from_millis(250), completed)
            .await
            .unwrap()
            .unwrap();
        producer.join().unwrap();
        drop(sender);
    }

    #[tokio::test]
    async fn partial_owned_replay_exit_or_error_never_emits_completion() {
        for terminal in [
            DaemonMsg::Exited { code: Some(3) },
            DaemonMsg::Error("replay failed".into()),
        ] {
            let mut service = test_service();
            let credit = add_subscription(&mut service, "phone", 7, "feed");
            add_subscription(&mut service, "phone", 8, "other-pane");
            let mut messages = vec![DaemonMsg::Snapshot(b"partial".to_vec())];
            let event = if matches!(&terminal, DaemonMsg::Exited { .. }) {
                "exited"
            } else {
                "terminal_error"
            };
            messages.push(terminal);
            let (sender, completed, producer) =
                worker(service.output.clone(), credit, messages, Vec::new());
            drop(sender);
            let output = receive_output(&mut service).await;
            assert_eq!(output.value["type"], "output");
            service.output_frame(output, &mut Vec::new());
            let output = receive_output(&mut service).await;
            assert_eq!(output.value["type"], event);
            assert!(output.finished);
            service.output_frame(output, &mut Vec::new());
            tokio::time::timeout(Duration::from_millis(250), completed)
                .await
                .unwrap()
                .unwrap();
            producer.join().unwrap();
            assert!(service.outputs.try_recv().is_err());
            assert!(!service.peers["phone"].subscriptions.contains_key(&7));
            assert!(service.peers["phone"].subscriptions.contains_key(&8));
        }
    }

    #[tokio::test]
    async fn cancelled_workers_stop_while_waiting_for_credit_or_output_queue() {
        for capacity in [1, 128] {
            let (output, mut receiver) = async_mpsc::channel(capacity);
            let credit = Arc::new(OutputCredit::new());
            let (sender, completed, producer) = worker(
                output,
                credit.clone(),
                vec![DaemonMsg::Snapshot(vec![b'x'; 5 * MAX_OUTPUT_BYTES])],
                Vec::new(),
            );
            let target = if capacity == 1 { 2 } else { OUTPUT_WINDOW };
            tokio::time::timeout(Duration::from_secs(1), async {
                while credit.state.lock().reserved < target {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            drop(Subscription {
                id: "feed".into(),
                workspace: "workspace".into(),
                observer: None,
                lease_started: false,
                credit,
            });
            tokio::time::timeout(Duration::from_millis(250), completed)
                .await
                .unwrap()
                .unwrap();
            producer.join().unwrap();
            while let Ok(output) = receiver.try_recv() {
                assert_ne!(output.value["type"], "snapshot_complete");
            }
            drop(sender);
        }
    }

    #[tokio::test]
    async fn stale_ack_and_queued_events_cannot_touch_a_reopened_same_pane() {
        let mut service = test_service();
        let old = add_subscription(&mut service, "phone", 7, "old");
        let sequence = old.reserve().unwrap();
        let stale = Output {
            client: "phone".into(),
            pane: 7,
            subscription: "old".into(),
            value: json!({"type":"terminal_error", "pane_id":7, "subscription_id":"old", "message":"old failure"}),
            sequence: Some(sequence),
            finished: true,
        };
        client_frame(
            &mut service,
            0,
            json!({"type":"request", "id":1, "op":"unsubscribe", "pane_id":7}),
        )
        .await;
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let (sender, completed, producer) = worker(
            service.output.clone(),
            credit.clone(),
            vec![DaemonMsg::Snapshot(vec![b'y'; 5 * MAX_OUTPUT_BYTES])],
            Vec::new(),
        );
        for sequence in 1..=4 {
            let output = receive_output(&mut service).await;
            assert_eq!(output.sequence, Some(sequence));
            service.output_frame(output, &mut Vec::new());
        }
        let mut outgoing = Vec::new();
        service.output_frame(stale, &mut outgoing);
        service.output_frame(Output { client: "phone".into(), pane: 7, subscription: "old".into(),
            value: json!({"type":"snapshot_complete", "pane_id":7, "subscription_id":"old", "sequence":sequence}),
            sequence: None, finished: false }, &mut outgoing);
        assert!(outgoing.is_empty());
        acknowledge(&mut service, 1, "old", u64::MAX).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), service.outputs.recv())
                .await
                .is_err()
        );
        assert!(service.peers.contains_key("phone"));
        acknowledge(&mut service, 2, "feed", 4).await;
        let output = receive_output(&mut service).await;
        assert_eq!(output.sequence, Some(5));
        assert_eq!(output.subscription, "feed");
        service.output_frame(output, &mut Vec::new());
        receive_completion(&mut service, 5).await;
        service.peers.remove("phone");
        tokio::time::timeout(Duration::from_millis(250), completed)
            .await
            .unwrap()
            .unwrap();
        producer.join().unwrap();
        drop(sender);
    }

    #[tokio::test]
    async fn ack_rejects_zero_future_and_reserved_but_not_sent_sequences() {
        for (sent, sequence) in [(0, json!(1)), (1, json!(0)), (1, json!(2)), (1, json!("1"))] {
            let mut service = test_service();
            let credit = add_subscription(&mut service, "phone", 7, "feed");
            credit.reserve().unwrap();
            if sent == 1 {
                credit.sent(1).unwrap();
            }
            let outgoing = client_frame(
                &mut service,
                0,
                json!({"type":"output_ack", "pane_id":7,
                "subscription_id":"feed", "sequence":sequence}),
            )
            .await;
            assert!(!service.peers.contains_key("phone"));
            assert_eq!(outgoing.len(), 1);
            assert!(
                matches!(&outgoing[0], Message::Text(text) if serde_json::from_str::<Value>(text).unwrap()["type"] == "disconnect")
            );
        }
    }

    #[tokio::test]
    async fn live_fragments_coalesce_without_crossing_dimensions_snapshots_or_size_limit() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let snapshot = vec![b'z'; MAX_OUTPUT_BYTES + 1];
        let (sender, completed, producer) = worker(
            service.output.clone(),
            credit,
            Vec::new(),
            vec![
                DaemonMsg::Output(vec![0xc3]),
                DaemonMsg::Output(vec![0xa9, 0x1b, b'[', b'3', b'1']),
                DaemonMsg::Size(size(100, 30)),
                DaemonMsg::Output(b"mred".to_vec()),
                DaemonMsg::Snapshot(snapshot.clone()),
                DaemonMsg::Output(b"tail".to_vec()),
                DaemonMsg::Exited { code: Some(0) },
            ],
        );
        receive_completion(&mut service, 0).await;
        let expected = [
            (false, 80, 24, vec![0xc3, 0xa9, 0x1b, b'[', b'3', b'1']),
            (false, 100, 30, Vec::new()),
            (false, 100, 30, b"mred".to_vec()),
            (true, 100, 30, snapshot[..MAX_OUTPUT_BYTES].to_vec()),
            (true, 100, 30, snapshot[MAX_OUTPUT_BYTES..].to_vec()),
            (false, 100, 30, b"tail".to_vec()),
        ];
        for (index, (snapshot, cols, rows, bytes)) in expected.into_iter().enumerate() {
            let output = receive_output(&mut service).await;
            assert_eq!(output.value["subscription_id"], "feed");
            assert_eq!(output.value["snapshot"], snapshot);
            assert_eq!(output.value["cols"], cols);
            assert_eq!(output.value["rows"], rows);
            assert_eq!(
                crypto::decode(output.value["data"].as_str().unwrap()).unwrap(),
                bytes
            );
            service.output_frame(output, &mut Vec::new());
            acknowledge(&mut service, index as u64, "feed", index as u64 + 1).await;
        }
        let output = receive_output(&mut service).await;
        assert_eq!(output.value["type"], "exited");
        assert_eq!(output.value["subscription_id"], "feed");
        assert_eq!(output.value["code"], 0);
        service.output_frame(output, &mut Vec::new());
        assert!(service.peers["phone"].subscriptions.is_empty());
        assert!(service.peers.contains_key("phone"));
        tokio::time::timeout(Duration::from_millis(250), completed)
            .await
            .unwrap()
            .unwrap();
        producer.join().unwrap();
        drop(sender);
    }

    #[tokio::test]
    async fn observer_errors_and_unexpected_disconnect_remove_only_the_failed_subscription() {
        for message in [
            Some(DaemonMsg::Error("runtime output failed".into())),
            Some(DaemonMsg::Error("界".repeat(crypto::MAX_PLAINTEXT))),
            None,
        ] {
            let mut service = test_service();
            let credit = add_subscription(&mut service, "phone", 7, "feed");
            add_subscription(&mut service, "phone", 8, "other-pane");
            add_subscription(&mut service, "other-phone", 9, "other-peer");
            let (sender, completed, producer) = worker(
                service.output.clone(),
                credit,
                Vec::new(),
                message.into_iter().collect(),
            );
            drop(sender);
            receive_completion(&mut service, 0).await;
            let output = receive_output(&mut service).await;
            assert_eq!(output.value["type"], "terminal_error");
            assert_eq!(output.value["subscription_id"], "feed");
            assert!(!output.value["message"].as_str().unwrap().is_empty());
            assert!(output.value["message"].as_str().unwrap().len() <= 4096);
            let mut outgoing = Vec::new();
            service.output_frame(output, &mut outgoing);
            assert_eq!(outgoing.len(), 1);
            assert!(!service.peers["phone"].subscriptions.contains_key(&7));
            assert!(service.peers["phone"].subscriptions.contains_key(&8));
            assert!(service.peers["other-phone"].subscriptions.contains_key(&9));
            acknowledge(&mut service, 0, "feed", u64::MAX).await;
            assert!(
                client_frame(
                    &mut service,
                    1,
                    json!({"type":"request", "id":42, "op":"list"})
                )
                .await
                .is_empty()
            );
            let (_, reply) =
                tokio::time::timeout(Duration::from_secs(1), service.reply_receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(reply["id"], 42);
            assert_eq!(reply["ok"], true);
            tokio::time::timeout(Duration::from_millis(250), completed)
                .await
                .unwrap()
                .unwrap();
            producer.join().unwrap();
            assert!(
                service.outputs.try_recv().is_err(),
                "failed live observer must not emit more output"
            );
        }
    }

    #[tokio::test]
    async fn legacy_protocol_fails_closed_before_pairing_or_deriving_a_session() {
        let mut service = test_service();
        service.bootstrap = Some(Bootstrap {
            id: "pair-id".into(),
            secret: [1; 32],
            pairing: Pairing {
                uri: String::new(),
                expires_at: unix_now() + 60,
            },
            attempts: 0,
        });
        for protocol in [Value::Null, json!(0), json!(1), json!("2"), json!(2.0)] {
            let mut hello = json!({"kind":"hello", "credential_id":"pair:pair-id", "nonce":crypto::encode(&[2; 32])});
            if !protocol.is_null() {
                hello["protocol"] = protocol;
            }
            let mut peer = Peer::new(Phase::Hello);
            let mut outgoing = Vec::new();
            assert!(
                service
                    .peer_frame("phone", &mut peer, &hello.to_string(), &mut outgoing)
                    .await
                    .is_err()
            );
            assert!(peer.cipher.is_none());
            assert!(matches!(peer.phase, Phase::Hello));
            assert!(outgoing.is_empty());
            assert_eq!(service.bootstrap.as_ref().unwrap().attempts, 0);
        }
        let mut peer = Peer::new(Phase::Hello);
        let mut outgoing = Vec::new();
        service
            .peer_frame(
                "phone",
                &mut peer,
                &json!({"kind":"hello", "protocol":2,
            "credential_id":"pair:pair-id", "nonce":crypto::encode(&[2; 32])})
                .to_string(),
                &mut outgoing,
            )
            .await
            .unwrap();
        let Message::Text(text) = &outgoing[0] else {
            panic!("expected challenge");
        };
        let frame: Value = serde_json::from_str(text).unwrap();
        let challenge: Value = serde_json::from_str(frame["payload"].as_str().unwrap()).unwrap();
        assert_eq!(challenge["protocol"], 2);
        assert_eq!(challenge["kind"], "challenge");
        assert!(peer.cipher.is_some());
        assert!(matches!(peer.phase, Phase::AwaitPair));
    }

    struct NoOperations(Arc<std::sync::atomic::AtomicUsize>);
    impl Backend for NoOperations {
        fn request(&self, _: &Value) -> io::Result<Value> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Value::Null)
        }
        fn observe(&self, _: &str, _: u64) -> io::Result<Observation> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::other("not available in this test"))
        }
    }

    fn client_payload(value: Value) -> String {
        client_payload_at(value, 0)
    }

    fn client_payload_at(value: Value, seq: u64) -> String {
        use aes_gcm::{
            Aes256Gcm, KeyInit, Nonce,
            aead::{Aead, Payload},
        };
        let salt = [2; 64];
        let mut key = [0; 32];
        hkdf::Hkdf::<sha2_mobile::Sha256>::new(Some(&salt), &[1; 32])
            .expand(crypto::C2H, &mut key)
            .unwrap();
        let cipher = Aes256Gcm::new((&key).into());
        let bytes = serde_json::to_vec(&value).unwrap();
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&seq.to_be_bytes());
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &bytes,
                    aad: crypto::C2H,
                },
            )
            .unwrap();
        serde_json::to_string(&Envelope {
            kind: "encrypted".into(),
            seq,
            data: crypto::encode(&encrypted),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn valid_bootstrap_encryption_never_authorizes_operations_before_approval() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut service = Service::new(
            Err(io::Error::other("test")),
            Arc::new(NoOperations(calls.clone())),
        );
        for phase in [
            Phase::AwaitPair,
            Phase::Pending {
                request_id: "pending".into(),
                name: "phone".into(),
            },
        ] {
            let mut peer = Peer::new(phase);
            peer.cipher = Some(Cipher::host(&[1; 32], &[2; 32], &[2; 32]));
            let result = service
                .peer_frame(
                    "peer",
                    &mut peer,
                    &client_payload(json!({
                        "type":"request", "id":1, "op":"list"
                    })),
                    &mut Vec::new(),
                )
                .await;
            assert!(result.is_err());
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn public_plaintext_and_malformed_pins_are_refused() {
        assert!(validate_endpoint("ws://relay.example.invalid:8443/v1/ws", None).is_err());
        assert!(validate_endpoint("ws://127.0.0.1.evil.test/v1/ws", None).is_err());
        assert!(validate_endpoint("ws://127.0.0.1:8443/v1/ws", None).is_ok());
        assert!(
            validate_endpoint("wss://relay.example.invalid:8443/v1/ws", Some("not-a-pin")).is_err()
        );
        let test_pin = "11".repeat(32);
        assert!(
            validate_endpoint(
                "wss://user@relay.example.invalid:8443/v1/ws",
                Some(&test_pin)
            )
            .is_err()
        );
    }
    #[test]
    fn status_debug_never_exposes_qr_secret() {
        let pairing = Pairing {
            uri: "tty7://pair?data=secret".into(),
            expires_at: 1,
        };
        assert!(!format!("{pairing:?}").contains("data=secret"));
    }
}
