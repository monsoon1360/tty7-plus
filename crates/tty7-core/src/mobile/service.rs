use super::crypto::{self, Cipher, Envelope};
use super::store::{Credential, Store};
use super::{AdminReply, AdminRequest, Device, Pairing, Pending, Status};
use crate::daemon::pane::{DaemonPane, MobileAgentState};
use crate::daemon::protocol::WinSize;
use crate::daemon::screen::{HistorySnapshot, ScreenSnapshot};
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
const MOBILE_PROTOCOL: u64 = 3;
const OUTPUT_WINDOW: u64 = 4;
const MAX_SCREEN_FRAME_BYTES: usize = 128 * 1024;
const MAX_SCREEN_STATE_BYTES: usize = 8 * 1024 * 1024;
const MAX_HISTORY_ROWS: usize = 5000;
const MAX_HISTORY_BYTES: usize = 8 * 1024 * 1024;
const MAX_HISTORY_PAGE_ROWS: usize = 200;
const MAX_HISTORY_PAGE_BYTES: usize = 128 * 1024;
const MAX_HISTORY_VIEWS: usize = 16;
const MAX_HISTORY_TOTAL_BYTES: usize = 32 * 1024 * 1024;
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
    pub receiver: mpsc::Receiver<()>,
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
    resync: Arc<AtomicBool>,
    history: Arc<Mutex<HistoryStore>>,
    history_busy: Arc<AtomicBool>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.credit.cancel();
        self.history.lock().remove(&self.id);
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
struct HistoryView {
    subscription: String,
    snapshot: Arc<HistorySnapshot>,
    sizes: Arc<Vec<usize>>,
    end: usize,
    bytes: usize,
    created: Instant,
}

#[derive(Default)]
struct HistoryStore {
    views: HashMap<String, HistoryView>,
}

impl HistoryStore {
    fn remove(&mut self, subscription: &str) {
        self.views.retain(|_, view| view.subscription != subscription);
    }

    fn take(&mut self, subscription: &str, cursor: &str) -> io::Result<HistoryView> {
        if !self.views.get(cursor).is_some_and(|view| view.subscription == subscription) {
            return Err(io::Error::other("history cursor expired or does not belong to this subscription"));
        }
        Ok(self.views.remove(cursor).expect("checked history cursor"))
    }

    fn retain_view(&mut self, view: HistoryView) -> String {
        self.remove(&view.subscription);
        while self.views.len() >= MAX_HISTORY_VIEWS
            || self.views.values().map(|old| old.bytes).sum::<usize>() + view.bytes > MAX_HISTORY_TOTAL_BYTES
        {
            let oldest = self.views.iter().min_by_key(|(_, old)| old.created)
                .map(|(key, _)| key.clone()).expect("bounded history view");
            self.views.remove(&oldest);
        }
        let cursor = uuid::Uuid::new_v4().to_string();
        self.views.insert(cursor.clone(), view);
        cursor
    }
}

fn history_view(subscription: String, mut snapshot: HistorySnapshot) -> HistoryView {
    let mut sizes = Vec::new();
    let mut bytes = 0;
    for line in snapshot.lines.iter().rev().take(MAX_HISTORY_ROWS) {
        let size = json_string_bytes(&line.text) + json_string_bytes(&line.ansi) + 48;
        if size > MAX_HISTORY_PAGE_BYTES - 1024 || bytes + size > MAX_HISTORY_BYTES {
            break;
        }
        bytes += size;
        sizes.push(size);
    }
    let end = sizes.len();
    let discard = snapshot.lines.len() - end;
    snapshot.truncated |= discard > 0;
    snapshot.lines.drain(..discard);
    sizes.reverse();
    HistoryView {
        subscription, snapshot: Arc::new(snapshot), sizes: Arc::new(sizes),
        end, bytes, created: Instant::now(),
    }
}

fn history_page(mut view: HistoryView, store: &Mutex<HistoryStore>, credit: &OutputCredit) -> io::Result<Value> {
    let end = view.end;
    let mut start = end;
    let mut bytes = 1024;
    while start > 0 && end - start < MAX_HISTORY_PAGE_ROWS {
        let next = view.sizes[start - 1];
        if bytes + next > MAX_HISTORY_PAGE_BYTES { break; }
        bytes += next;
        start -= 1;
    }
    let lines = serde_json::to_value(&view.snapshot.lines[start..end]).map_err(io::Error::other)?;
    let truncated = view.snapshot.truncated;
    view.end = start;
    let mut store = store.lock();
    if !credit.active() {
        return Err(io::Error::other("terminal subscription is no longer active"));
    }
    let cursor = if start > 0 { Some(store.retain_view(view)) } else { None };
    Ok(json!({"lines":lines, "cursor":cursor, "has_more":start > 0, "truncated":truncated}))
}

fn active_subscription<'a>(peer: &'a Peer, request: &Value) -> io::Result<&'a Subscription> {
    let pane = number(request, "pane_id")?;
    let id = field(request, "subscription_id")?;
    let workspace = field(request, "workspace_id")?;
    peer.subscriptions.get(&pane).filter(|subscription| {
        subscription.id == id && subscription.workspace == workspace && subscription.credit.active()
    }).ok_or_else(|| io::Error::other("terminal subscription is no longer active"))
}

struct HistoryJob(Arc<AtomicBool>);
impl Drop for HistoryJob {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
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
    history: Arc<Mutex<HistoryStore>>,
    history_slots: Arc<tokio::sync::Semaphore>,
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
            history: Arc::new(Mutex::new(HistoryStore::default())),
            history_slots: Arc::new(tokio::sync::Semaphore::new(2)),
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
            if field(&hello, "kind")? != "hello" {
                return Err(crypto::denied());
            }
            if hello.get("protocol").and_then(Value::as_u64) != Some(MOBILE_PROTOCOL) {
                outgoing.push(data(client, json!({"kind":"error", "code":"update_required",
                    "protocol":MOBILE_PROTOCOL,
                    "message":"Mobile protocol mismatch; update the app and host to protocol 3."}).to_string()));
                return Err(io::Error::other("mobile protocol mismatch: update required"));
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
                if field(&request, "op")? == "history" {
                    match self.schedule_history(client, peer, &request, id) {
                        Ok(()) => return Ok(()),
                        Err(error) => {
                            outgoing.push(encrypted(client, peer, &response(id, Err(error)))?);
                            return Ok(());
                        }
                    }
                }
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
    fn schedule_history(&self, client: &str, peer: &Peer, request: &Value, id: u64) -> io::Result<()> {
        let subscription = active_subscription(peer, request)?;
        let cursor = match request.get("cursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(cursor)) if cursor.len() <= 128 => Some(cursor.clone()),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid history cursor")),
        };
        let pane = subscription.observer.as_ref()
            .ok_or_else(|| io::Error::other("terminal observer is unavailable"))?.0.clone();
        let permit = self.history_slots.clone().try_acquire_owned()
            .map_err(|_| io::Error::other("mobile history workers are busy"))?;
        if subscription.history_busy.swap(true, Ordering::AcqRel) {
            return Err(io::Error::other("a history request is already pending"));
        }
        let job = HistoryJob(subscription.history_busy.clone());
        let subscription = subscription.id.clone();
        let credit = active_subscription(peer, request)?.credit.clone();
        let store = self.history.clone();
        let replies = self.replies.clone();
        let alive = peer.alive.clone();
        let failed = peer.failed.clone();
        let client = client.to_string();
        std::thread::Builder::new().name("tty7-mobile-history".into()).spawn(move || {
            let (_permit, _job) = (permit, job);
            let result = (|| {
                if !credit.active() { return Err(io::Error::other("terminal subscription is no longer active")); }
                let view = match cursor {
                    Some(cursor) => store.lock().take(&subscription, &cursor)?,
                    None => {
                        store.lock().remove(&subscription);
                        history_view(subscription, pane.mobile_history()?)
                    }
                };
                history_page(view, &store, &credit)
            })();
            if alive.load(Ordering::Acquire) && credit.active()
                && replies.try_send((client, response(id, result))).is_err()
            {
                failed.store(true, Ordering::Release);
            }
        })?;
        Ok(())
    }

    async fn operation(&self, client: &str, peer: &mut Peer, request: Value) -> io::Result<Value> {
        match field(&request, "op")? {
            "subscribe" => {
                let workspace = field(&request, "workspace_id")?.to_string();
                let pane = number(&request, "pane_id")?;
                let target = if request.get("cols").is_some() || request.get("rows").is_some() {
                    Some(terminal_size(&request)?)
                } else {
                    None
                };
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
                peer.subscriptions.remove(&pane);
                if let Some(size) = target {
                    observation.pane.mobile_observer_resize(observation.observer_id, size, true)?;
                }
                let size = observation.pane.mobile_size();
                let id = uuid::Uuid::new_v4().to_string();
                let credit = Arc::new(OutputCredit::new());
                let resync = Arc::new(AtomicBool::new(true));
                let subscription = Subscription {
                    id: id.clone(),
                    workspace,
                    observer: Some((observation.pane.clone(), observation.observer_id)),
                    lease_started: target.is_some(),
                    credit: credit.clone(),
                    resync: resync.clone(),
                    history: self.history.clone(),
                    history_busy: Arc::new(AtomicBool::new(false)),
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
                        forward_output(observation, sink, resync);
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
            "resync" => {
                let subscription = active_subscription(peer, &request)?;
                subscription.resync.store(true, Ordering::Release);
                subscription.credit.changed.notify_all();
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
    fn agent(&self, state: MobileAgentState) -> bool {
        self.send(
            json!({"type":"agent", "pane_id":self.pane,
            "subscription_id":self.id, "agent_revision":state.revision,
            "agent":state.agent.map(|(name, status)| json!({
                "name":name.display_name(), "status":status}))}),
            None,
            false,
        )
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
    fn wait_credit(&self, status: &mut impl FnMut() -> bool) -> bool {
        loop {
            if !self.credit.active() || !status() { return false; }
            let mut state = self.credit.state.lock();
            if !state.active { return false; }
            if state.reserved - state.acknowledged < OUTPUT_WINDOW { return true; }
            self.credit.changed.wait_for(&mut state, Duration::from_millis(200));
        }
    }

    fn screen(&self, screen: &ScreenSnapshot, previous: Option<&ScreenSnapshot>,
        status: &mut impl FnMut() -> bool) -> bool
    {
        let parts = match screen_parts(screen, previous) {
            Ok(parts) => parts,
            Err(error) => { self.error(&error.to_string()); return false; }
        };
        let base = previous.filter(|old| same_screen_epoch(old, screen));
        for (part, rows) in parts.iter().enumerate() {
            if !self.wait_credit(status) { return false; }
            let Some(sequence) = self.sequence() else { return false; };
            let lines: Vec<_> = rows.iter().map(|&row| json!({
                "row":row, "ansi":screen.lines[row].as_str()
            })).collect();
            if !self.send(json!({
                "type":"screen", "pane_id":self.pane, "subscription_id":self.id,
                "sequence":sequence, "revision":screen.revision.to_string(),
                "base_revision":base.map(|old| old.revision.to_string()),
                "epoch":screen.epoch.to_string(), "cols":screen.cols, "rows":screen.rows,
                "alternate":screen.alternate, "cursor":screen.cursor, "modes":screen.modes,
                "lines":lines, "part":part, "last":part + 1 == parts.len()
            }), Some(sequence), false) { return false; }
        }
        true
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

fn same_screen_epoch(a: &ScreenSnapshot, b: &ScreenSnapshot) -> bool {
    a.epoch == b.epoch && a.cols == b.cols && a.rows == b.rows && a.alternate == b.alternate
}


// JSON escapes are charged without allocating a serialized copy of every row.
fn json_string_bytes(text: &str) -> usize {
    2 + text.bytes().map(|byte| match byte {
        b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
        0..=31 => 6,
        _ => 1,
    }).sum::<usize>()
}

fn screen_parts(screen: &ScreenSnapshot, previous: Option<&ScreenSnapshot>) -> io::Result<Vec<Vec<usize>>> {
    if screen.lines.len() != usize::from(screen.rows) {
        return Err(io::Error::other("screen snapshot has an invalid row count"));
    }
    let previous = previous.filter(|old| same_screen_epoch(old, screen));
    let mut parts = vec![Vec::new()];
    let mut part_bytes = 2048;
    let mut state_bytes = 2048;
    for (row, line) in screen.lines.iter().enumerate() {
        // Includes the row number and line object punctuation.
        let bytes = json_string_bytes(line) + 96;
        state_bytes += bytes;
        if state_bytes > MAX_SCREEN_STATE_BYTES || bytes > MAX_SCREEN_FRAME_BYTES - 2048 {
            return Err(io::Error::other("screen state exceeds mobile staging limits"));
        }
        if previous.is_some_and(|old| old.lines.get(row) == Some(line)) { continue; }
        if part_bytes + bytes > MAX_SCREEN_FRAME_BYTES {
            parts.push(Vec::new());
            part_bytes = 2048;
        }
        parts.last_mut().expect("screen part").push(row);
        part_bytes += bytes;
    }
    Ok(parts)
}

fn forward_output(observation: Observation, sink: MobileOutput, resync: Arc<AtomicBool>) {
    let mut last_agent = None;
    let mut last_lease = None;
    let status = || {
        let state = observation.pane.mobile_agent();
        if last_agent != Some(state) {
            last_agent = Some(state);
            if !sink.agent(state) { return false; }
        }
        let held = observation.pane.mobile_lease_held(observation.observer_id);
        if last_lease != Some(held) {
            last_lease = Some(held);
            if !sink.send(json!({"type":"terminal_lease", "pane_id":sink.pane,
                "subscription_id":sink.id, "held":held}), None, false) { return false; }
        }
        if let Some(code) = observation.pane.mobile_exit() {
            sink.send(json!({"type":"exited", "pane_id":sink.pane,
                "subscription_id":sink.id, "code":code}), None, true);
            return false;
        }
        true
    };
    forward_screens(&observation.receiver, &sink, &resync,
        || observation.pane.mobile_screen(), status);
}

fn forward_screens(
    receiver: &mpsc::Receiver<()>, sink: &MobileOutput, resync: &AtomicBool,
    mut snapshot: impl FnMut() -> io::Result<Arc<ScreenSnapshot>>,
    mut status: impl FnMut() -> bool,
) {
    let mut previous: Option<Arc<ScreenSnapshot>> = None;
    while sink.credit.active() {
        // Wait BEFORE sampling: arbitrarily many producer updates collapse into
        // the latest immutable state, never a queue of historical PTY output.
        if !sink.wait_credit(&mut status) { return; }
        let full = resync.swap(false, Ordering::AcqRel);
        let screen = match snapshot() {
            Ok(screen) => screen,
            Err(error) => { sink.error(&error.to_string()); return; }
        };
        if full || previous.as_ref().is_none_or(|old| old.revision != screen.revision || !same_screen_epoch(old, &screen)) {
            let base = if full { None } else { previous.as_deref() };
            if !sink.screen(&screen, base, &mut status) { return; }
            previous = Some(screen);
        }
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if status() { sink.error("terminal observer disconnected unexpectedly"); }
                return;
            }
        }
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
                    resync: Arc::new(AtomicBool::new(false)),
                    history: service.history.clone(),
                    history_busy: Arc::new(AtomicBool::new(false)),
                },
            );
        credit
    }

    fn sink(output: async_mpsc::Sender<Output>, credit: Arc<OutputCredit>) -> MobileOutput {
        MobileOutput {
            output, credit, runtime: tokio::runtime::Handle::current(),
            client: "phone".into(), pane: 7, id: "feed".into(),
        }
    }

    fn screen_fixture(rows: u16, width: usize) -> ScreenSnapshot {
        use crate::daemon::screen::{ScreenCursor, ScreenModes};
        ScreenSnapshot {
            revision: 1, epoch: 1, cols: 80, rows, alternate: false,
            cursor: ScreenCursor { row: 0, col: 0, shape: "block".into(), visible: true, blink: true },
            modes: ScreenModes { app_cursor: false, app_keypad: false, bracketed_paste: true,
                mouse: false, sgr_mouse: false, alternate_scroll: true },
            lines: (0..rows).map(|row| Arc::new(format!("\x1b[0;31m{row}界e\u{301}{}\x1b[0m", "x".repeat(width)))).collect(),
        }
    }

    fn history_fixture(rows: usize) -> HistorySnapshot {
        use crate::daemon::screen::HistoryLine;
        HistorySnapshot {
            lines: (0..rows).map(|row| HistoryLine {
                text: format!("row {row}"), ansi: format!("\x1b[0mrow {row}"), cols: 80,
            }).collect(),
            truncated: false,
        }
    }

    async fn receive_output(service: &mut Service) -> Output {
        tokio::time::timeout(Duration::from_secs(1), service.outputs.recv())
            .await
            .unwrap()
            .unwrap()
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
    async fn credit_wait_coalesces_current_state_and_resync_keeps_sequence_order() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let current = Arc::new(Mutex::new(Arc::new(screen_fixture(3, 10))));
        let resync = Arc::new(AtomicBool::new(true));
        let (wake, receive) = mpsc::sync_channel(1);
        let output = sink(service.output.clone(), credit.clone());
        let live = current.clone();
        let reset = resync.clone();
        let status_reads = Arc::new(AtomicUsize::new(0));
        let reads = status_reads.clone();
        let (done, completed) = tokio::sync::oneshot::channel();
        let producer = std::thread::spawn(move || {
            forward_screens(&receive, &output, &reset, || Ok(live.lock().clone()), || {
                reads.fetch_add(1, Ordering::Relaxed);
                true
            });
            let _ = done.send(());
        });
        for revision in 1..=4 {
            if revision > 1 {
                let mut screen = screen_fixture(3, 10);
                screen.revision = revision;
                screen.lines[1] = Arc::new(format!("\x1b[0mrevision {revision}"));
                *current.lock() = Arc::new(screen);
                let _ = wake.try_send(());
            }
            let frame = receive_output(&mut service).await;
            assert_eq!(frame.value["revision"], revision.to_string());
            assert_eq!(frame.sequence, Some(revision));
            service.output_frame(frame, &mut Vec::new());
        }
        for revision in 5..=5000 {
            let mut screen = screen_fixture(3, 10);
            screen.revision = revision;
            screen.lines[1] = Arc::new(format!("\x1b[0mrevision {revision}"));
            *current.lock() = Arc::new(screen);
            let _ = wake.try_send(());
        }
        let before = status_reads.load(Ordering::Relaxed);
        assert!(tokio::time::timeout(Duration::from_millis(250), service.outputs.recv()).await.is_err());
        assert!(status_reads.load(Ordering::Relaxed) > before, "status remains sampled without screen credit");
        assert_eq!(credit.state.lock().reserved, 4);
        acknowledge(&mut service, 0, "feed", 4).await;
        let frame = receive_output(&mut service).await;
        assert_eq!(frame.sequence, Some(5));
        assert_eq!(frame.value["revision"], "5000");
        assert_eq!(frame.value["base_revision"], "4");
        assert_eq!(frame.value["lines"].as_array().unwrap().len(), 1);
        service.output_frame(frame, &mut Vec::new());
        resync.store(true, Ordering::Release);
        let _ = wake.try_send(());
        let frame = receive_output(&mut service).await;
        assert_eq!(frame.sequence, Some(6));
        assert_eq!(frame.value["revision"], "5000");
        assert!(frame.value["base_revision"].is_null());
        assert_eq!(frame.value["lines"].as_array().unwrap().len(), 3);
        service.peers.remove("phone");
        tokio::time::timeout(Duration::from_secs(1), completed).await.unwrap().unwrap();
        producer.join().unwrap();
    }

    #[tokio::test]
    async fn metadata_only_patch_transmits_cursor_modes_and_revision_without_rows() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let output = sink(service.output.clone(), credit);
        let producer = std::thread::spawn(move || {
            let before = screen_fixture(3, 10);
            let mut after = screen_fixture(3, 10);
            after.revision = u64::MAX - 1;
            after.cursor.col = 7;
            after.cursor.visible = false;
            after.modes.app_cursor = true;
            after.modes.app_keypad = true;
            output.screen(&after, Some(&before), &mut || true)
        });
        let frame = receive_output(&mut service).await;
        assert!(producer.join().unwrap());
        assert_eq!(frame.value["revision"], (u64::MAX - 1).to_string());
        assert_eq!(frame.value["base_revision"], "1");
        assert_eq!(frame.value["cursor"]["col"], 7);
        assert_eq!(frame.value["cursor"]["visible"], false);
        assert_eq!(frame.value["modes"]["app_cursor"], true);
        assert_eq!(frame.value["modes"]["app_keypad"], true);
        assert_eq!(frame.value["lines"], json!([]));
        assert_eq!(frame.value["last"], true);
    }

    #[tokio::test]
    async fn observer_failure_is_bounded_and_only_removes_its_own_subscription() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        add_subscription(&mut service, "phone", 8, "other");
        let output = sink(service.output.clone(), credit);
        let producer = std::thread::spawn(move || output.error(&"界".repeat(crypto::MAX_PLAINTEXT)));
        let frame = receive_output(&mut service).await;
        producer.join().unwrap();
        assert_eq!(frame.value["type"], "terminal_error");
        assert!(frame.value["message"].as_str().unwrap().len() <= 4096);
        service.output_frame(frame, &mut Vec::new());
        assert!(!service.peers["phone"].subscriptions.contains_key(&7));
        assert!(service.peers["phone"].subscriptions.contains_key(&8));
    }

    #[test]
    fn screen_limits_charge_json_escaping_and_reject_unbounded_staging() {
        let mut screen = screen_fixture(100, 100 * 1024);
        assert!(screen_parts(&screen, None).is_err());
        screen.rows = 1;
        screen.lines.truncate(1);
        screen.lines[0] = Arc::new("\x1b".repeat(24 * 1024));
        assert!(screen_parts(&screen, None).is_err());
        for text in ["界e\u{301}", "\x1b[0m\"\\\n\r\t\u{08}\u{0c}"] {
            assert_eq!(json_string_bytes(text), serde_json::to_vec(text).unwrap().len());
        }
    }

    #[test]
    fn canonical_mirror_patches_reconstruct_split_ansi_utf8_clear_alt_and_resize_without_history() {
        use crate::daemon::screen::ScreenMirror;
        fn apply(previous: &mut Option<Arc<ScreenSnapshot>>, rendered: &mut Vec<Arc<String>>, next: Arc<ScreenSnapshot>) {
            if previous.as_ref().is_none_or(|old| !same_screen_epoch(old, &next)) {
                *rendered = vec![Arc::new(String::new()); usize::from(next.rows)];
            }
            for row in screen_parts(&next, previous.as_deref()).unwrap().into_iter().flatten() {
                rendered[row] = next.lines[row].clone();
            }
            assert_eq!(*rendered, next.lines);
            *previous = Some(next);
        }
        let mut mirror = ScreenMirror::new(size(20, 5), false);
        for _ in 0..300 { mirror.feed(b"historical-only\r\n"); }
        mirror.feed(b"\x1b[2J\x1b[Hcurrent");
        let first = mirror.snapshot();
        assert!(mirror.history().lines.len() > 200);
        assert_eq!(first.lines.len(), 5);
        assert!(!first.lines.iter().any(|line| line.contains("historical-only")));
        let mut previous = None;
        let mut rendered = Vec::new();
        apply(&mut previous, &mut rendered, first);
        for byte in "\x1b[2;3H\x1b[38;2;255;16;32m界e\u{301}\x1b[0m\x1b[?1h\x1b[?2004h\x1b[?25l".as_bytes() {
            mirror.feed(std::slice::from_ref(byte));
            apply(&mut previous, &mut rendered, mirror.snapshot());
        }
        for bytes in [
            b"\x1b[2J\x1b[Hclear".as_slice(),
            b"\x1b[?1049h\x1b[Halternate".as_slice(),
            b"\x1b[?1049l\x1b[3;1Hnormal".as_slice(),
        ] {
            mirror.feed(bytes);
            apply(&mut previous, &mut rendered, mirror.snapshot());
        }
        mirror.resize(size(30, 8));
        apply(&mut previous, &mut rendered, mirror.snapshot());
        assert_eq!(rendered.len(), 8);
    }

    #[test]
    fn state_diff_preserves_cells_and_full_replaces_geometry_and_buffer_changes() {
        let before = screen_fixture(4, 20);
        let mut after = screen_fixture(4, 20);
        after.revision = 2;
        after.lines[2] = Arc::new("\x1b[0mcleared\x1b[0m".into());
        after.cursor.col = 4;
        after.modes.app_cursor = true;
        assert_eq!(screen_parts(&after, Some(&before)).unwrap(), vec![vec![2]]);
        let mut reconstructed = before.lines.clone();
        for row in screen_parts(&after, Some(&before)).unwrap().into_iter().flatten() {
            reconstructed[row] = after.lines[row].clone();
        }
        assert_eq!(reconstructed, after.lines);
        for (epoch, alternate, cols) in [(2, false, 80), (1, true, 80), (1, false, 100)] {
            after.epoch = epoch;
            after.alternate = alternate;
            after.cols = cols;
            assert_eq!(screen_parts(&after, Some(&before)).unwrap(), vec![vec![0, 1, 2, 3]]);
        }
        after = screen_fixture(4, 20);
        after.revision = 3;
        after.cursor.visible = false;
        assert_eq!(screen_parts(&after, Some(&before)).unwrap(), vec![Vec::<usize>::new()]);
    }

    #[tokio::test]
    async fn multipart_screen_obeys_credit_and_preserves_atomic_state_metadata() {
        let mut service = test_service();
        let credit = add_subscription(&mut service, "phone", 7, "feed");
        let screen = screen_fixture(14, 64 * 1024);
        let expected: Vec<_> = screen.lines.iter().map(|line| line.as_str().to_owned()).collect();
        let output = sink(service.output.clone(), credit.clone());
        let producer = std::thread::spawn(move || output.screen(&screen, None, &mut || true));
        let mut staged = Vec::new();
        let mut envelope = 0;
        for part in 0..14 {
            if part == 4 {
                assert!(tokio::time::timeout(Duration::from_millis(30), service.outputs.recv()).await.is_err());
                assert!(client_frame(&mut service, envelope, json!({"type":"request","id":19,"op":"list"})).await.is_empty());
                envelope += 1;
                let (_, reply) = service.reply_receiver.recv().await.unwrap();
                assert_eq!(reply["ok"], true);
                acknowledge(&mut service, envelope, "feed", 4).await;
                envelope += 1;
            }
            let frame = receive_output(&mut service).await;
            assert_eq!(frame.value["type"], "screen");
            assert_eq!(frame.value["part"], part);
            assert_eq!(frame.value["last"], part == 13);
            assert_eq!(frame.value["revision"], "1");
            assert_eq!(frame.value["epoch"], "1");
            assert!(frame.value["base_revision"].is_null());
            assert_eq!(frame.value["modes"]["bracketed_paste"], true);
            assert!(serde_json::to_vec(&frame.value).unwrap().len() < MAX_SCREEN_FRAME_BYTES);
            for line in frame.value["lines"].as_array().unwrap() {
                assert_eq!(line["row"], staged.len());
                staged.push(line["ansi"].as_str().unwrap().to_string());
            }
            service.output_frame(frame, &mut Vec::new());
            if part >= 4 {
                acknowledge(&mut service, envelope, "feed", part + 1).await;
                envelope += 1;
            }
        }
        assert_eq!(staged, expected);
        assert!(producer.join().unwrap());
    }

    #[tokio::test]
    async fn stale_ack_and_queued_events_cannot_touch_a_reopened_same_pane() {
        let mut service = test_service();
        let old = add_subscription(&mut service, "phone", 7, "old");
        let sequence = old.reserve().unwrap();
        let current = add_subscription(&mut service, "phone", 7, "feed");
        assert!(!old.active());
        current.reserve().unwrap();
        current.sent(1).unwrap();
        let mut outgoing = Vec::new();
        service.output_frame(Output {
            client:"phone".into(), pane:7, subscription:"old".into(),
            value:json!({"type":"screen","sequence":sequence}), sequence:Some(sequence), finished:true,
        }, &mut outgoing);
        assert!(outgoing.is_empty());
        acknowledge(&mut service, 0, "old", u64::MAX).await;
        assert_eq!(current.state.lock().acknowledged, 0);
        acknowledge(&mut service, 1, "feed", 1).await;
        assert_eq!(current.state.lock().acknowledged, 1);
        assert_eq!(service.peers["phone"].subscriptions[&7].id, "feed");
    }

    #[tokio::test]
    async fn cancelled_workers_stop_while_waiting_for_credit_or_output_queue() {
        for capacity in [1, 128] {
            let (output, _receive) = async_mpsc::channel(capacity);
            let credit = Arc::new(OutputCredit::new());
            let output = sink(output, credit.clone());
            let (done, completed) = tokio::sync::oneshot::channel();
            let producer = std::thread::spawn(move || {
                output.screen(&screen_fixture(10, 64 * 1024), None, &mut || true);
                let _ = done.send(());
            });
            tokio::time::timeout(Duration::from_secs(1), async {
                while credit.state.lock().reserved < if capacity == 1 { 2 } else { 4 } {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }).await.unwrap();
            credit.cancel();
            tokio::time::timeout(Duration::from_secs(1), completed).await.unwrap().unwrap();
            producer.join().unwrap();
        }
    }

    #[test]
    fn history_is_immutable_bounded_chronological_and_subscription_scoped() {
        let store = Mutex::new(HistoryStore::default());
        let credit = OutputCredit::new();
        let view = history_view("feed".into(), history_fixture(6000));
        assert_eq!(view.end, MAX_HISTORY_ROWS);
        assert!(view.snapshot.truncated);
        let first = history_page(view, &store, &credit).unwrap();
        assert_eq!(first["lines"][0]["text"], "row 5800");
        assert_eq!(first["lines"][199]["text"], "row 5999");
        assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_HISTORY_PAGE_BYTES);
        let cursor = first["cursor"].as_str().unwrap();
        assert!(store.lock().take("other-subscription", cursor).is_err());
        let independent_live_history = history_fixture(7000);
        assert_eq!(independent_live_history.lines.len(), 7000);
        let view = store.lock().take("feed", cursor).unwrap();
        let second = history_page(view, &store, &credit).unwrap();
        assert_eq!(second["lines"][0]["text"], "row 5600");
        assert_eq!(second["lines"][199]["text"], "row 5799");
        assert!(store.lock().take("feed", cursor).is_err());
        store.lock().remove("feed");
        assert!(store.lock().take("feed", second["cursor"].as_str().unwrap()).is_err());
        assert_eq!(credit.state.lock().reserved, 0);
    }

    #[test]
    fn history_limits_global_views_bytes_and_cancelled_insertion() {
        let store = Mutex::new(HistoryStore::default());
        let credit = OutputCredit::new();
        for index in 0..MAX_HISTORY_VIEWS + 2 {
            history_page(history_view(index.to_string(), history_fixture(201)), &store, &credit).unwrap();
        }
        assert_eq!(store.lock().views.len(), MAX_HISTORY_VIEWS);
        for index in 0..6 {
            let mut oversized = history_fixture(5000);
            for line in &mut oversized.lines { line.ansi = "界".repeat(1000); }
            let view = history_view(format!("large-{index}"), oversized);
            assert!(view.bytes <= MAX_HISTORY_BYTES);
            assert!(view.snapshot.truncated);
            history_page(view, &store, &credit).unwrap();
        }
        assert!(store.lock().views.values().map(|view| view.bytes).sum::<usize>() <= MAX_HISTORY_TOTAL_BYTES);
        assert!(store.lock().views.len() <= 4);
        credit.cancel();
        assert!(history_page(history_view("cancelled".into(), history_fixture(300)), &store, &credit).is_err());
        assert!(!store.lock().views.values().any(|view| view.subscription == "cancelled"));
    }

    #[test]
    fn reading_an_active_history_view_can_pause_without_expiring_its_next_page() {
        let store = Mutex::new(HistoryStore::default());
        let credit = OutputCredit::new();
        let first = history_page(history_view("reader".into(), history_fixture(600)), &store, &credit).unwrap();
        let cursor = first["cursor"].as_str().unwrap();
        store.lock().views.get_mut(cursor).unwrap().created = Instant::now() - Duration::from_secs(3600);
        let view = store.lock().take("reader", cursor).unwrap();
        let second = history_page(view, &store, &credit).unwrap();
        assert_eq!(second["lines"][0]["text"], "row 200");
        assert_eq!(second["lines"][199]["text"], "row 399");
        assert!(second["has_more"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn history_and_resync_reject_wrong_pane_workspace_and_generation() {
        let mut service = test_service();
        add_subscription(&mut service, "phone", 7, "feed");
        let mut peer = service.peers.remove("phone").unwrap();
        for (pane, id, workspace) in [(8,"feed","workspace"),(7,"old","workspace"),(7,"feed","other")] {
            let request = json!({"op":"history","pane_id":pane,"subscription_id":id,"workspace_id":workspace});
            assert_eq!(service.schedule_history("phone", &peer, &request, 1).unwrap_err().to_string(),
                "terminal subscription is no longer active");
            let mut request = request;
            request["op"] = json!("resync");
            assert!(service.operation("phone", &mut peer, request).await.is_err());
        }
        assert!(!peer.subscriptions[&7].resync.load(Ordering::Acquire));
        service.operation("phone", &mut peer, json!({
            "op":"resync","pane_id":7,"subscription_id":"feed","workspace_id":"workspace"
        })).await.unwrap();
        assert!(peer.subscriptions[&7].resync.load(Ordering::Acquire));
        assert_eq!(peer.subscriptions[&7].credit.state.lock().reserved, 0);
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
        for protocol in [Value::Null, json!(0), json!(1), json!(2), json!("3"), json!(3.0)] {
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
            let Message::Text(text) = &outgoing[0] else { panic!("expected protocol error"); };
            let envelope: Value = serde_json::from_str(text).unwrap();
            let error: Value = serde_json::from_str(envelope["payload"].as_str().unwrap()).unwrap();
            assert_eq!(error["code"], "update_required");
            assert_eq!(service.bootstrap.as_ref().unwrap().attempts, 0);
        }
        let mut peer = Peer::new(Phase::Hello);
        let mut outgoing = Vec::new();
        service
            .peer_frame(
                "phone",
                &mut peer,
                &json!({"kind":"hello", "protocol":3,
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
        assert_eq!(challenge["protocol"], 3);
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
