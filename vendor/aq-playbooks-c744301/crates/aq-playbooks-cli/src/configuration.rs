//! Explicit host-owned key configuration. Key material never appears in diagnostics.
use anyhow::{Context, Result, anyhow, ensure};
use aq_playbooks_postgres::ProtectionKeyring;
use serde::Deserialize;
use std::{collections::BTreeMap, fs, io::Read, path::Path};
use zeroize::{Zeroize, Zeroizing};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    active_key_id: String,
    keys: BTreeMap<String, String>,
}
impl Drop for KeyFile {
    fn drop(&mut self) {
        for value in self.keys.values_mut() {
            value.zeroize();
        }
    }
}
/// This file is host configuration, never an artifact or database record.
/// Retain old keys for encrypted backups until their retention period expires.
pub fn read_keyring(path: &Path) -> Result<ProtectionKeyring> {
    let file = fs::File::open(path).context("open protection key file")?;
    let metadata = file
        .metadata()
        .context("read protection key file metadata")?;
    ensure!(
        metadata.is_file(),
        "protection key configuration must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "protection key file must deny group and other access (mode 0600)"
        );
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(65_537)
        .read_to_end(&mut bytes)
        .context("read protection key file")?;
    ensure!(bytes.len() <= 65_536, "protection key file exceeds 64 KiB");
    let config: KeyFile = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow!("invalid protection key configuration"))?;
    ensure!(
        !config.keys.is_empty() && config.keys.len() <= 32,
        "keyring must contain 1..32 keys"
    );
    let mut keys = BTreeMap::new();
    for (id, encoded) in &config.keys {
        ensure!(!id.is_empty() && id.len() <= 128, "invalid key identifier");
        let key = decode_key(encoded).ok_or_else(|| {
            anyhow!("each protection key must encode exactly 32 bytes as hexadecimal")
        })?;
        keys.insert(id.clone(), key);
    }
    Ok(ProtectionKeyring::new(config.active_key_id.clone(), keys)?)
}
fn decode_key(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut result = [0u8; 32];
    for (index, bytes) in value.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        };
        match (digit(bytes[0]), digit(bytes[1])) {
            (Some(a), Some(b)) => result[index] = (a << 4) | b,
            _ => {
                result.zeroize();
                return None;
            }
        }
    }
    Some(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_nonhex_wrong_lengths_and_unicode_without_echoing_input() {
        assert!(decode_key("secret").is_none());
        assert!(decode_key(&"é".repeat(32)).is_none());
        assert!(decode_key(&"gg".repeat(32)).is_none());
        assert_eq!(decode_key(&"ab".repeat(32)), Some([0xab; 32]));
    }
    #[test]
    fn secret_error_contains_no_key_data() {
        let bytes = br#"{"active_key_id":"x","keys":{"x":"very-secret"},"unexpected":true}"#;
        let result: Result<KeyFile, _> = serde_json::from_slice(bytes);
        let error = result
            .map_err(|_| anyhow!("invalid protection key configuration"))
            .err()
            .unwrap();
        assert!(!error.to_string().contains("very-secret"));
    }
}
