use std::{net::SocketAddr, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector,
    rustls::{self, ClientConfig, RootCertStore, pki_types::ServerName},
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config, tungstenite::Message,
};
use tokio_util::sync::CancellationToken;

use crate::{registry::Registry, transport};

const TEST_TIMEOUT: Duration = Duration::from_secs(3);
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start(
    tls: Option<TlsAcceptor>,
) -> (
    SocketAddr,
    CancellationToken,
    JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let registry = Arc::new(Registry::new(shutdown.clone()));
    let task = tokio::spawn(transport::serve(listener, tls, registry));
    (address, shutdown, task)
}

fn client_config(roots: RootCertStore) -> Arc<ClientConfig> {
    Arc::new(
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

async fn connect(address: SocketAddr, config: Arc<ClientConfig>) -> Socket {
    timeout(
        TEST_TIMEOUT,
        connect_async_tls_with_config(
            format!("wss://{address}/v1/ws"),
            None,
            true,
            Some(Connector::Rustls(config)),
        ),
    )
    .await
    .unwrap()
    .unwrap()
    .0
}

async fn send(socket: &mut Socket, frame: serde_json::Value) {
    timeout(
        TEST_TIMEOUT,
        socket.send(Message::Text(frame.to_string().into())),
    )
    .await
    .unwrap()
    .unwrap();
}

async fn next(socket: &mut Socket) -> serde_json::Value {
    timeout(TEST_TIMEOUT, async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Text(text) => break serde_json::from_str(&text).unwrap(),
                Message::Ping(data) => socket.send(Message::Pong(data)).await.unwrap(),
                message => panic!("expected relay text, got {message:?}"),
            }
        }
    })
    .await
    .unwrap()
}

async fn assert_closed(socket: &mut Socket) {
    timeout(TEST_TIMEOUT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(Some(frame)))) => {
                    assert!(
                        matches!(u16::from(frame.code), 1000 | 1008),
                        "unexpected close status: {frame:?}"
                    );
                    break;
                }
                Some(Ok(Message::Ping(data))) => socket.send(Message::Pong(data)).await.unwrap(),
                other => panic!("expected disconnect, got {other:?}"),
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn native_tls_health_forwarding_identity_duplicate_host_and_disconnect() {
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cert_path = directory.path().join("tls.crt");
    let key_path = directory.path().join("tls.key");
    std::fs::write(&cert_path, certificate.cert.pem()).unwrap();
    std::fs::write(&key_path, certificate.signing_key.serialize_pem()).unwrap();
    let tls = transport::tls_acceptor(&cert_path, &key_path).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(certificate.cert.der().clone()).unwrap();
    let config = client_config(roots);
    let (address, shutdown, task) = start(Some(tls)).await;

    let untrusted = timeout(
        TEST_TIMEOUT,
        connect_async_tls_with_config(
            format!("wss://{address}/v1/ws"),
            None,
            true,
            Some(Connector::Rustls(client_config(RootCertStore::empty()))),
        ),
    )
    .await
    .unwrap();
    assert!(
        untrusted.is_err(),
        "self-signed TLS must not be accepted without explicit trust"
    );

    let tcp = TcpStream::connect(address).await.unwrap();
    let mut health = TlsConnector::from(Arc::clone(&config))
        .connect(ServerName::try_from("127.0.0.1").unwrap(), tcp)
        .await
        .unwrap();
    health
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    timeout(TEST_TIMEOUT, health.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains(r#""status":"ok""#));

    let token = [42_u8; 32];
    let host_id = hex::encode(Sha256::digest(token));
    let registration = serde_json::json!({
        "type": "register", "role": "host", "host_id": host_id,
        "token": URL_SAFE_NO_PAD.encode(token),
    });
    let mut host = connect(address, Arc::clone(&config)).await;
    send(&mut host, registration.clone()).await;
    assert_eq!(next(&mut host).await, serde_json::json!({"type": "ready"}));
    let mut duplicate = connect(address, Arc::clone(&config)).await;
    send(&mut duplicate, registration).await;
    assert_eq!(next(&mut duplicate).await["type"], "error");
    assert_closed(&mut duplicate).await;

    let mut client = connect(address, Arc::clone(&config)).await;
    send(
        &mut client,
        serde_json::json!({"type": "register", "role": "client", "host_id": host_id}),
    )
    .await;
    let client_id = next(&mut client).await["client_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        next(&mut host).await,
        serde_json::json!({"type": "peer_joined", "client_id": client_id})
    );
    let mut other = connect(address, Arc::clone(&config)).await;
    send(
        &mut other,
        serde_json::json!({"type": "register", "role": "client", "host_id": host_id}),
    )
    .await;
    let other_id = next(&mut other).await["client_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(client_id, other_id);
    assert_eq!(
        next(&mut host).await,
        serde_json::json!({"type": "peer_joined", "client_id": other_id})
    );

    let payload = r#"{"kind":"encrypted","seq":0,"data":"opaque-test-ciphertext"}"#;
    send(
        &mut client,
        serde_json::json!({"type": "data", "client_id": other_id, "payload": payload}),
    )
    .await;
    assert_eq!(
        next(&mut host).await,
        serde_json::json!({"type": "data", "client_id": client_id, "payload": payload})
    );
    send(
        &mut host,
        serde_json::json!({"type": "data", "client_id": client_id, "payload": payload}),
    )
    .await;
    assert_eq!(
        next(&mut client).await,
        serde_json::json!({"type": "data", "client_id": client_id, "payload": payload})
    );
    assert!(
        timeout(Duration::from_millis(100), other.next())
            .await
            .is_err()
    );

    // Small PTY fragments must not evict a healthy target merely because the
    // host's already-buffered frames monopolize the relay executor.
    let publish = async {
        for sequence in 0..256 {
            send(
                &mut host,
                serde_json::json!({
                    "type": "data", "client_id": client_id,
                    "payload": format!("output-fragment-{sequence}"),
                }),
            )
            .await;
        }
    };
    let consume = async {
        for sequence in 0..256 {
            let frame = next(&mut client).await;
            assert_eq!(frame["client_id"], client_id);
            assert_eq!(frame["payload"], format!("output-fragment-{sequence}"));
        }
    };
    tokio::join!(publish, consume);

    // Pair approve/reject sends a final encrypted reply immediately followed
    // by disconnect. No sleep or acknowledgement is allowed between them.
    let final_reply = r#"{"kind":"encrypted","seq":1,"data":"final-opaque-reply"}"#;
    send(
        &mut host,
        serde_json::json!({"type": "data", "client_id": client_id, "payload": final_reply}),
    )
    .await;
    send(
        &mut host,
        serde_json::json!({"type": "disconnect", "client_id": client_id}),
    )
    .await;
    assert_eq!(
        next(&mut host).await,
        serde_json::json!({"type": "peer_left", "client_id": client_id})
    );
    assert_eq!(
        next(&mut client).await,
        serde_json::json!({"type": "data", "client_id": client_id, "payload": final_reply})
    );
    assert_closed(&mut client).await;
    host.close(None).await.unwrap();
    assert_closed(&mut other).await;
    shutdown.cancel();
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn hostless_client_and_forged_host_are_rejected() {
    let (address, shutdown, task) = start(None).await;
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/v1/ws"))
        .await
        .unwrap();
    send(
        &mut client,
        serde_json::json!({"type": "register", "role": "client", "host_id": "0".repeat(64)}),
    )
    .await;
    assert_eq!(next(&mut client).await["type"], "error");
    assert_closed(&mut client).await;
    let (mut host, _) = tokio_tungstenite::connect_async(format!("ws://{address}/v1/ws"))
        .await
        .unwrap();
    send(
        &mut host,
        serde_json::json!({
            "type": "register", "role": "host", "host_id": "0".repeat(64),
            "token": URL_SAFE_NO_PAD.encode([1_u8; 32]),
        }),
    )
    .await;
    assert_eq!(next(&mut host).await["type"], "error");
    assert_closed(&mut host).await;
    shutdown.cancel();
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap().unwrap();
}
