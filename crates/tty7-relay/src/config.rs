use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;

// A 256MiB deployment needs headroom for fragmented receive buffers, parsed
// JSON, bounded serialization, rustls, and the WebSocket write buffer. Sixteen
// sockets admit at most 32MiB queued data; even budgeting 8MiB extra per socket
// leaves 96MiB for the runtime and TLS/control overhead. Keep the 1MiB wire cap.
pub(crate) const MAX_CONNECTIONS: usize = 16;
pub(crate) const MAX_HOSTS: usize = 64;
pub(crate) const MAX_CLIENTS_PER_HOST: usize = 32;
pub(crate) const QUEUE_MESSAGES: usize = 128;
pub(crate) const PEER_QUEUE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const GLOBAL_QUEUE_BYTES: usize = MAX_CONNECTIONS * PEER_QUEUE_BYTES;
pub(crate) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_REGISTRATION_BYTES: usize = 2048;

#[derive(Parser)]
#[command(version, about = "tty7 mobile WebSocket relay (native TLS required outside loopback)")]
pub(crate) struct Options {
    /// IP socket address. Defaults to 0.0.0.0:8443 in production.
    #[arg(long, env = "TTY7_RELAY_LISTEN")]
    pub(crate) listen: Option<SocketAddr>,
    /// PEM certificate chain, leaf first (the leaf must contain the pinned IP SAN).
    #[arg(long, env = "TTY7_RELAY_CERT")]
    pub(crate) cert: Option<PathBuf>,
    /// PEM private key for the leaf certificate.
    #[arg(long, env = "TTY7_RELAY_KEY")]
    pub(crate) key: Option<PathBuf>,
    /// Development only: explicitly enable plain ws on an explicitly bound loopback IP.
    #[arg(long, env = "TTY7_RELAY_ALLOW_INSECURE_LOOPBACK", default_value_t = false)]
    pub(crate) allow_insecure_loopback: bool,
}

pub(crate) struct Config {
    pub(crate) listen: SocketAddr,
    pub(crate) tls: Option<(PathBuf, PathBuf)>,
}

impl Options {
    pub(crate) fn validate(self) -> Result<Config, &'static str> {
        if self.allow_insecure_loopback {
            let listen = self.listen.ok_or("plain ws requires an explicit --listen loopback address")?;
            if !listen.ip().is_loopback() {
                return Err("plain ws is permitted only on an explicit loopback bind");
            }
            if self.cert.is_some() || self.key.is_some() {
                return Err("--allow-insecure-loopback cannot be combined with --cert or --key");
            }
            return Ok(Config { listen, tls: None });
        }
        let cert = self.cert.ok_or("native TLS requires --cert/TTY7_RELAY_CERT and --key/TTY7_RELAY_KEY; plain ws is disabled")?;
        let key = self.key.ok_or("native TLS requires --key/TTY7_RELAY_KEY; plain ws is disabled")?;
        Ok(Config {
            listen: self.listen.unwrap_or(SocketAddr::from(([0, 0, 0, 0], 8443))),
            tls: Some((cert, key)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(listen: Option<&str>, insecure: bool) -> Options {
        Options {
            listen: listen.map(|value| value.parse().unwrap()),
            cert: None,
            key: None,
            allow_insecure_loopback: insecure,
        }
    }

    #[test]
    fn production_fails_closed_without_tls() {
        assert!(options(None, false).validate().is_err());
        assert!(options(Some("127.0.0.1:8443"), false).validate().is_err());
    }

    #[test]
    fn plaintext_needs_explicit_loopback_and_opt_in() {
        assert!(options(None, true).validate().is_err());
        assert!(options(Some("0.0.0.0:8443"), true).validate().is_err());
        assert!(options(Some("[::]:8443"), true).validate().is_err());
        assert!(options(Some("[::ffff:192.0.2.1]:8443"), true).validate().is_err());
        assert!(options(Some("127.0.0.1:8443"), true).validate().is_ok());
        assert!(options(Some("[::1]:8443"), true).validate().is_ok());
    }

    #[test]
    fn configured_buffers_leave_headroom_under_256_mib() {
        // Fragment/frame/JSON/serialization/write scratch allowance, not a
        // measurement of runtime RSS. Reserve the remaining 96MiB for it.
        let conservative_socket_bytes = 8 * MAX_MESSAGE_BYTES;
        assert!(MAX_CONNECTIONS * conservative_socket_bytes + GLOBAL_QUEUE_BYTES <= 160 * 1024 * 1024);
        // Global pressure must never strand a destination with free peer
        // capacity because some unrelated source used the shared budget.
        assert_eq!(GLOBAL_QUEUE_BYTES, MAX_CONNECTIONS * PEER_QUEUE_BYTES);
    }
}
