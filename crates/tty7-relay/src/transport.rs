use std::{fs::File, io::{self, BufReader}, path::Path, sync::Arc, time::Duration};

use axum::{Extension, Router};
use hyper::server::conn::http1;
use hyper_util::{rt::{TokioIo, TokioTimer}, service::TowerToHyperService};
use tokio::{io::{AsyncRead, AsyncWrite}, net::TcpListener, sync::Semaphore, task::JoinSet, time::timeout};
use tokio_rustls::{TlsAcceptor, rustls::{self, ServerConfig}};
use tokio_util::sync::CancellationToken;

use crate::{config::MAX_CONNECTIONS, registry::Registry, service::{self, ConnectionLease, HANDSHAKE_TIMEOUT}};

pub(crate) fn tls_acceptor(cert: &Path, key: &Path) -> io::Result<TlsAcceptor> {
    let mut cert_reader = BufReader::new(File::open(cert)?);
    let certificates = rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    if certificates.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "TLS certificate file contains no certificates"));
    }
    let mut key_reader = BufReader::new(File::open(key)?);
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "TLS key file contains no supported private key"))?;
    // Explicit provider avoids a workspace dependency enabling a different
    // rustls provider and making default-provider selection ambiguous.
    let mut config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(certificates, key).map_err(io::Error::other)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn http<I>(stream: I, application: Router, shutdown: CancellationToken)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut builder = http1::Builder::new();
    builder.keep_alive(true)
        .max_headers(64)
        .max_buf_size(16 * 1024)
        .timer(TokioTimer::new())
        .header_read_timeout(HANDSHAKE_TIMEOUT);
    let connection = builder.serve_connection(
        TokioIo::new(stream), TowerToHyperService::new(application),
    ).with_upgrades();
    tokio::select! {
        _ = shutdown.cancelled() => {},
        _ = timeout(HANDSHAKE_TIMEOUT, connection) => {},
    }
}

pub(crate) async fn serve(listener: TcpListener, tls: Option<TlsAcceptor>, registry: Arc<Registry>) -> io::Result<()> {
    let application = service::router(Arc::clone(&registry));
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            _ = registry.shutdown.cancelled() => break,
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                // No unbounded queue of accepted sockets or TLS handshake tasks.
                let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                stream.set_nodelay(true)?;
                let lease = Arc::new(ConnectionLease { _permit: permit });
                let application = application.clone().layer(Extension(Arc::clone(&lease)));
                let tls = tls.clone();
                let shutdown = registry.shutdown.clone();
                tasks.spawn(async move {
                    let _lease = lease;
                    if let Some(tls) = tls {
                        let stream = tokio::select! {
                            _ = shutdown.cancelled() => return,
                            accepted = timeout(HANDSHAKE_TIMEOUT, tls.accept(stream)) => {
                                match accepted { Ok(Ok(stream)) => stream, _ => return }
                            }
                        };
                        http(stream, application, shutdown).await;
                    } else {
                        http(stream, application, shutdown).await;
                    }
                });
            }
        }
    }
    // Upgraded WebSockets retain the same semaphore permit as their TCP socket.
    // Wait for both HTTP and WebSocket tasks, without admitting new connections.
    let _ = timeout(Duration::from_secs(10), async {
        while tasks.join_next().await.is_some() {}
        let _all_connections = connections.acquire_many(MAX_CONNECTIONS as u32).await;
    }).await;
    tasks.abort_all();
    Ok(())
}
