use std::{sync::Arc, time::Duration};

use axum::{
    Extension, Router,
    extract::{State, WebSocketUpgrade, ws::{CloseFrame, Message, WebSocket, close_code}},
    http::header,
    response::Response,
    routing::get,
};
use tokio::{sync::OwnedSemaphorePermit, time::{Instant, MissedTickBehavior, interval, timeout}};
use uuid::Uuid;

use crate::{
    config::{MAX_MESSAGE_BYTES, MAX_REGISTRATION_BYTES},
    protocol::{self, Frame, Incoming},
    registry::{RegistrationGuard, Registry},
};

pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(20);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);

// The TCP permit follows an HTTP upgrade into its WebSocket task. Merely
// finishing Hyper's upgrade future must not release the connection capacity.
pub(crate) struct ConnectionLease {
    pub(crate) _permit: OwnedSemaphorePermit,
}

pub(crate) fn router(registry: Arc<Registry>) -> Router {
    Router::new()
        .route("/healthz", get(|| async {
            ([(header::CONTENT_TYPE, "application/json")], r#"{"status":"ok","protocol":1}"#)
        }))
        .route("/v1/ws", get(upgrade))
        .with_state(registry)
}

async fn upgrade(
    State(registry): State<Arc<Registry>>,
    Extension(lease): Extension<Arc<ConnectionLease>>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(MAX_MESSAGE_BYTES + 64 * 1024)
        .on_upgrade(move |socket| session(socket, registry, lease))
}

async fn send(socket: &mut WebSocket, message: Message) -> Result<(), ()> {
    timeout(SEND_TIMEOUT, socket.send(message)).await.map_err(|_| ())?.map_err(|_| ())
}

async fn reject(socket: &mut WebSocket, message: &'static str) {
    if let Ok(error) = protocol::encode(&Frame::Error { message }) {
        let _ = send(socket, Message::Text(error.into())).await;
    }
    let _ = send(socket, Message::Close(Some(CloseFrame {
        code: close_code::POLICY,
        reason: "relay policy".into(),
    }))).await;
}

async fn session(mut socket: WebSocket, registry: Arc<Registry>, lease: Arc<ConnectionLease>) {
    let _lease = lease;
    let registration = tokio::select! {
        _ = registry.shutdown.cancelled() => return,
        first = timeout(HANDSHAKE_TIMEOUT, socket.recv()) => {
            match first {
                Ok(Some(Ok(Message::Text(text)))) if text.len() <= MAX_REGISTRATION_BYTES => {
                    protocol::registration(&text)
                }
                Err(_) => Err("registration timed out"),
                _ => Err("first frame must be a register text frame"),
            }
        }
    };
    let registration = match registration {
        Ok(registration) => registration,
        Err(error) => { reject(&mut socket, error).await; return; }
    };
    let (peer, mut outbound) = registry.peer();
    let owner = match registry.register(registration, peer.clone()) {
        Ok(owner) => owner,
        Err(error) => { reject(&mut socket, error).await; return; }
    };
    let registration = RegistrationGuard::new(owner, Arc::clone(&registry));
    let mut heartbeat = interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_ping = Instant::now();
    let mut pong_deadline: Option<Instant> = None;
    let mut drain_deadline = None;
    let mut input_burst = 0u8;
    loop {
        if peer.is_closing() && drain_deadline.is_none() {
            drain_deadline = Some(Instant::now() + Duration::from_secs(10));
        }
        tokio::select! {
            _ = peer.cancel.cancelled() => break,
            _ = async {
                match drain_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => break,
            queued = outbound.recv() => {
                let Some(queued) = queued else { break; };
                // Keep byte reservations until the frame is actually flushed.
                // Moving only the message leaves the permits alive in queued.
                let close = matches!(&queued.message, Message::Close(_));
                let sent = match drain_deadline {
                    Some(deadline) => tokio::time::timeout_at(deadline, send(&mut socket, queued.message)).await.unwrap_or(Err(())),
                    None => send(&mut socket, queued.message).await,
                };
                if sent.is_err() { break; }
                if close { break; }
            }
            incoming = socket.recv(), if !peer.is_closing() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if peer.is_closing() { continue; }
                        let frame = match protocol::incoming(&text) {
                            Ok(frame) => frame,
                            Err(_) => { reject(&mut socket, "invalid relay frame").await; break; }
                        };
                        let result = match frame {
                            Incoming::Data { client_id, payload } => {
                                let target = if registration.owner.client_id.is_some() {
                                    None // Never trust any client-supplied identity.
                                } else {
                                    client_id.as_ref().and_then(|id| serde_json::from_str::<Uuid>(id.get()).ok())
                                };
                                registry.forward(&registration.owner, target, &payload).await
                            }
                            Incoming::Disconnect { client_id } => registry.disconnect(&registration.owner, client_id),
                        };
                        if let Err(error) = result {
                            registry.remove(&registration.owner);
                            reject(&mut socket, error).await;
                            break;
                        }
                        // Buffered recv can complete without polling TCP again. Give
                        // target writers a turn before harmless fragments fill their FIFO.
                        input_burst += 1;
                        if input_burst == 16 {
                            input_burst = 0;
                            tokio::task::yield_now().await;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if send(&mut socket, Message::Pong(payload)).await.is_err() { break; }
                    }
                    Some(Ok(Message::Pong(payload))) => {
                        if payload.as_ref() == b"tty7" {
                            pong_deadline = None;
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        reject(&mut socket, "only JSON text frames are accepted").await;
                        break;
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                }
            }
            _ = heartbeat.tick(), if !peer.is_closing() => {
                if peer.is_closing() { continue; }
                let now = Instant::now();
                if pong_deadline.is_some_and(|deadline| now >= deadline) {
                    break;
                }
                if pong_deadline.is_none() && now.duration_since(last_ping) >= PING_INTERVAL {
                    if send(&mut socket, Message::Ping(b"tty7".as_slice().into())).await.is_err() { break; }
                    last_ping = now;
                    pong_deadline = Some(now + PONG_TIMEOUT);
                }
            }
        }
    }
    // Revoke ownership before waiting on an unresponsive socket's close write.
    drop(registration);
    if !peer.is_closing() {
        let _ = send(&mut socket, Message::Close(Some(CloseFrame {
            code: close_code::NORMAL,
            reason: "disconnected".into(),
        }))).await;
    }
}
