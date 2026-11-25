//! Startup-only protected destination configuration. No URL enters SQL or Debug.
use crate::discord::Webhook;
use aq_lifecycle_api::Digest;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::Arc,
};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointFile {
    key: String,
    revision: u64,
    secret_file: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    revision: u64,
    endpoints: Vec<EndpointFile>,
}
pub struct Endpoint {
    pub revision: u64,
    pub webhook: Option<Webhook>,
    pub fingerprint: Option<String>,
}
pub struct Loaded {
    pub revision: u64,
    pub endpoints: BTreeMap<String, Endpoint>,
}
impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationConfiguration")
            .field("revision", &self.revision)
            .field("endpoint_keys", &self.endpoints.keys().collect::<Vec<_>>())
            .finish()
    }
}
fn revision_valid(revision: u64) -> bool {
    (1..=i64::MAX as u64).contains(&revision)
}

impl EndpointFile {
    fn valid(&self) -> bool {
        let key = (1..=128).contains(&self.key.len())
            && self
                .key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        key && revision_valid(self.revision) && self.secret_file.is_absolute()
    }

    fn endpoint(&self) -> Result<Endpoint, &'static str> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
            .open(&self.secret_file);
        match file {
            Ok(file) => self.read(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Endpoint {
                revision: self.revision,
                webhook: None,
                fingerprint: None,
            }),
            Err(_) => Err("cannot open protected notification endpoint"),
        }
    }

    fn read(&self, file: std::fs::File) -> Result<Endpoint, &'static str> {
        let metadata = file
            .metadata()
            .map_err(|_| "notification endpoint metadata unavailable")?;
        if !protected(&metadata) {
            return Err("notification endpoint must be an owner-only regular file");
        }
        let mut bytes = Vec::new();
        file.take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read protected notification endpoint")?;
        if bytes.is_empty() || bytes.len() > 4096 {
            return Err("notification endpoint byte bound");
        }
        let secret = std::str::from_utf8(&bytes)
            .map_err(|_| "invalid protected notification endpoint")?
            .trim();
        let webhook =
            Webhook::parse(secret).map_err(|_| "invalid protected notification endpoint")?;
        Ok(Endpoint {
            revision: self.revision,
            webhook: Some(webhook),
            fingerprint: Some(Digest::of(secret.as_bytes()).as_str().to_owned()),
        })
    }
}

fn protected(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
        && metadata.mode() & 0o077 == 0
        && metadata.uid() == nix::unistd::geteuid().as_raw()
        && metadata.nlink() == 1
}

impl Configuration {
    fn load(self) -> Result<Loaded, &'static str> {
        if !revision_valid(self.revision) || self.endpoints.len() > 32 {
            return Err("invalid notification configuration bounds");
        }
        let mut endpoints = BTreeMap::new();
        for item in self.endpoints {
            if !item.valid() || endpoints.contains_key(&item.key) {
                return Err("invalid notification endpoint identity");
            }
            let endpoint = item.endpoint()?;
            endpoints.insert(item.key, endpoint);
        }
        Ok(Loaded {
            revision: self.revision,
            endpoints,
        })
    }
}
pub fn deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Arc<Loaded>>, D::Error> {
    let config = Option::<Configuration>::deserialize(d)?;
    config
        .map(|c| c.load().map(Arc::new).map_err(serde::de::Error::custom))
        .transpose()
}
#[cfg(feature = "test-support")]
impl Loaded {
    pub fn fixture(revision: u64, key: &str, endpoint_revision: u64, url: &str) -> Self {
        let webhook =
            Webhook::loopback_fixture(url).expect("explicit loopback notification fixture");
        Self {
            revision,
            endpoints: BTreeMap::from([(
                key.to_owned(),
                Endpoint {
                    revision: endpoint_revision,
                    webhook: Some(webhook),
                    fingerprint: Some(Digest::of(url.as_bytes()).as_str().to_owned()),
                },
            )]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    fn configuration(path: &std::path::Path) -> Configuration {
        Configuration {
            revision: 1,
            endpoints: vec![EndpointFile {
                key: "discord-test".into(),
                revision: 1,
                secret_file: path.into(),
            }],
        }
    }
    #[test]
    fn protected_configuration_refuses_unsafe_files_and_keeps_credentials_opaque() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("webhook");
        let secret = "https://discord.com/api/v10/webhooks/42/synthetic_fixture_secret_token";
        std::fs::write(&file, secret).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let loaded = configuration(&file).load().unwrap();
        let debug = format!("{loaded:?}");
        assert!(!debug.contains(secret));
        assert!(!debug.contains("synthetic_fixture_secret_token"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(configuration(&file).load().is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("symlink");
        symlink(&file, &link).unwrap();
        assert!(configuration(&link).load().is_err());
        let hard = dir.path().join("hardlink");
        std::fs::hard_link(&file, &hard).unwrap();
        assert!(configuration(&file).load().is_err());
        std::fs::remove_file(hard).unwrap();
        std::fs::write(&file, vec![b'x'; 4097]).unwrap();
        assert!(configuration(&file).load().is_err());
        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(configuration(&fifo).load().is_err());
        let missing = configuration(&dir.path().join("missing")).load().unwrap();
        assert!(missing.endpoints["discord-test"].webhook.is_none());
    }
}
