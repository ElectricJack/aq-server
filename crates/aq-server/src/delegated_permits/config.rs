use super::CredentialKey;
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    key_file: PathBuf,
}

pub fn deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Arc<CredentialKey>>, D::Error> {
    Option::<Config>::deserialize(d)?
        .map(|config| {
            use std::{
                io::Read,
                os::unix::fs::{MetadataExt, OpenOptionsExt},
            };
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
                .open(config.key_file)
                .map_err(|_| serde::de::Error::custom("delegated permit key unavailable"))?;
            let metadata = file.metadata().map_err(serde::de::Error::custom)?;
            if !metadata.is_file() || metadata.mode() & 0o077 != 0 || metadata.len() != 32 {
                return Err(serde::de::Error::custom(
                    "delegated permit key must be a private 32-byte regular file",
                ));
            }
            let mut bytes = zeroize::Zeroizing::new([0; 32]);
            file.read_exact(bytes.as_mut())
                .map_err(serde::de::Error::custom)?;
            Ok(Arc::new(CredentialKey::new(*bytes)))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::fs::PermissionsExt};
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(deserialize_with = "deserialize")]
        delegated_permits: Option<Arc<CredentialKey>>,
    }
    #[test]
    fn delegated_key_file_reload_preserves_credentials_and_refuses_public_key_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&[7; 32]).unwrap();
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let text = format!(
            "[delegated_permits]\nkey_file = {:?}\n",
            file.path().to_str().unwrap()
        );
        let first: Wrapper = toml::from_str(&text).unwrap();
        let restarted: Wrapper = toml::from_str(&text).unwrap();
        let id = uuid::Uuid::new_v4();
        assert!(
            first.delegated_permits.unwrap().credential(id).unwrap()
                == restarted.delegated_permits.unwrap().credential(id).unwrap()
        );
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(toml::from_str::<Wrapper>(&text).is_err());
    }
}
