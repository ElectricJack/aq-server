use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;

/// Vault `[git_http]`: aq-git's git smart-HTTP socket and the transfer limits.
/// Every limit is configuration; none has a default.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHttpConfig {
    /// `aq-git serve --git-http-socket`.
    pub socket: PathBuf,
    /// Whole-request limit of one fetch or push, in place of the Bulk deadline.
    pub transfer_timeout_ms: u64,
    /// Sent to aq-git as `AQ-Max-Push-Bytes`; also bounds the request body.
    pub max_push_bytes: u64,
    /// Concurrent git transfers through this server.
    pub concurrency: usize,
}
impl GitHttpConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.socket.is_absolute()
            || self
                .socket
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("git_http.socket must be an absolute path without '..'".into());
        }
        if self.transfer_timeout_ms == 0 || self.transfer_timeout_ms > i64::MAX as u64 {
            return Err("git_http.transfer_timeout_ms must be positive".into());
        }
        if self.max_push_bytes == 0 || self.max_push_bytes > i64::MAX as u64 {
            return Err("git_http.max_push_bytes must be positive".into());
        }
        if !(1..=Semaphore::MAX_PERMITS).contains(&self.concurrency) {
            return Err("git_http.concurrency must be positive".into());
        }
        Ok(())
    }
}
/// Loaded `[git_http]` with its transfer slots.
pub struct Loaded {
    pub config: GitHttpConfig,
    pub(crate) slots: Semaphore,
}
impl Loaded {
    pub fn new(config: GitHttpConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            slots: Semaphore::new(config.concurrency),
            config,
        })
    }
}
impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}
/// `deserialize_with` for an optional `[git_http]` section.
pub fn deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Arc<Loaded>>, D::Error> {
    Option::<GitHttpConfig>::deserialize(d)?
        .map(|c| Loaded::new(c).map(Arc::new))
        .transpose()
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_limit_is_required_and_positive() {
        let ok: GitHttpConfig = serde_json::from_value(serde_json::json!({
            "socket": "/run/aq-git/git-http.sock", "transfer_timeout_ms": 600000,
            "max_push_bytes": 1048576, "concurrency": 2
        }))
        .unwrap();
        Loaded::new(ok.clone()).unwrap();
        assert!(
            serde_json::from_value::<GitHttpConfig>(
                serde_json::json!({"socket": "/run/aq-git/git-http.sock"})
            )
            .is_err()
        );
        for bad in [
            GitHttpConfig {
                socket: "relative.sock".into(),
                ..ok.clone()
            },
            GitHttpConfig {
                transfer_timeout_ms: 0,
                ..ok.clone()
            },
            GitHttpConfig {
                max_push_bytes: 0,
                ..ok.clone()
            },
            GitHttpConfig {
                concurrency: 0,
                ..ok.clone()
            },
        ] {
            assert!(Loaded::new(bad).is_err());
        }
    }
}
