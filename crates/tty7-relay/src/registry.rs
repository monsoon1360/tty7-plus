use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::extract::ws::{CloseFrame, Message, close_code};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    config::{
        GLOBAL_QUEUE_BYTES, MAX_CLIENTS_PER_HOST, MAX_HOSTS, MAX_MESSAGE_BYTES, PEER_QUEUE_BYTES,
        QUEUE_MESSAGES,
    },
    protocol::{Frame, RegisteredRole, ValidRegistration, encode},
};

struct DataBudget {
    _message: OwnedSemaphorePermit,
    _peer_bytes: OwnedSemaphorePermit,
    _global_bytes: OwnedSemaphorePermit,
}

pub(crate) struct QueuedMessage {
    pub(crate) message: Message,
    _data_budget: Option<DataBudget>,
    // Returned only after peer_left is flushed, not when the client leaves.
    _lifecycle: Option<OwnedSemaphorePermit>,
}

impl QueuedMessage {
    fn control(text: String, lifecycle: Option<OwnedSemaphorePermit>) -> Self {
        Self {
            message: Message::Text(text.into()),
            _data_budget: None,
            _lifecycle: lifecycle,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Peer {
    outbound: mpsc::Sender<QueuedMessage>,
    pub(crate) cancel: CancellationToken,
    closing: CancellationToken,
    messages: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    global_bytes: Arc<Semaphore>,
}

impl Peer {
    // Queue pressure never cancels the destination. The routing owner decides
    // which source/slow client is responsible, preserving unrelated sessions.
    pub(crate) fn send(&self, text: String) -> bool {
        let len = text.len();
        if self.cancel.is_cancelled() || self.closing.is_cancelled() || len > MAX_MESSAGE_BYTES {
            return false;
        }
        let Ok(message_slot) = Arc::clone(&self.messages).try_acquire_owned() else {
            return false;
        };
        let Ok(peer_bytes) = Arc::clone(&self.bytes).try_acquire_many_owned(len as u32) else {
            return false;
        };
        let Ok(global_bytes) = Arc::clone(&self.global_bytes).try_acquire_many_owned(len as u32)
        else {
            return false;
        };
        self.enqueue(
            text,
            DataBudget {
                _message: message_slot,
                _peer_bytes: peer_bytes,
                _global_bytes: global_bytes,
            },
        )
    }

    async fn send_output(&self, text: String) -> bool {
        let len = text.len();
        if self.cancel.is_cancelled() || self.closing.is_cancelled() || len > MAX_MESSAGE_BYTES {
            return false;
        }
        // Replays exceed a single peer's budget. Wait for its writer to return
        // permits instead of treating a healthy terminal burst as a flood.
        // Client-originated requests still use the strict nonblocking limit.
        let budget = async {
            let message = Arc::clone(&self.messages).acquire_owned().await.ok()?;
            let peer_bytes = Arc::clone(&self.bytes)
                .acquire_many_owned(len as u32)
                .await
                .ok()?;
            let global_bytes = Arc::clone(&self.global_bytes)
                .acquire_many_owned(len as u32)
                .await
                .ok()?;
            Some(DataBudget {
                _message: message,
                _peer_bytes: peer_bytes,
                _global_bytes: global_bytes,
            })
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => false,
            _ = self.closing.cancelled() => false,
            result = tokio::time::timeout(Duration::from_secs(5), budget) => {
                match result {
                    Ok(Some(budget)) => self.enqueue(text, budget),
                    _ => false,
                }
            }
        }
    }

    fn enqueue(&self, text: String, budget: DataBudget) -> bool {
        if self.cancel.is_cancelled() || self.closing.is_cancelled() {
            return false;
        }
        self.outbound
            .try_send(QueuedMessage {
                message: Message::Text(text.into()),
                _data_budget: Some(budget),
                _lifecycle: None,
            })
            .is_ok()
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.is_cancelled()
    }

    fn ready(&self, client_id: Option<Uuid>) -> Result<(), &'static str> {
        self.outbound
            .try_send(QueuedMessage::control(
                encode(&Frame::Ready { client_id })?,
                None,
            ))
            .map_err(|_| "connection queue is unavailable")
    }

    fn close_after_flush(&self) {
        self.closing.cancel();
        // Clients have only ready, bounded data slots, and one close marker.
        // The larger FIFO's reserved host-lifecycle capacity cannot be
        // exhausted here; the marker follows every accepted data frame.
        let close = QueuedMessage {
            message: Message::Close(Some(CloseFrame {
                code: close_code::NORMAL,
                reason: "disconnected".into(),
            })),
            _data_budget: None,
            _lifecycle: None,
        };
        if self.outbound.try_send(close).is_err() {
            self.cancel.cancel();
        }
    }
}

#[derive(Clone)]
pub(crate) struct Owner {
    host_id: String,
    generation: Uuid,
    pub(crate) client_id: Option<Uuid>,
}

struct Client {
    peer: Peer,
    left: mpsc::OwnedPermit<QueuedMessage>,
    lifecycle: OwnedSemaphorePermit,
    left_frame: String,
}

struct Host {
    generation: Uuid,
    peer: Peer,
    clients: HashMap<Uuid, Client>,
    lifecycle: Arc<Semaphore>,
}

impl Host {
    fn remove_client(&mut self, client_id: Uuid, flush: bool) {
        if let Some(client) = self.clients.remove(&client_id) {
            if flush {
                client.peer.close_after_flush();
            } else {
                client.peer.cancel.cancel();
            }
            // Reserved at admission, so floods cannot displace peer_left.
            client.left.send(QueuedMessage::control(
                client.left_frame,
                Some(client.lifecycle),
            ));
        }
    }
}

#[derive(Default)]
struct Entries {
    hosts: HashMap<String, Host>,
}

pub(crate) struct Registry {
    entries: Mutex<Entries>,
    global_bytes: Arc<Semaphore>,
    pub(crate) shutdown: CancellationToken,
}

impl Registry {
    pub(crate) fn new(shutdown: CancellationToken) -> Self {
        Self {
            entries: Mutex::new(Entries::default()),
            global_bytes: Arc::new(Semaphore::new(GLOBAL_QUEUE_BYTES)),
            shutdown,
        }
    }

    fn entries(&self) -> MutexGuard<'_, Entries> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn peer(&self) -> (Peer, mpsc::Receiver<QueuedMessage>) {
        // Data uses bounded slots. Each lifecycle holds space for both
        // joined and left, plus one host ready frame. Left slots are reserved
        // at admission; lifecycle slots remain held through the left flush.
        let (outbound, receiver) = mpsc::channel(QUEUE_MESSAGES + 2 * MAX_CLIENTS_PER_HOST + 1);
        (
            Peer {
                outbound,
                cancel: self.shutdown.child_token(),
                closing: CancellationToken::new(),
                messages: Arc::new(Semaphore::new(QUEUE_MESSAGES)),
                bytes: Arc::new(Semaphore::new(PEER_QUEUE_BYTES)),
                global_bytes: Arc::clone(&self.global_bytes),
            },
            receiver,
        )
    }

    pub(crate) fn register(
        &self,
        registration: ValidRegistration,
        peer: Peer,
    ) -> Result<Owner, &'static str> {
        let mut entries = self.entries();
        match registration.role {
            RegisteredRole::Host => {
                if entries.hosts.contains_key(&registration.host_id) {
                    return Err("host is already connected");
                }
                if entries.hosts.len() >= MAX_HOSTS {
                    return Err("host capacity reached");
                }
                peer.ready(None)?;
                let generation = Uuid::new_v4();
                entries.hosts.insert(
                    registration.host_id.clone(),
                    Host {
                        generation,
                        peer,
                        clients: HashMap::new(),
                        lifecycle: Arc::new(Semaphore::new(MAX_CLIENTS_PER_HOST)),
                    },
                );
                Ok(Owner {
                    host_id: registration.host_id,
                    generation,
                    client_id: None,
                })
            }
            RegisteredRole::Client => {
                let host = entries
                    .hosts
                    .get_mut(&registration.host_id)
                    .ok_or("host is not connected")?;
                if host.peer.cancel.is_cancelled() {
                    return Err("host is not connected");
                }
                let lifecycle = Arc::clone(&host.lifecycle)
                    .try_acquire_owned()
                    .map_err(|_| "host peer lifecycle capacity reached")?;
                let client_id = loop {
                    let id = Uuid::new_v4();
                    if !host.clients.contains_key(&id) {
                        break id;
                    }
                };
                let joined_frame = encode(&Frame::PeerJoined { client_id })?;
                let left_frame = encode(&Frame::PeerLeft { client_id })?;
                let left = host
                    .peer
                    .outbound
                    .clone()
                    .try_reserve_owned()
                    .map_err(|_| "host lifecycle queue is unavailable")?;
                let joined = host
                    .peer
                    .outbound
                    .clone()
                    .try_reserve_owned()
                    .map_err(|_| "host lifecycle queue is unavailable")?;
                peer.ready(Some(client_id))?;
                joined.send(QueuedMessage::control(joined_frame, None));
                host.clients.insert(
                    client_id,
                    Client {
                        peer,
                        left,
                        lifecycle,
                        left_frame,
                    },
                );
                Ok(Owner {
                    host_id: registration.host_id,
                    generation: host.generation,
                    client_id: Some(client_id),
                })
            }
        }
    }

    pub(crate) async fn forward(
        &self,
        owner: &Owner,
        target: Option<Uuid>,
        payload: &str,
    ) -> Result<(), &'static str> {
        let (client_id, destination) = {
            let mut entries = self.entries();
            let host = entries
                .hosts
                .get_mut(&owner.host_id)
                .ok_or("host is not connected")?;
            if host.generation != owner.generation || host.peer.cancel.is_cancelled() {
                return Err("connection is no longer registered");
            }
            if let Some(client_id) = owner.client_id {
                let client = host
                    .clients
                    .get(&client_id)
                    .ok_or("connection is no longer registered")?;
                if client.peer.cancel.is_cancelled() || client.peer.closing.is_cancelled() {
                    return Err("connection is no longer registered");
                }
                if !host.peer.send(encode(&Frame::Data { client_id, payload })?) {
                    host.remove_client(client_id, false);
                    return Err("source exceeded host queue capacity");
                }
                return Ok(());
            }
            let client_id = target.ok_or("host data requires a client_id UUID")?;
            // In-flight output after peer_left cannot cross identities.
            let Some(client) = host.clients.get(&client_id) else {
                return Ok(());
            };
            (client_id, client.peer.clone())
        };
        // Never hold the registry lock while waiting for a slow phone.
        if !destination
            .send_output(encode(&Frame::Data { client_id, payload })?)
            .await
        {
            let mut entries = self.entries();
            if let Some(host) = entries.hosts.get_mut(&owner.host_id)
                && host.generation == owner.generation
            {
                host.remove_client(client_id, false);
            }
        }
        Ok(())
    }

    pub(crate) fn disconnect(&self, owner: &Owner, client_id: Uuid) -> Result<(), &'static str> {
        if owner.client_id.is_some() {
            return Err("only hosts can disconnect peers");
        }
        let mut entries = self.entries();
        let host = entries
            .hosts
            .get_mut(&owner.host_id)
            .ok_or("host is not connected")?;
        if host.generation != owner.generation {
            return Err("connection is no longer registered");
        }
        // Revoke routing now, but append Close behind already accepted final
        // replies. The normal per-frame send timeout bounds the drain.
        host.remove_client(client_id, true);
        Ok(())
    }

    pub(crate) fn remove(&self, owner: &Owner) {
        let mut entries = self.entries();
        let Some(host) = entries.hosts.get_mut(&owner.host_id) else {
            return;
        };
        if host.generation != owner.generation {
            return;
        }
        if let Some(client_id) = owner.client_id {
            host.remove_client(client_id, false);
        } else if let Some(host) = entries.hosts.remove(&owner.host_id) {
            host.peer.cancel.cancel();
            for client in host.clients.into_values() {
                client.peer.cancel.cancel();
            }
        }
    }
}

// An upgraded socket may fail, get cancelled, or be aborted by shutdown. All
// paths release only their own registration, including replacement generations.
pub(crate) struct RegistrationGuard {
    pub(crate) owner: Owner,
    registry: Arc<Registry>,
}

impl RegistrationGuard {
    pub(crate) fn new(owner: Owner, registry: Arc<Registry>) -> Self {
        Self { owner, registry }
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        self.registry.remove(&self.owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration(host_id: &str, host: bool) -> ValidRegistration {
        ValidRegistration {
            host_id: host_id.to_owned(),
            role: if host {
                RegisteredRole::Host
            } else {
                RegisteredRole::Client
            },
        }
    }

    fn drain(receiver: &mut mpsc::Receiver<QueuedMessage>) {
        while receiver.try_recv().is_ok() {}
    }

    fn next(receiver: &mut mpsc::Receiver<QueuedMessage>) -> serde_json::Value {
        let frame = receiver.try_recv().unwrap();
        let Message::Text(text) = &frame.message else {
            panic!("expected text frame");
        };
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn terminal_replay_beyond_peer_budget_preserves_fifo_and_client_route() {
        let registry = Registry::new(CancellationToken::new());
        let (host, mut host_frames) = registry.peer();
        let host_owner = registry
            .register(registration("burst", true), host)
            .unwrap();
        let (client, mut client_frames) = registry.peer();
        let client_owner = registry
            .register(registration("burst", false), client.clone())
            .unwrap();
        drain(&mut host_frames);
        drain(&mut client_frames);
        let fragment = "x".repeat(48 * 1024);
        let produce = async {
            for sequence in 0..171 {
                registry
                    .forward(
                        &host_owner,
                        client_owner.client_id,
                        &format!("{sequence}:{fragment}"),
                    )
                    .await
                    .unwrap();
            }
        };
        let consume = async {
            for sequence in 0..171 {
                let frame = client_frames
                    .recv()
                    .await
                    .expect("replay must not close the route");
                let Message::Text(text) = &frame.message else {
                    panic!("expected terminal data");
                };
                let value: serde_json::Value = serde_json::from_str(text).unwrap();
                assert_eq!(
                    value["client_id"],
                    client_owner.client_id.unwrap().to_string()
                );
                assert_eq!(value["payload"], format!("{sequence}:{fragment}"));
                tokio::task::yield_now().await;
            }
        };
        tokio::join!(produce, consume);
        assert!(!client.cancel.is_cancelled());
        assert!(client_frames.try_recv().is_err());
        registry
            .forward(&client_owner, None, "input after output burst")
            .await
            .unwrap();
        assert_eq!(
            next(&mut host_frames)["payload"],
            "input after output burst"
        );
    }

    #[tokio::test]
    async fn full_output_queue_waits_while_client_input_keeps_forwarding() {
        let registry = Registry::new(CancellationToken::new());
        let (host, mut host_frames) = registry.peer();
        let host_owner = registry
            .register(registration("waiting", true), host)
            .unwrap();
        let (client, mut client_frames) = registry.peer();
        let client_owner = registry
            .register(registration("waiting", false), client.clone())
            .unwrap();
        drain(&mut host_frames);
        drain(&mut client_frames);
        for sequence in 0..QUEUE_MESSAGES {
            registry
                .forward(&host_owner, client_owner.client_id, &sequence.to_string())
                .await
                .unwrap();
        }
        let pending = registry.forward(&host_owner, client_owner.client_id, "after pressure");
        tokio::pin!(pending);
        tokio::select! {
            biased;
            _ = &mut pending => panic!("full output queue must apply backpressure"),
            _ = tokio::task::yield_now() => {},
        }
        registry
            .forward(&client_owner, None, "input while output is queued")
            .await
            .unwrap();
        assert_eq!(
            next(&mut host_frames)["payload"],
            "input while output is queued"
        );
        assert!(!client.cancel.is_cancelled());
        assert_eq!(next(&mut client_frames)["payload"], "0");
        pending.await.unwrap();
        for sequence in 1..QUEUE_MESSAGES {
            assert_eq!(next(&mut client_frames)["payload"], sequence.to_string());
        }
        assert_eq!(next(&mut client_frames)["payload"], "after pressure");
    }

    #[tokio::test]
    async fn revocation_cancels_waiting_output_without_appending_after_close() {
        let registry = Registry::new(CancellationToken::new());
        let (host, mut host_frames) = registry.peer();
        let host_owner = registry
            .register(registration("revoked", true), host)
            .unwrap();
        let (client, mut client_frames) = registry.peer();
        let owner = registry
            .register(registration("revoked", false), client.clone())
            .unwrap();
        drain(&mut host_frames);
        drain(&mut client_frames);
        for _ in 0..QUEUE_MESSAGES {
            registry
                .forward(&host_owner, owner.client_id, "accepted before revocation")
                .await
                .unwrap();
        }
        let pending = registry.forward(&host_owner, owner.client_id, "must not arrive");
        tokio::pin!(pending);
        tokio::select! {
            biased;
            _ = &mut pending => panic!("output must be waiting"),
            _ = tokio::task::yield_now() => {},
        }
        registry
            .disconnect(&host_owner, owner.client_id.unwrap())
            .unwrap();
        pending.await.unwrap();
        for _ in 0..QUEUE_MESSAGES {
            assert_eq!(
                next(&mut client_frames)["payload"],
                "accepted before revocation"
            );
        }
        assert!(matches!(
            client_frames.try_recv().unwrap().message,
            Message::Close(_)
        ));
        assert!(client_frames.try_recv().is_err());
        assert_eq!(next(&mut host_frames)["type"], "peer_left");
        assert!(host_frames.try_recv().is_err());
    }

    #[test]
    fn duplicate_host_does_not_replace_or_cancel_original() {
        let registry = Registry::new(CancellationToken::new());
        let (host, mut frames) = registry.peer();
        let owner = registry
            .register(registration("a", true), host.clone())
            .unwrap();
        let (replacement, _receiver) = registry.peer();
        assert!(
            registry
                .register(registration("a", true), replacement)
                .is_err()
        );
        assert!(!host.cancel.is_cancelled());
        assert_eq!(registry.entries().hosts["a"].generation, owner.generation);
        assert_eq!(next(&mut frames)["type"], "ready");
    }

    #[tokio::test]
    async fn client_forwarding_ignores_spoofed_target_and_is_host_scoped() {
        let registry = Registry::new(CancellationToken::new());
        let (host, mut host_frames) = registry.peer();
        let host_owner = registry.register(registration("a", true), host).unwrap();
        let (other_host, mut other_frames) = registry.peer();
        let other_owner = registry
            .register(registration("b", true), other_host)
            .unwrap();
        let (client, mut client_frames) = registry.peer();
        let owner = registry.register(registration("a", false), client).unwrap();
        let (other_client, mut other_client_frames) = registry.peer();
        let other_client_owner = registry
            .register(registration("b", false), other_client)
            .unwrap();
        drain(&mut host_frames);
        drain(&mut other_frames);
        drain(&mut client_frames);
        drain(&mut other_client_frames);
        registry
            .forward(&owner, other_client_owner.client_id, "opaque")
            .await
            .unwrap();
        let forwarded = next(&mut host_frames);
        assert_eq!(forwarded["client_id"], owner.client_id.unwrap().to_string());
        assert_eq!(forwarded["payload"], "opaque");
        assert!(other_frames.try_recv().is_err());
        registry
            .forward(
                &host_owner,
                other_client_owner.client_id,
                "no cross-host forwarding",
            )
            .await
            .unwrap();
        assert!(other_client_frames.try_recv().is_err());
        assert!(
            registry
                .disconnect(&owner, other_client_owner.client_id.unwrap())
                .is_err()
        );
        registry
            .forward(&other_owner, other_client_owner.client_id, "right host")
            .await
            .unwrap();
        assert_eq!(next(&mut other_client_frames)["payload"], "right host");
    }

    #[test]
    fn stale_cleanup_cannot_remove_replacement_host_or_peers() {
        let registry = Registry::new(CancellationToken::new());
        let (host, _frames) = registry.peer();
        let old_host = registry.register(registration("a", true), host).unwrap();
        let (client, _client_frames) = registry.peer();
        let old_client = registry
            .register(registration("a", false), client.clone())
            .unwrap();
        registry.remove(&old_host);
        assert!(client.cancel.is_cancelled());
        let (new_host, _new_frames) = registry.peer();
        let replacement = registry
            .register(registration("a", true), new_host.clone())
            .unwrap();
        let (new_client, _new_client_frames) = registry.peer();
        registry
            .register(registration("a", false), new_client.clone())
            .unwrap();
        registry.remove(&old_host);
        registry.remove(&old_client);
        assert_eq!(
            registry.entries().hosts["a"].generation,
            replacement.generation
        );
        assert!(!new_host.cancel.is_cancelled());
        assert!(!new_client.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn disconnect_removes_authorization_and_emits_peer_left_once() {
        let registry = Registry::new(CancellationToken::new());
        let (host, mut host_frames) = registry.peer();
        let host_owner = registry.register(registration("a", true), host).unwrap();
        let (client, mut client_frames) = registry.peer();
        let owner = registry
            .register(registration("a", false), client.clone())
            .unwrap();
        drain(&mut host_frames);
        drain(&mut client_frames);
        registry
            .forward(&host_owner, owner.client_id, "final opaque pairing reply")
            .await
            .unwrap();
        registry
            .disconnect(&host_owner, owner.client_id.unwrap())
            .unwrap();
        assert!(client.is_closing());
        assert!(!client.cancel.is_cancelled());
        assert_eq!(
            next(&mut client_frames)["payload"],
            "final opaque pairing reply"
        );
        assert!(matches!(
            client_frames.try_recv().unwrap().message,
            Message::Close(_)
        ));
        assert!(
            registry
                .forward(&owner, None, "cannot send after revocation")
                .await
                .is_err()
        );
        assert_eq!(next(&mut host_frames)["type"], "peer_left");
        registry.remove(&owner);
        assert!(host_frames.try_recv().is_err());
    }

    #[test]
    fn data_queue_limits_do_not_cancel_the_destination() {
        let registry = Registry::new(CancellationToken::new());
        let (peer, _frames) = registry.peer();
        for _ in 0..QUEUE_MESSAGES {
            assert!(peer.send("small frame".into()));
        }
        assert!(!peer.send("overflow".into()));
        assert!(!peer.cancel.is_cancelled());
        let (peer, _frames) = registry.peer();
        assert!(peer.send("x".repeat(MAX_MESSAGE_BYTES)));
        assert!(peer.send("x".repeat(MAX_MESSAGE_BYTES)));
        assert!(!peer.send("one byte too many".into()));
        assert!(!peer.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn flooding_source_is_revoked_but_host_and_authorized_peer_keep_forwarding() {
        let registry = Registry::new(CancellationToken::new());
        let (host, mut host_frames) = registry.peer();
        let host_owner = registry
            .register(registration("a", true), host.clone())
            .unwrap();
        let (authorized, mut authorized_frames) = registry.peer();
        let authorized_owner = registry
            .register(registration("a", false), authorized.clone())
            .unwrap();
        let (attacker, _attacker_frames) = registry.peer();
        let attacker_owner = registry
            .register(registration("a", false), attacker.clone())
            .unwrap();
        drain(&mut host_frames);
        drain(&mut authorized_frames);
        for _ in 0..QUEUE_MESSAGES {
            registry
                .forward(&attacker_owner, None, "accepted opaque data")
                .await
                .unwrap();
        }
        assert!(
            registry
                .forward(&attacker_owner, None, "overflow")
                .await
                .is_err()
        );
        assert!(attacker.cancel.is_cancelled());
        assert!(!host.cancel.is_cancelled());
        assert!(!authorized.cancel.is_cancelled());
        // Observe every accepted frame followed by the reliable lifecycle event.
        for _ in 0..QUEUE_MESSAGES {
            let frame = next(&mut host_frames);
            assert_eq!(frame["type"], "data");
            assert_eq!(
                frame["client_id"],
                attacker_owner.client_id.unwrap().to_string()
            );
            assert_eq!(frame["payload"], "accepted opaque data");
        }
        let left = next(&mut host_frames);
        assert_eq!(left["type"], "peer_left");
        assert_eq!(
            left["client_id"],
            attacker_owner.client_id.unwrap().to_string()
        );
        assert!(
            registry
                .forward(&attacker_owner, None, "revoked")
                .await
                .is_err()
        );
        registry
            .forward(&authorized_owner, None, "authorized request after flood")
            .await
            .unwrap();
        assert_eq!(
            next(&mut host_frames)["payload"],
            "authorized request after flood"
        );
        registry
            .forward(
                &host_owner,
                authorized_owner.client_id,
                "authorized response after flood",
            )
            .await
            .unwrap();
        assert_eq!(
            next(&mut authorized_frames)["payload"],
            "authorized response after flood"
        );
    }

    #[test]
    fn lifecycle_churn_is_bounded_until_peer_left_is_consumed_without_cancelling_host() {
        let registry = Registry::new(CancellationToken::new());
        let (host, mut host_frames) = registry.peer();
        registry
            .register(registration("a", true), host.clone())
            .unwrap();
        let mut receivers = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..MAX_CLIENTS_PER_HOST {
            let (client, frames) = registry.peer();
            let owner = registry.register(registration("a", false), client).unwrap();
            ids.push(owner.client_id.unwrap());
            receivers.push(frames);
            registry.remove(&owner);
        }
        let (extra, _extra_frames) = registry.peer();
        assert!(registry.register(registration("a", false), extra).is_err());
        assert!(!host.cancel.is_cancelled());
        assert_eq!(next(&mut host_frames)["type"], "ready");
        for id in ids {
            assert_eq!(
                next(&mut host_frames),
                serde_json::json!({"type":"peer_joined","client_id":id})
            );
            assert_eq!(
                next(&mut host_frames),
                serde_json::json!({"type":"peer_left","client_id":id})
            );
        }
        let (new_client, _new_frames) = registry.peer();
        let owner = registry
            .register(registration("a", false), new_client)
            .unwrap();
        assert_eq!(
            next(&mut host_frames)["client_id"],
            owner.client_id.unwrap().to_string()
        );
        assert!(!host.cancel.is_cancelled());
    }

    #[test]
    fn hostless_and_over_capacity_clients_are_rejected() {
        let registry = Registry::new(CancellationToken::new());
        let (client, _frames) = registry.peer();
        assert!(
            registry
                .register(registration("absent", false), client)
                .is_err()
        );
        let (host, mut host_frames) = registry.peer();
        registry.register(registration("a", true), host).unwrap();
        let mut receivers = Vec::new();
        for _ in 0..MAX_CLIENTS_PER_HOST {
            drain(&mut host_frames);
            let (client, frames) = registry.peer();
            registry.register(registration("a", false), client).unwrap();
            receivers.push(frames);
        }
        let (client, _frames) = registry.peer();
        assert!(registry.register(registration("a", false), client).is_err());
    }
}
