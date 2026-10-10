mod config;
mod protocol;
mod registry;
mod service;
mod transport;

#[cfg(test)]
mod tests;

use std::{io, process::ExitCode, sync::Arc};

use clap::Parser;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Startup/listener errors only. Never print connection frames,
            // authentication tokens, terminal payloads, or parsing errors.
            eprintln!("tty7-relay: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> io::Result<()> {
    let config = config::Options::parse().validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let tls = config.tls.as_ref().map(|(cert, key)| transport::tls_acceptor(cert, key)).transpose()?;
    let listener = TcpListener::bind(config.listen).await?;
    eprintln!("tty7-relay listening on {} ({})", listener.local_addr()?, if tls.is_some() { "native TLS" } else { "explicit loopback ws" });
    let shutdown = CancellationToken::new();
    let registry = Arc::new(registry::Registry::new(shutdown.clone()));
    let server = transport::serve(listener, tls, Arc::clone(&registry));
    tokio::pin!(server);
    let result = tokio::select! {
        result = &mut server => result,
        signal = shutdown_signal() => {
            shutdown.cancel();
            let stopped = server.await;
            signal.and(stopped)
        }
    };
    shutdown.cancel();
    result
}

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}
