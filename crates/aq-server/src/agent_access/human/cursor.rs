use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, aead::Aead};
use rand::RngCore;
use zeroize::Zeroizing;
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    pub actor: Uuid,
    pub project: Uuid,
    pub revision: u64,
    pub workspace: Uuid,
    pub kind: String,
    pub limit: u16,
    pub q: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    binding: Binding,
    upstream: String,
    issued: i64,
    expires: i64,
}
pub(super) struct Codec {
    key: Zeroizing<[u8; 32]>,
}
impl Codec {
    pub fn new() -> Self {
        let mut key = [0; 32];
        rand::thread_rng().fill_bytes(&mut key);
        Self {
            key: Zeroizing::new(key),
        }
    }
    pub fn encode(&self, b: &Binding, upstream: &str, now: i64) -> Result<String> {
        if upstream.is_empty() || upstream.len() > 256 || upstream.chars().any(char::is_control) {
            return Err(unavailable());
        }
        let value = Cursor {
            binding: b.clone(),
            upstream: upstream.into(),
            issued: now,
            expires: now.checked_add(30000).ok_or_else(unavailable)?,
        };
        let mut nonce = [0; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let cipher =
            ChaCha20Poly1305::new_from_slice(self.key.as_ref()).map_err(|_| unavailable())?;
        let bytes = serde_json::to_vec(&value).map_err(|_| unavailable())?;
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                chacha20poly1305::aead::Payload {
                    msg: &bytes,
                    aad: b"aq-human-graph-cursor-1",
                },
            )
            .map_err(|_| unavailable())?;
        let mut output = nonce.to_vec();
        output.extend(encrypted);
        Ok(URL_SAFE_NO_PAD.encode(output))
    }
    pub fn decode(&self, token: &str, b: &Binding, now: i64) -> Result<String> {
        if token.len() > 4096 {
            return Err(invalid());
        }
        let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| conflict())?;
        if bytes.len() < 28 {
            return Err(conflict());
        }
        let cipher =
            ChaCha20Poly1305::new_from_slice(self.key.as_ref()).map_err(|_| unavailable())?;
        let value = cipher
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                chacha20poly1305::aead::Payload {
                    msg: &bytes[12..],
                    aad: b"aq-human-graph-cursor-1",
                },
            )
            .map_err(|_| conflict())?;
        let value: Cursor = serde_json::from_slice(&value).map_err(|_| conflict())?;
        if &value.binding != b || now < value.issued || now >= value.expires {
            return Err(conflict());
        }
        Ok(value.upstream)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cursors_bind_actor_project_revision_filters_clock_and_boot() {
        let c = Codec::new();
        let b = Binding {
            actor: Uuid::new_v4(),
            project: Uuid::new_v4(),
            revision: 1,
            workspace: Uuid::new_v4(),
            kind: "task".into(),
            limit: 1,
            q: None,
        };
        let token = c.encode(&b, "private-upstream-cursor", 1000).unwrap();
        assert_eq!(
            c.decode(&token, &b, 1001).unwrap(),
            "private-upstream-cursor"
        );
        assert!(!token.contains("private-upstream-cursor"));
        for changed in [
            Binding {
                actor: Uuid::new_v4(),
                ..b.clone()
            },
            Binding {
                revision: 2,
                ..b.clone()
            },
            Binding {
                project: Uuid::new_v4(),
                ..b.clone()
            },
            Binding {
                kind: "knowledge".into(),
                ..b.clone()
            },
            Binding {
                limit: 2,
                ..b.clone()
            },
            Binding {
                q: Some("different".into()),
                ..b.clone()
            },
        ] {
            assert!(c.decode(&token, &changed, 1001).is_err())
        }
        assert!(c.decode(&token, &b, 999).is_err());
        assert!(c.decode(&token, &b, 31000).is_err());
        assert!(Codec::new().decode(&token, &b, 1001).is_err());
        let mut tampered = token.into_bytes();
        tampered[20] = if tampered[20] == b'A' { b'B' } else { b'A' };
        assert!(
            c.decode(std::str::from_utf8(&tampered).unwrap(), &b, 1001)
                .is_err()
        );
    }
}
