//! Bounded local protected bytes. Typed callers must supply canonical, domain-separated
//! AAD containing every immutable record identity and revision. Keyrings are independently
//! configured; this primitive provides no authorization, retention or terminal identity.
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use rand::RngCore;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};
use zeroize::Zeroizing;
const MAX_PLAINTEXT: usize = 1048576;
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyFile {
    pub id: String,
    pub file: PathBuf,
}
impl std::fmt::Debug for KeyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyFile")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectionConfig {
    pub active_key_id: String,
    pub keys: Vec<KeyFile>,
}
pub struct Keyring {
    active: String,
    keys: BTreeMap<String, Zeroizing<[u8; 32]>>,
}
impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("active", &self.active)
            .field("key_count", &self.keys.len())
            .finish_non_exhaustive()
    }
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid protected storage configuration")]
    Config,
    #[error("unsafe or unavailable protected-storage key file")]
    KeyFile,
    #[error("protected record bound")]
    Bound,
    #[error("protected record key unavailable")]
    MissingKey,
    #[error("protected record authentication failed")]
    Authentication,
    #[error("protected record encryption failed")]
    Encryption,
}
#[derive(Clone)]
pub struct Sealed {
    pub version: u32,
    pub key_id: String,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}
impl std::fmt::Debug for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealed")
            .field("version", &self.version)
            .field("key_id", &self.key_id)
            .field("bytes", &self.ciphertext.len())
            .finish_non_exhaustive()
    }
}
fn key_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}
fn read_key(item: &KeyFile) -> Result<[u8; 32], Error> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(&item.file)
        .map_err(|_| Error::KeyFile)?;
    let metadata = file.metadata().map_err(|_| Error::KeyFile)?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.nlink() != 1
    {
        return Err(Error::KeyFile);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(33)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::KeyFile)?;
    bytes.as_slice().try_into().map_err(|_| Error::KeyFile)
}

impl Keyring {
    pub fn load(config: &ProtectionConfig) -> Result<Self, Error> {
        if config.keys.is_empty() || config.keys.len() > 8 || !key_id(&config.active_key_id) {
            return Err(Error::Config);
        }
        let mut keys = BTreeMap::new();
        for item in &config.keys {
            if !key_id(&item.id) || !item.file.is_absolute() || keys.contains_key(&item.id) {
                return Err(Error::Config);
            }
            keys.insert(item.id.clone(), Zeroizing::new(read_key(item)?));
        }
        if !keys.contains_key(&config.active_key_id) {
            return Err(Error::Config);
        }
        Ok(Self {
            active: config.active_key_id.clone(),
            keys,
        })
    }
    fn associated(aad: &[u8], key: &str, version: u32) -> Result<Vec<u8>, Error> {
        if aad.is_empty() || aad.len() > 8192 {
            return Err(Error::Bound);
        }
        // The primitive owns format/key binding; callers own a distinct typed domain.
        serde_json::to_vec(&("aq.protected-bytes", version, key, aad))
            .map_err(|_| Error::Authentication)
    }
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Sealed, Error> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(Error::Bound);
        }
        let key = self.keys.get(&self.active).ok_or(Error::MissingKey)?;
        let cipher =
            ChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| Error::Encryption)?;
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| Error::Encryption)?;
        let ad = Self::associated(aad, &self.active, 1)?;
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &ad,
                },
            )
            .map_err(|_| Error::Encryption)?;
        Ok(Sealed {
            version: 1,
            key_id: self.active.clone(),
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }
    pub fn open(&self, aad: &[u8], sealed: &Sealed) -> Result<Zeroizing<Vec<u8>>, Error> {
        if sealed.version != 1
            || !key_id(&sealed.key_id)
            || sealed.nonce.len() != 12
            || sealed.ciphertext.len() < 16
            || sealed.ciphertext.len() > MAX_PLAINTEXT + 16
        {
            return Err(Error::Bound);
        }
        let key = self.keys.get(&sealed.key_id).ok_or(Error::MissingKey)?;
        let cipher =
            ChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| Error::Authentication)?;
        let ad = Self::associated(aad, &sealed.key_id, sealed.version)?;
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&sealed.nonce),
                Payload {
                    msg: &sealed.ciphertext,
                    aad: &ad,
                },
            )
            .map_err(|_| Error::Authentication)?;
        Ok(Zeroizing::new(plaintext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn key_files_require_private_permissions_exact_length_and_single_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        let config = ProtectionConfig {
            active_key_id: "one".into(),
            keys: vec![KeyFile {
                id: "one".into(),
                file: path.clone(),
            }],
        };
        for (mode, length) in [(0o644, 32), (0o600, 31), (0o600, 33)] {
            std::fs::write(&path, vec![19u8; length]).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(Keyring::load(&config).unwrap_err(), Error::KeyFile);
        }
        std::fs::write(&path, [19u8; 32]).unwrap();
        Keyring::load(&config).unwrap();
        let link = dir.path().join("link");
        std::fs::hard_link(&path, &link).unwrap();
        assert_eq!(Keyring::load(&config).unwrap_err(), Error::KeyFile);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&link, &path).unwrap();
        assert_eq!(Keyring::load(&config).unwrap_err(), Error::KeyFile);
    }

    #[test]
    fn typed_domains_are_independent_and_aad_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, [19u8; 32]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ring = Keyring::load(&ProtectionConfig {
            active_key_id: "one".into(),
            keys: vec![KeyFile {
                id: "one".into(),
                file: path,
            }],
        })
        .unwrap();
        let terminal = br#"["terminal/v1",{"revision":1}]"#;
        let transcript = br#"["transcript/v1",{"revision":1}]"#;
        let sealed = ring.seal(terminal, b"private bytes").unwrap();
        assert_eq!(
            ring.open(terminal, &sealed).unwrap().as_slice(),
            b"private bytes"
        );
        assert_eq!(
            ring.open(transcript, &sealed).unwrap_err(),
            Error::Authentication
        );
        assert_eq!(ring.seal(&[], b"x").unwrap_err(), Error::Bound);
        assert_eq!(ring.seal(&vec![0; 8193], b"x").unwrap_err(), Error::Bound);
        assert_eq!(
            ring.seal(terminal, &vec![0; MAX_PLAINTEXT + 1])
                .unwrap_err(),
            Error::Bound
        );
    }
}
