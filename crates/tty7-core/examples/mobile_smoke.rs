use std::{thread, time::Duration};
use tty7_core::{client::ControlClient, daemon::control::{ControlHello, ControlRequest, ReplyOk}, mobile::{AdminRequest, AdminReply}};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ControlClient::connect(&ControlHello::host_rpc("mobile-smoke".into(), "smoke"))?;
    let call = |request| -> Result<AdminReply, Box<dyn std::error::Error>> {
        match client.request(ControlRequest::MobileAdmin { request })? {
            ReplyOk::MobileAdmin(reply) => Ok(reply), other => Err(format!("unexpected: {other:?}").into()),
        }
    };
    call(AdminRequest::Configure { relay_url: "ws://127.0.0.1:48722/v1/ws".into(), tls_fingerprint: None, enabled: true })?;
    if let AdminReply::Pairing(pairing) = call(AdminRequest::BeginPairing)? {
        std::fs::write("/tmp/tty7-mobile-smoke-pair-uri", pairing.uri)?;
    }
    if let ReplyOk::WorkspaceTree(workspace) = client.request(ControlRequest::WorkspaceCreate { name: Some("Mobile smoke".into()), workspace: None })? {
        println!("Workspace {}", workspace.id);
    }
    println!("Smoke pairing ready");
    loop {
        if let AdminReply::Status(status) = call(AdminRequest::Status)? {
            for pending in status.pending { call(AdminRequest::Approve { request_id: pending.request_id })?; println!("Approved isolated smoke device"); }
        }
        thread::sleep(Duration::from_millis(250));
    }
}
