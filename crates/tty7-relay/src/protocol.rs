use std::io::{self, Write};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::config::{MAX_MESSAGE_BYTES, MAX_REGISTRATION_BYTES};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Host,
    Client,
}

// Intentionally no Debug: registration includes a host authentication secret.
#[derive(Deserialize)]
struct Registration {
    #[serde(rename = "type")]
    frame_type: String,
    role: Role,
    host_id: String,
    token: Option<String>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(token) = &mut self.token {
            token.zeroize();
        }
    }
}

pub(crate) enum RegisteredRole {
    Host,
    Client,
}

pub(crate) struct ValidRegistration {
    pub(crate) role: RegisteredRole,
    pub(crate) host_id: String,
}

pub(crate) fn registration(text: &str) -> Result<ValidRegistration, &'static str> {
    if text.len() > MAX_REGISTRATION_BYTES {
        return Err("registration is too large");
    }
    let mut registration: Registration =
        serde_json::from_str(text).map_err(|_| "invalid registration")?;
    if registration.frame_type != "register" || !valid_host_id(&registration.host_id) {
        return Err("invalid registration");
    }
    let role = match registration.role {
        Role::Client => RegisteredRole::Client,
        Role::Host => {
            let token = registration
                .token
                .as_ref()
                .ok_or("invalid host authentication")?;
            if token.len() != 43 {
                return Err("invalid host authentication");
            }
            let mut decoded = [0_u8; 32];
            let decoded_len = URL_SAFE_NO_PAD.decode_slice(token, &mut decoded);
            if !matches!(decoded_len, Ok(32)) {
                decoded.zeroize();
                return Err("invalid host authentication");
            }
            let digest = Sha256::digest(&decoded);
            decoded.zeroize();
            if hex::encode(digest) != registration.host_id {
                return Err("invalid host authentication");
            }
            RegisteredRole::Host
        }
    };
    Ok(ValidRegistration {
        role,
        host_id: std::mem::take(&mut registration.host_id),
    })
}

fn valid_host_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    // Do not expand an untrusted identity object into a JSON Value tree.
    // Clients ignore it; hosts must supply a UUID string.
    client_id: Option<Box<serde_json::value::RawValue>>,
    payload: Option<String>,
}

pub(crate) enum Incoming {
    Data {
        client_id: Option<Box<serde_json::value::RawValue>>,
        payload: String,
    },
    Disconnect {
        client_id: Uuid,
    },
}

pub(crate) fn incoming(text: &str) -> Result<Incoming, ()> {
    let envelope: Envelope = serde_json::from_str(text).map_err(|_| ())?;
    match envelope.kind.as_str() {
        "data" => Ok(Incoming::Data {
            client_id: envelope.client_id,
            payload: envelope.payload.ok_or(())?,
        }),
        "disconnect" => {
            let client_id = envelope.client_id.ok_or(())?;
            Ok(Incoming::Disconnect {
                client_id: serde_json::from_str(client_id.get()).map_err(|_| ())?,
            })
        }
        _ => Err(()),
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Frame<'a> {
    Ready {
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<Uuid>,
    },
    PeerJoined {
        client_id: Uuid,
    },
    PeerLeft {
        client_id: Uuid,
    },
    Data {
        client_id: Uuid,
        payload: &'a str,
    },
    Error {
        message: &'a str,
    },
}

struct BoundedJson(Vec<u8>);

impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_MESSAGE_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("relay frame exceeds size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn encode(frame: &Frame<'_>) -> Result<String, &'static str> {
    let mut buffer = BoundedJson(Vec::with_capacity(128));
    serde_json::to_writer(&mut buffer, frame).map_err(|_| "relay frame exceeds size limit")?;
    String::from_utf8(buffer.0).map_err(|_| "invalid relay frame")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_identity_is_derived_from_decoded_token() {
        let token = [7_u8; 32];
        let host_id = hex::encode(Sha256::digest(token));
        let register = serde_json::json!({
            "type": "register", "role": "host", "host_id": host_id,
            "token": URL_SAFE_NO_PAD.encode(token),
        });
        assert!(registration(&register.to_string()).is_ok());
        let mut forged = register;
        forged["host_id"] = serde_json::Value::String("0".repeat(64));
        assert!(registration(&forged.to_string()).is_err());
    }

    #[test]
    fn noncanonical_token_and_host_id_are_rejected() {
        let token = [0_u8; 32];
        let host_id = hex::encode(Sha256::digest(token));
        let mut register = serde_json::json!({
            "type": "register", "role": "host", "host_id": host_id,
            "token": format!("{}=", URL_SAFE_NO_PAD.encode(token)),
        });
        assert!(registration(&register.to_string()).is_err());
        register["token"] = serde_json::Value::String(URL_SAFE_NO_PAD.encode(token));
        register["host_id"] = serde_json::Value::String(host_id.to_uppercase());
        assert!(registration(&register.to_string()).is_err());
    }

    #[test]
    fn client_identity_field_is_not_a_validation_or_spoofing_input() {
        let frame =
            incoming(r#"{"type":"data","client_id":{"spoof":true},"payload":"opaque"}"#).unwrap();
        assert!(matches!(frame, Incoming::Data { .. }));
    }

    #[test]
    fn outgoing_envelope_also_obeys_frame_limit() {
        let payload = "x".repeat(MAX_MESSAGE_BYTES);
        assert!(
            encode(&Frame::Data {
                client_id: Uuid::new_v4(),
                payload: &payload
            })
            .is_err()
        );
        let payload = "x".repeat(MAX_MESSAGE_BYTES - 128);
        assert!(
            encode(&Frame::Data {
                client_id: Uuid::new_v4(),
                payload: &payload
            })
            .is_ok()
        );
    }
}
