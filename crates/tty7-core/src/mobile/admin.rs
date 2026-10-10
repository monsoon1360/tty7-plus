//! Local-only control RPC. Call off the GUI thread.

use std::io;
use crate::client::ControlClient;
use crate::daemon::control::{ControlHello, ControlRequest, ReplyOk, feature};
use super::{AdminReply, AdminRequest};
pub use super::{Status, Device, Pending, Pairing};

fn call(request: AdminRequest) -> io::Result<AdminReply> {
    crate::daemon::spawn::ensure_running().map_err(io::Error::other)?;
    let client = ControlClient::connect(&ControlHello::host_rpc(
        uuid::Uuid::new_v4().to_string(), "mobile-admin",
    ))?;
    if !client.hello().has_feature(feature::MOBILE_ADMIN) {
        return Err(io::Error::new(io::ErrorKind::Unsupported,
            "the session daemon needs updating to support mobile pairing"));
    }
    let result = match client.request(ControlRequest::MobileAdmin { request })? {
        ReplyOk::MobileAdmin(reply) => Ok(reply),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "invalid mobile admin reply")),
    };
    client.close();
    result
}

pub fn status() -> io::Result<Status> {
    match call(AdminRequest::Status)? {
        AdminReply::Status(status) => Ok(status),
        _ => Err(io::Error::other("invalid mobile status reply")),
    }
}

pub fn configure(relay_url: String, tls_fingerprint: Option<String>, enabled: bool) -> io::Result<Status> {
    match call(AdminRequest::Configure { relay_url, tls_fingerprint, enabled })? {
        AdminReply::Status(status) => Ok(status),
        _ => Err(io::Error::other("invalid mobile configure reply")),
    }
}

pub fn begin_pairing() -> io::Result<Pairing> {
    match call(AdminRequest::BeginPairing)? {
        AdminReply::Pairing(pairing) => Ok(pairing),
        _ => Err(io::Error::other("invalid mobile pairing reply")),
    }
}

fn unit(request: AdminRequest) -> io::Result<()> {
    match call(request)? {
        AdminReply::Unit => Ok(()),
        _ => Err(io::Error::other("invalid mobile admin acknowledgement")),
    }
}

pub fn cancel_pairing() -> io::Result<()> { unit(AdminRequest::CancelPairing) }
pub fn approve(request_id: String) -> io::Result<()> { unit(AdminRequest::Approve { request_id }) }
pub fn reject(request_id: String) -> io::Result<()> { unit(AdminRequest::Reject { request_id }) }
pub fn revoke(credential_id: String) -> io::Result<()> { unit(AdminRequest::Revoke { credential_id }) }
