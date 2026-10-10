//! Daemon-owned, explicitly enabled, end-to-end encrypted mobile access.

pub mod admin;
mod crypto;
mod service;
mod store;

use serde::{Deserialize, Serialize};

pub const DEFAULT_RELAY_URL: &str = "wss://relay.example.invalid:8443/v1/ws";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub enabled: bool,
    pub relay_url: String,
    pub tls_fingerprint: Option<String>,
    pub connected: bool,
    pub error: Option<String>,
    pub devices: Vec<Device>,
    pub pending: Vec<Pending>,
    pub pairing: Option<Pairing>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub credential_id: String,
    pub name: String,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub request_id: String,
    pub device_name: String,
    pub code: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    pub uri: String,
    pub expires_at: u64,
}

// The QR contains the bootstrap secret. Control tracing must never print it.
impl std::fmt::Debug for Pairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pairing")
            .field("uri", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRequest {
    Status,
    Configure {
        relay_url: String,
        tls_fingerprint: Option<String>,
        enabled: bool,
    },
    BeginPairing,
    CancelPairing,
    Approve {
        request_id: String,
    },
    Reject {
        request_id: String,
    },
    Revoke {
        credential_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminReply {
    Status(Status),
    Pairing(Pairing),
    Unit,
}

pub(crate) use service::{Backend, Observation, dispatch, start};
