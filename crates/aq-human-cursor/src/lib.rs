//! One opaque authenticated cursor format. Callers still own current ACL and
//! snapshot membership checks. A process restart intentionally invalidates keys.
//! Moved out of aq-server (`dashboard_api::cursor` re-exports it) to keep the
//! server lib within its build-budget ratchet.
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
impl Binding {
    pub fn position(
        &self,
        snapshot: &str,
        position: &str,
        validity: std::ops::Range<u64>,
    ) -> Position {
        Position {
            binding: self.clone(),
            snapshot: snapshot.into(),
            position: position.into(),
            issued: validity.start,
            expires: validity.end,
        }
    }
}

impl Position {
    fn valid(&self) -> bool {
        let valid_snapshot = (1..=512).contains(&self.snapshot.len());
        let valid_page = (1..=100).contains(&self.binding.page_size) && self.position.len() <= 512;
        self.issued < self.expires && valid_snapshot && valid_page
    }
}

impl CursorCodec {
    pub fn encode(&self, value: Position) -> Result<String, CursorError> {
        if !value.valid() {
            return Err(CursorError::Unavailable);
        }
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
        if &value.binding != binding || !(value.issued..value.expires).contains(&now) {
            return Err(CursorError::RestartRequired);
        }
        Ok(value)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn binding() -> Binding {
        Binding {
            principal: Uuid::new_v4(),
            authorization_partition: "1".into(),
            scope: json!({"kind":"all_authorized"}),
            filters: json!({"q":"normalized"}),
            sort: "created_at,id".into(),
            page_size: 25,
            resource: "sessions".into(),
        }
    }
    #[test]
    fn shared_cursor_binds_every_dimension_and_expires_with_restart() {
        let codec = CursorCodec::default();
        let b = binding();
        let token = codec
            .encode(b.position("snapshot-1", "offset-2", 100..200))
            .unwrap();
        let decoded = codec.decode(&token, &b, 150).unwrap();
        assert_eq!(decoded.snapshot, "snapshot-1");
        assert_eq!(decoded.position, "offset-2");
        for changed in [
            Binding {
                principal: Uuid::new_v4(),
                ..b.clone()
            },
            Binding {
                authorization_partition: "2".into(),
                ..b.clone()
            },
            Binding {
                scope: json!({"kind":"project","project_id":Uuid::new_v4()}),
                ..b.clone()
            },
            Binding {
                filters: json!({"q":"changed"}),
                ..b.clone()
            },
            Binding {
                sort: "id".into(),
                ..b.clone()
            },
            Binding {
                page_size: 20,
                ..b.clone()
            },
            Binding {
                resource: "projects".into(),
                ..b.clone()
            },
        ] {
            assert!(codec.decode(&token, &changed, 150).is_err());
        }
        assert_eq!(
            codec.decode(&token, &b, 200).unwrap_err(),
            CursorError::RestartRequired
        );
        assert!(codec.decode(&token, &b, 99).is_err());
        assert!(CursorCodec::default().decode(&token, &b, 150).is_err());
        assert!(codec.decode("bad", &b, 150).is_err());
        assert!(codec.decode(&"a".repeat(9000), &b, 150).is_err());
        let mut bytes = token.into_bytes();
        bytes[20] = if bytes[20] == b'A' { b'B' } else { b'A' };
        assert!(
            codec
                .decode(std::str::from_utf8(&bytes).unwrap(), &b, 150)
                .is_err()
        );
    }
}
