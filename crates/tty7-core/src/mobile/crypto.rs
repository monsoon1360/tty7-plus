use std::io;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::{Aead, Payload}};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use serde::{Serialize, Deserialize};
use sha2_mobile::{Digest, Sha256};

pub(super) const C2H: &[u8] = b"tty7-mobile-v1/c2h";
pub(super) const H2C: &[u8] = b"tty7-mobile-v1/h2c";
pub(super) const MAX_PLAINTEXT: usize = 256 * 1024;

pub(super) fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "mobile authentication refused")
}
pub(super) fn random() -> io::Result<[u8; 32]> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| io::Error::other("OS randomness unavailable"))?;
    Ok(bytes)
}
pub(super) fn encode(bytes: &[u8]) -> String { URL_SAFE_NO_PAD.encode(bytes) }
pub(super) fn decode(text: &str) -> io::Result<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).map_err(|_| denied())
}
pub(super) fn secret(text: &str) -> io::Result<[u8; 32]> {
    if text.len() != 43 { return Err(denied()); }
    decode(text)?.try_into().map_err(|_| denied())
}
pub(super) fn digest(bytes: &[u8]) -> String { hex::encode(Sha256::digest(bytes)) }

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Envelope {
    pub kind: String,
    pub seq: u64,
    pub data: String,
}

pub(super) struct Cipher {
    receive: Aes256Gcm,
    send: Aes256Gcm,
    receive_seq: u64,
    send_seq: u64,
}
impl Cipher {
    pub(super) fn host(psk: &[u8; 32], client: &[u8; 32], host: &[u8; 32]) -> Self {
        let mut salt = [0; 64];
        salt[..32].copy_from_slice(client);
        salt[32..].copy_from_slice(host);
        let hkdf = Hkdf::<Sha256>::new(Some(&salt), psk);
        let mut receive = [0; 32];
        let mut send = [0; 32];
        hkdf.expand(C2H, &mut receive).expect("fixed HKDF output length");
        hkdf.expand(H2C, &mut send).expect("fixed HKDF output length");
        Self { receive: Aes256Gcm::new((&receive).into()), send: Aes256Gcm::new((&send).into()), receive_seq: 0, send_seq: 0 }
    }
    pub(super) fn open(&mut self, envelope: Envelope) -> io::Result<serde_json::Value> {
        if envelope.kind != "encrypted" || envelope.seq != self.receive_seq
            || envelope.data.len() > (MAX_PLAINTEXT + 16).div_ceil(3) * 4 {
            return Err(denied());
        }
        let bytes = decode(&envelope.data)?;
        let nonce = nonce(self.receive_seq);
        let plaintext = self.receive.decrypt(Nonce::from_slice(&nonce), Payload { msg: &bytes, aad: C2H })
            .map_err(|_| denied())?;
        if plaintext.len() > MAX_PLAINTEXT { return Err(denied()); }
        let value = serde_json::from_slice(&plaintext).map_err(|_| denied())?;
        self.receive_seq = self.receive_seq.checked_add(1).ok_or_else(denied)?;
        Ok(value)
    }
    pub(super) fn seal(&mut self, value: &serde_json::Value) -> io::Result<String> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        if bytes.len() > MAX_PLAINTEXT { return Err(io::Error::other("mobile response too large")); }
        let nonce = nonce(self.send_seq);
        let encrypted = self.send.encrypt(Nonce::from_slice(&nonce), Payload { msg: &bytes, aad: H2C })
            .map_err(|_| denied())?;
        let result = serde_json::to_string(&Envelope { kind: "encrypted".into(), seq: self.send_seq, data: encode(&encrypted) })
            .map_err(io::Error::other)?;
        self.send_seq = self.send_seq.checked_add(1).ok_or_else(denied)?;
        Ok(result)
    }
}
fn nonce(seq: u64) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn client_frame(psk: &[u8; 32], client: &[u8; 32], host: &[u8; 32], seq: u64) -> Envelope {
        let mut salt = [0; 64];
        salt[..32].copy_from_slice(client);
        salt[32..].copy_from_slice(host);
        let mut key = [0; 32];
        Hkdf::<Sha256>::new(Some(&salt), psk).expand(C2H, &mut key).unwrap();
        let cipher = Aes256Gcm::new((&key).into());
        let data = cipher.encrypt(Nonce::from_slice(&nonce(seq)), Payload { msg: br#"{"type":"auth"}"#, aad: C2H }).unwrap();
        Envelope { kind: "encrypted".into(), seq, data: encode(&data) }
    }
    #[test]
    fn replay_and_out_of_order_never_authenticate() {
        let mut host = Cipher::host(&[1; 32], &[2; 32], &[3; 32]);
        assert!(host.open(client_frame(&[1; 32], &[2; 32], &[3; 32], 1)).is_err());
        assert_eq!(host.open(client_frame(&[1; 32], &[2; 32], &[3; 32], 0)).unwrap(), json!({"type":"auth"}));
        assert!(host.open(client_frame(&[1; 32], &[2; 32], &[3; 32], 0)).is_err());
    }
    #[test]
    fn wrong_psk_and_old_connection_are_refused() {
        let mut host = Cipher::host(&[1; 32], &[2; 32], &[3; 32]);
        assert!(host.open(client_frame(&[4; 32], &[2; 32], &[3; 32], 0)).is_err());
        assert!(host.open(client_frame(&[1; 32], &[2; 32], &[4; 32], 0)).is_err());
        assert!(host.open(client_frame(&[1; 32], &[2; 32], &[3; 32], 0)).is_ok());
    }
}
