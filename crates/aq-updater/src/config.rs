//! `/etc/agent-q/updater.toml`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config: {0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeConfig {
    pub project_dir: PathBuf,
    pub files: Vec<PathBuf>,
    #[serde(default)]
    pub env_files: Vec<PathBuf>,
    pub project_name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VerifierConfig {
    CosignKeyless {
        cosign_path: PathBuf,
        identity_regexp: String,
        oidc_issuer: String,
    },
    #[cfg(feature = "test-verifier")]
    TestKey { public_key_hex: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdaterConfig {
    pub state_dir: PathBuf,
    pub backup_dir: PathBuf,
    pub compose: ComposeConfig,
    #[serde(default = "d_repo")]
    pub release_repo: String,
    #[serde(default = "d_channel")]
    pub channel: String,
    pub verifier: VerifierConfig,
    #[serde(default = "d_ready")]
    pub readiness_deadline_secs: u64,
    #[serde(default = "d_poll")]
    pub poll_interval_secs: u64,
    #[serde(default = "d_free")]
    pub min_free_bytes: u64,
    #[serde(default = "d_keep")]
    pub backups_keep: usize,
    /// The `auth_mode` a release's readiness report must show.
    #[serde(default = "d_auth_mode")]
    pub expected_auth_mode: String,
    /// Seconds to wait after entering maintenance before the backup starts,
    /// so admin writes already past the maintenance gate can commit.
    #[serde(default = "d_drain")]
    pub maintenance_drain_secs: u64,
}

fn d_repo() -> String {
    "ghcr.io/electricjack/agent-q-release".into()
}
fn d_channel() -> String {
    "stable".into()
}
fn d_ready() -> u64 {
    120
}
fn d_poll() -> u64 {
    3600
}
fn d_free() -> u64 {
    2_147_483_648
}
fn d_keep() -> usize {
    5
}
fn d_drain() -> u64 {
    5
}
fn d_auth_mode() -> String {
    "cloudflare_access".into()
}

impl UpdaterConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let cfg: UpdaterConfig = toml::from_str(text).map_err(|e| {
            let msg = e.to_string();
            // Without the test-verifier feature the `test_key` variant does
            // not exist; serde's "unknown variant" message is not obvious.
            if msg.contains("unknown variant") && msg.contains("test_key") {
                ConfigError::Parse(
                    "verifier.kind \"test_key\" requires the test-verifier build feature".into(),
                )
            } else if msg.contains("unknown variant") {
                ConfigError::Parse(format!("verifier.kind: {msg}"))
            } else {
                ConfigError::Parse(msg)
            }
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.validate_paths()?;
        self.validate_update_settings()?;
        self.validate_repository()?;
        if self.poll_interval_secs < 1 {
            return Err(ConfigError::Invalid(
                "poll_interval_secs must be >= 1".into(),
            ));
        }
        self.validate_verifier()
    }

    fn validate_paths(&self) -> Result<(), ConfigError> {
        let bad = |m: String| Err(ConfigError::Invalid(m));
        for (name, p) in [
            ("state_dir", &self.state_dir),
            ("backup_dir", &self.backup_dir),
            ("compose.project_dir", &self.compose.project_dir),
        ] {
            if !p.is_absolute() {
                return bad(format!("{name} must be an absolute path"));
            }
        }
        // Written unquoted into release.env as AQ_UPDATER_SPOOL.
        if !crate::runtime::is_env_safe_path(&crate::spool::spool_dir(&self.state_dir)) {
            return bad(
                "state_dir may use only A-Z a-z 0-9 . _ / + @ : - (its spool path is written into release.env)"
                    .into(),
            );
        }
        Ok(())
    }

    fn validate_update_settings(&self) -> Result<(), ConfigError> {
        let bad = |m: String| Err(ConfigError::Invalid(m));
        if self.compose.files.is_empty() {
            return bad("compose.files must not be empty".into());
        }
        if self.readiness_deadline_secs < 2 {
            return bad(
                "readiness_deadline_secs must be >= 2 (readiness is polled every 2 s)".into(),
            );
        }
        if self.backups_keep < 1 {
            return bad("backups_keep must be >= 1 (0 could prune the rollback backup)".into());
        }
        if self.expected_auth_mode.is_empty() {
            return bad("expected_auth_mode must not be empty".into());
        }
        if self.channel.is_empty() {
            return bad("channel must not be empty".into());
        }
        Ok(())
    }

    fn validate_repository(&self) -> Result<(), ConfigError> {
        let bad = |m: String| Err(ConfigError::Invalid(m));
        let repo = &self.release_repo;
        let last = repo.rsplit('/').next().unwrap_or("");
        let qualified_path = repo.contains('/') && !repo.contains('@');
        let untagged_name = !last.is_empty() && !last.contains(':');
        if !(qualified_path && untagged_name) {
            return bad(
                "release_repo must look like host[:port]/path with no tag or digest ('@', or ':' in the last segment)"
                    .into(),
            );
        }
        Ok(())
    }

    fn validate_verifier(&self) -> Result<(), ConfigError> {
        let bad = |m: String| Err(ConfigError::Invalid(m));
        #[allow(irrefutable_let_patterns)]
        if let VerifierConfig::CosignKeyless {
            identity_regexp, ..
        } = &self.verifier
        {
            if let Err(e) = regex::Regex::new(identity_regexp) {
                return bad(format!("verifier.identity_regexp does not compile: {e}"));
            }
            // cosign matches the regexp anywhere in the identity unless it
            // is anchored: an unanchored pattern would also accept, say, a
            // workflow in another repository whose URL contains this one.
            if !identity_regexp.starts_with('^') || !identity_regexp.ends_with('$') {
                return bad(
                    "verifier.identity_regexp must be anchored: start with ^ and end with $".into(),
                );
            }
        }
        Ok(())
    }
}
