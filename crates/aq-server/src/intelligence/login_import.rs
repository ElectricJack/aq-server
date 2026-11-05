//! C5b operator import/replace of a subscription login (`~/.codex/auth.json`,
//! or a Claude long-lived token file). The bytes become the newest stored
//! version of the credential's login, which always wins over node
//! write-backs. Nothing here prints, logs or returns the login bytes.
use crate::profiled_execution::login_refresh::{self, store::MAX_BYTES};
use anyhow::{Context, bail};
use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt, path::Path, sync::Arc};
use zeroize::Zeroizing;

/// Reads a regular file (symlinks refused) of 1..=64 KiB. No format check: a
/// Claude token is plain text, a Codex login is JSON.
pub fn read_source(source: &Path) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(source)
        .context("cannot open the login file (symlinks are refused)")?;
    if !file.metadata()?.is_file() {
        bail!("the login file must be a regular file");
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("cannot read the login file")?;
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        bail!("the login file must be 1..={MAX_BYTES} bytes");
    }
    Ok(bytes)
}

/// Stores `source` as the newest version of the configured subscription login
/// `credential` (`id@version`) at `revision`; returns that version.
pub async fn import(
    store: &crate::store::Store,
    loaded: &super::config::LoadedRegistry,
    credential: &str,
    revision: u64,
    source: &Path,
    now_ms: i64,
) -> anyhow::Result<i64> {
    let bytes = Arc::new(read_source(source)?);
    let profile = loaded
        .credentials
        .iter()
        .find(|c| c.revision == revision && login_refresh::identity(&c.profile) == credential)
        .map(|c| c.profile.clone())
        .with_context(|| format!("no configured credential {credential} at revision {revision}"))?;
    Ok(
        login_refresh::operator_replace(&store.pool, loaded, &profile, revision, bytes, now_ms)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_must_be_a_small_regular_file() {
        let root = tempfile::tempdir().unwrap();
        let token = root.path().join("token");
        std::fs::write(&token, b"sk-ant-oat01-SECRET").unwrap();
        assert_eq!(&**read_source(&token).unwrap(), b"sk-ant-oat01-SECRET");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&token, &link).unwrap();
        assert!(read_source(&link).is_err());
        std::fs::write(&token, b"").unwrap();
        assert!(read_source(&token).is_err());
        std::fs::write(&token, vec![b'x'; MAX_BYTES + 1]).unwrap();
        let big = read_source(&token).unwrap_err();
        assert!(!format!("{big:#}").contains("xxx"));
        assert!(read_source(root.path()).is_err());
    }
}
