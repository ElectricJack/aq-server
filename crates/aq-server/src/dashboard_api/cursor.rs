//! One opaque authenticated cursor format. Callers still own current ACL and
//! snapshot membership checks. A process restart intentionally invalidates keys.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroize::Zeroizing;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub principal: Uuid,
    pub authorization_partition: String,
    pub scope: Value,
    pub filters: Value,
    pub sort: String,
    pub page_size: u16,
    pub resource: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    binding: Binding,
    pub snapshot: String,
    pub position: String,
    pub issued: u64,
    pub expires: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorError {
    RestartRequired,
    AuthorizationChanged,
    Unavailable,
}
pub struct CursorCodec {
    key: Zeroizing<[u8; 32]>,
}
impl Default for CursorCodec {
    fn default() -> Self {
        let mut key = [0; 32];
        rand::thread_rng().fill_bytes(&mut key);
        Self {
            key: Zeroizing::new(key),
        }
    }
}
impl CursorCodec {
    pub fn encode(
        &self,
        binding: &Binding,
        snapshot: &str,
        position: &str,
        issued: u64,
        expires: u64,
    ) -> Result<String, CursorError> {
        if issued >= expires
            || snapshot.is_empty()
            || snapshot.len() > 512
            || position.len() > 512
            || binding.page_size == 0
            || binding.page_size > 100
        {
            return Err(CursorError::Unavailable);
        }
        let value = Position {
            binding: binding.clone(),
            snapshot: snapshot.into(),
            position: position.into(),
            issued,
            expires,
        };
        let bytes = serde_json::to_vec(&value).map_err(|_| CursorError::Unavailable)?;
        if bytes.len() > 2048 {
            return Err(CursorError::Unavailable);
        }
        let mut nonce = [0; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let cipher = ChaCha20Poly1305::new((&*self.key).into());
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &bytes,
                    aad: b"aq.human.cursor.v1",
                },
            )
            .map_err(|_| CursorError::Unavailable)?;
        let mut token = nonce.to_vec();
        token.extend(encrypted);
        Ok(URL_SAFE_NO_PAD.encode(token))
    }
    pub fn decode(
        &self,
        token: &str,
        binding: &Binding,
        now: u64,
    ) -> Result<Position, CursorError> {
        if token.len() > 4096 {
            return Err(CursorError::RestartRequired);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| CursorError::RestartRequired)?;
        if bytes.len() < 28 {
            return Err(CursorError::RestartRequired);
        }
        let cipher = ChaCha20Poly1305::new((&*self.key).into());
        let bytes = cipher
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad: b"aq.human.cursor.v1",
                },
            )
            .map_err(|_| CursorError::RestartRequired)?;
        let value: Position =
            serde_json::from_slice(&bytes).map_err(|_| CursorError::RestartRequired)?;
        if value.binding.principal != binding.principal
            || value.binding.authorization_partition != binding.authorization_partition
        {
            return Err(CursorError::AuthorizationChanged);
        }
        if &value.binding != binding || now < value.issued || now >= value.expires {
            return Err(CursorError::RestartRequired);
        }
        Ok(value)
    }
}
#[cfg(test)]
#[path = "cursor_tests.rs"]
mod tests;
