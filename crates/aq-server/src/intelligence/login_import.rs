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

/// Whether a configured credential is a Claude subscription login (the
/// `claude setup-token` value), the only shape whose CR/LF is trimmed. API
/// keys and other providers are delivered byte-exact (MS1 review nit).
pub use aq_intelligence_config::material::{is_claude_setup_token, normalize_claude_token};

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
    let index = loaded
        .credentials
        .iter()
        .position(|c| c.revision == revision && login_refresh::identity(&c.profile) == credential)
        .with_context(|| format!("no configured credential {credential} at revision {revision}"))?;
    let configured = &loaded.credentials[index];
    let profile = configured.profile.clone();
    let bytes = read_source(source)?;
    let setup_token = loaded
        .config
        .credentials
        .get(index)
        .is_some_and(is_claude_setup_token);
    let bytes = Arc::new(if setup_token {
        normalize_claude_token(bytes)?
    } else {
        bytes
    });
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
    #[test]
    fn only_claude_subscription_logins_are_trimmed() {
        let config = |kind: &str, provider: &str| -> super::super::config::CredentialConfig {
            serde_json::from_value(serde_json::json!({
                "kind": kind,
                "credential": {"profile": {"id": "p", "version": "1", "digest": "a".repeat(64)},
                    "provider": provider, "account_reference": "a", "endpoint_reference": "e",
                    "revision": 1, "enabled": true, "provisionable": false},
                "allowed_node_ids": [], "secret_file": null}))
            .unwrap()
        };
        assert!(is_claude_setup_token(&config(
            "subscription-login",
            "anthropic"
        )));
        assert!(!is_claude_setup_token(&config("api-key", "anthropic")));
        assert!(!is_claude_setup_token(&config(
            "subscription-login",
            "openai"
        )));
    }
    #[test]
    fn claude_token_trailing_newline_is_trimmed_and_other_shapes_refused() {
        let ok =
            normalize_claude_token(Zeroizing::new(b"sk-ant-oat01-SECRET\r\n".to_vec())).unwrap();
        assert_eq!(&**ok, b"sk-ant-oat01-SECRET");
        for bad in [&b"\n"[..], b"sk-ant two\n", b"line1\nline2\n"] {
            let error = normalize_claude_token(Zeroizing::new(bad.to_vec())).unwrap_err();
            assert!(!format!("{error:#}").contains("sk-ant"));
        }
    }
}
