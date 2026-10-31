//! Host-supplied encryption keys. Keys never enter the database, logs, serde
//! output or Debug implementations. Preserve retired keys with encrypted backups.
use crate::backend;
use aq_playbooks_runner::StorageError;
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, OsRng, Payload, rand_core::RngCore},
};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct ProtectionKeyring {
    active_key_id: String,
    keys: BTreeMap<String, Zeroizing<[u8; 32]>>,
}
pub(crate) struct SealedValue {
    pub key_id: String,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl ProtectionKeyring {
    pub fn new(
        active_key_id: impl Into<String>,
        keys: BTreeMap<String, [u8; 32]>,
    ) -> Result<Self, StorageError> {
        let active_key_id = active_key_id.into();
        if keys.is_empty()
            || !keys.contains_key(&active_key_id)
            || keys
                .keys()
                .any(|key| key.is_empty() || key.len() > 128 || key.contains('\0'))
        {
            return Err(backend(
                "invalid protection keyring or unavailable active key",
            ));
        }
        Ok(Self {
            active_key_id,
            keys: keys
                .into_iter()
                .map(|(id, key)| (id, Zeroizing::new(key)))
                .collect(),
        })
    }
    pub fn active_key_id(&self) -> &str {
        &self.active_key_id
    }
    fn cipher(&self, key_id: &str) -> Result<XChaCha20Poly1305, StorageError> {
        let key = self
            .keys
            .get(key_id)
            .ok_or_else(|| backend("required protection key unavailable"))?;
        XChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| backend("invalid protection key"))
    }
    fn aad(namespace: &str, revision: u64, key_id: &str) -> Result<Vec<u8>, StorageError> {
        serde_json::to_vec(&(
            "aq-playbooks.snapshot.xchacha20poly1305.v1",
            namespace,
            revision,
            key_id,
        ))
        .map_err(backend)
    }
    pub(crate) fn seal(
        &self,
        namespace: &str,
        revision: u64,
        plaintext: &[u8],
    ) -> Result<SealedValue, StorageError> {
        let aad = Self::aad(namespace, revision, &self.active_key_id)?;
        self.seal_aad(&aad, plaintext)
    }
    fn seal_aad(&self, aad: &[u8], plaintext: &[u8]) -> Result<SealedValue, StorageError> {
        let mut nonce = [0u8; 24];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| backend("protection nonce generation unavailable"))?;
        let ciphertext = self
            .cipher(&self.active_key_id)?
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| backend("protected value encryption failed"))?;
        Ok(SealedValue {
            key_id: self.active_key_id.clone(),
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }
    pub(crate) fn open(
        &self,
        namespace: &str,
        revision: u64,
        key_id: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, StorageError> {
        let aad = Self::aad(namespace, revision, key_id)?;
        self.open_aad(&aad, key_id, nonce, ciphertext)
    }
    fn open_aad(
        &self,
        aad: &[u8],
        key_id: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, StorageError> {
        if nonce.len() != 24 {
            return Err(backend("protected value authentication failed"));
        }
        self.cipher(key_id)?
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| backend("protected value authentication failed"))
    }
    pub(crate) fn seal_artifact(
        &self,
        namespace: &str,
        digest: &str,
        plaintext: &[u8],
    ) -> Result<SealedValue, StorageError> {
        let aad = serde_json::to_vec(&(
            "aq-playbooks.artifact.xchacha20poly1305.v1",
            namespace,
            digest,
            &self.active_key_id,
        ))
        .map_err(backend)?;
        self.seal_aad(&aad, plaintext)
    }
    pub(crate) fn open_artifact(
        &self,
        namespace: &str,
        digest: &str,
        key_id: &str,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, StorageError> {
        let aad = serde_json::to_vec(&(
            "aq-playbooks.artifact.xchacha20poly1305.v1",
            namespace,
            digest,
            key_id,
        ))
        .map_err(backend)?;
        self.open_aad(&aad, key_id, nonce, ciphertext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn keys(active: &str) -> ProtectionKeyring {
        ProtectionKeyring::new(
            active,
            BTreeMap::from([("old".into(), [1; 32]), ("new".into(), [2; 32])]),
        )
        .unwrap()
    }
    #[test]
    fn authenticated_context_nonce_and_key_rotation() {
        let old = keys("old");
        let first = old.seal("n", 7, b"synthetic protected payload").unwrap();
        let again = old.seal("n", 7, b"synthetic protected payload").unwrap();
        assert_ne!(first.nonce, again.nonce);
        let rotated = keys("new");
        assert_eq!(
            &*rotated
                .open("n", 7, &first.key_id, &first.nonce, &first.ciphertext)
                .unwrap(),
            b"synthetic protected payload"
        );
        for (namespace, revision) in [("other", 7), ("n", 8)] {
            assert!(
                rotated
                    .open(
                        namespace,
                        revision,
                        &first.key_id,
                        &first.nonce,
                        &first.ciphertext
                    )
                    .is_err()
            );
        }
        let mut altered = first.ciphertext.clone();
        altered[0] ^= 1;
        assert!(
            old.open("n", 7, &first.key_id, &first.nonce, &altered)
                .is_err()
        );
        let wrong =
            ProtectionKeyring::new("old", BTreeMap::from([("old".into(), [3; 32])])).unwrap();
        assert!(
            wrong
                .open("n", 7, &first.key_id, &first.nonce, &first.ciphertext)
                .is_err()
        );
        let missing =
            ProtectionKeyring::new("new", BTreeMap::from([("new".into(), [2; 32])])).unwrap();
        assert!(
            missing
                .open("n", 7, &first.key_id, &first.nonce, &first.ciphertext)
                .is_err()
        );
        assert_eq!(rotated.seal("n", 8, b"next").unwrap().key_id, "new");
    }
}
