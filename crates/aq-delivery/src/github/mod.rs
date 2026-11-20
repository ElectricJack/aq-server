//! GitHub App adapter (spec 0016 §6.3): App authentication and a typed API
//! client. Mechanism only: errors come back typed (rate limits as
//! [`GitHubApiError::Retryable`] with the reset time) and the caller's
//! playbook decides what to do. No token is ever logged or put in an error.
mod auth;
mod client;
mod pulls;

pub use auth::{AppCredentials, AppJwtClaims, InstallationToken};
pub use client::{Auth, GitHubClient};

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

/// Vault `[delivery]` (parsed by the server config). An absent
/// `[delivery.github]` leaves the GitHub App adapter unavailable: building it
/// refuses with [`GitHubApiError::Unconfigured`].
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeliveryConfig {
    #[serde(default)]
    pub github: Option<GitHubAppConfig>,
    /// Projects whose new offers carry a Git grant. Empty leaves all projects on D1/D3.
    #[serde(default)]
    pub project_allowlist: BTreeSet<Uuid>,
    /// Optional intersection with Graph task IDs. None allows every task
    /// in the listed projects. An empty list enrolls no tasks.
    pub task_allowlist: Option<BTreeSet<Uuid>>,
    /// aq-git's git smart-HTTP socket (`aq-git serve --git-http-socket`).
    /// Also needed for existing grants after a project is removed from the allowlist.
    #[serde(default)]
    pub git_http_socket: Option<PathBuf>,
}
impl DeliveryConfig {
    pub fn enrolls(&self, project: Uuid, task: Uuid) -> bool {
        self.project_allowlist.contains(&project)
            && self
                .task_allowlist
                .as_ref()
                .is_none_or(|tasks| tasks.contains(&task))
    }

    pub fn has_enrollment(&self) -> bool {
        !self.project_allowlist.is_empty()
            && self
                .task_allowlist
                .as_ref()
                .is_none_or(|tasks| !tasks.is_empty())
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.github
            .as_ref()
            .map_or(Ok(()), GitHubAppConfig::validate)
    }
}
/// Server config `deserialize_with` for `[delivery]`: parses and validates,
/// so an invalid section fails config load naming `delivery.github`.
pub fn deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<DeliveryConfig>, D::Error> {
    let config = Option::<DeliveryConfig>::deserialize(d)?;
    let socket = config.as_ref().and_then(|c| c.git_http_socket.as_ref());
    if socket.is_some_and(|p| {
        !p.is_absolute() || p.components().any(|c| c == std::path::Component::ParentDir)
    }) {
        return Err(serde::de::Error::custom(
            "delivery.git_http_socket: must be an absolute path without `..`",
        ));
    }
    if let Some(c) = &config {
        if c.project_allowlist.iter().any(Uuid::is_nil) {
            return Err(serde::de::Error::custom(
                "delivery.project_allowlist: project IDs must be non-nil",
            ));
        }
        if c.task_allowlist.iter().flatten().any(Uuid::is_nil) {
            return Err(serde::de::Error::custom(
                "delivery.task_allowlist: Graph task IDs must be non-nil",
            ));
        }
        if c.has_enrollment() && c.git_http_socket.is_none() {
            return Err(serde::de::Error::custom(
                "delivery.project_allowlist requires delivery.git_http_socket",
            ));
        }
    }
    if let Some(Err(reason)) = config.as_ref().map(DeliveryConfig::validate) {
        return Err(serde::de::Error::custom(format!(
            "delivery.github: {reason}"
        )));
    }
    Ok(config)
}

/// Vault `[delivery.github]`. Absent means the adapter is unavailable
/// ([`GitHubApiError::Unconfigured`]).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitHubAppConfig {
    pub app_id: u64,
    /// PEM private key of the App, held in the vault. Read when the adapter
    /// is built, never at config load.
    pub private_key_file: PathBuf,
    pub installations: Vec<Installation>,
    #[serde(default = "default_api_base")]
    pub api_base: String,
    /// Per-request HTTP timeout.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
}
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    /// User or organisation login the App is installed on.
    pub owner: String,
    pub installation_id: u64,
}
fn default_api_base() -> String {
    "https://api.github.com".into()
}
fn default_request_timeout_secs() -> u64 {
    10
}

impl GitHubAppConfig {
    /// Static validation for config load; does not touch the key file.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.app_id == 0 {
            return Err("app_id must be a positive GitHub App id");
        }
        if !self.private_key_file.is_absolute() {
            return Err("private_key_file must be an absolute path");
        }
        if self.request_timeout_secs == 0 {
            return Err("request_timeout_secs must be at least 1");
        }
        client::parse_base(&self.api_base)?;
        let mut owners = std::collections::BTreeSet::new();
        for i in &self.installations {
            if !valid_login(&i.owner) {
                return Err("installations[].owner is not a GitHub login");
            }
            if i.installation_id == 0 {
                return Err("installations[].installation_id must be positive");
            }
            if !owners.insert(i.owner.to_ascii_lowercase()) {
                return Err("installations[].owner is listed twice");
            }
        }
        Ok(())
    }
}

/// GitHub logins: ASCII alphanumerics and single inner hyphens.
fn valid_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}
/// Repository names: ASCII alphanumerics, `-`, `_` and `.`, not `.`/`..`.
fn valid_repository(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitHubApiError {
    /// No `[delivery.github]` section: the adapter is unavailable.
    #[error("github_unconfigured: no [delivery.github] section in the vault")]
    Unconfigured,
    /// The configured App key cannot be read or is not an RSA PEM key.
    #[error("github_key_invalid: {0}")]
    Key(&'static str),
    /// No installation is configured for this owner.
    #[error("github_no_installation: no installation configured for owner {owner}")]
    NoInstallation { owner: String },
    #[error("github_invalid_input: {0}")]
    InvalidInput(&'static str),
    /// Primary or secondary rate limit (403/429). `reset_at` is GitHub's
    /// reported reset (`retry-after` or `x-ratelimit-reset`) when it gave one.
    #[error("github_retryable: rate limited until {reset_at:?}")]
    Retryable {
        reset_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    #[error("github_not_found")]
    NotFound,
    /// 422 with GitHub's message.
    #[error("github_invalid: {message}")]
    Invalid { message: String },
    /// Any other non-success status, with GitHub's message.
    #[error("github_status: {status}: {message}")]
    Status { status: u16, message: String },
    #[error("github_transport: {0}")]
    Transport(String),
    /// A success response whose body is not what the endpoint documents.
    #[error("github_protocol: {0}")]
    Protocol(String),
}
impl GitHubApiError {
    /// Stable machine code for receipts and playbook facts.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unconfigured => "github_unconfigured",
            Self::Key(_) => "github_key_invalid",
            Self::NoInstallation { .. } => "github_no_installation",
            Self::InvalidInput(_) => "github_invalid_input",
            Self::Retryable { .. } => "retryable",
            Self::NotFound => "not_found",
            Self::Invalid { .. } => "invalid",
            Self::Status { .. } => "github_status",
            Self::Transport(_) => "github_transport",
            Self::Protocol(_) => "github_protocol",
        }
    }
}
pub type GitHubResult<T> = std::result::Result<T, GitHubApiError>;

/// The configured GitHub App: API client, App credentials, owner →
/// installation map and the per-repository installation token cache.
pub struct GitHubApp {
    client: GitHubClient,
    credentials: AppCredentials,
    installations: BTreeMap<String, u64>,
    tokens: auth::TokenCache,
}
impl GitHubApp {
    /// Builds the adapter from the vault section, reading the key file.
    /// `None` refuses with [`GitHubApiError::Unconfigured`].
    pub fn from_config(config: Option<&GitHubAppConfig>) -> GitHubResult<Self> {
        let config = config.ok_or(GitHubApiError::Unconfigured)?;
        config.validate().map_err(GitHubApiError::InvalidInput)?;
        let credentials = AppCredentials::load(config.app_id, &config.private_key_file)?;
        let client = GitHubClient::new(
            &config.api_base,
            Duration::from_secs(config.request_timeout_secs),
        )?;
        Ok(Self::new(client, credentials, &config.installations))
    }
    pub fn new(
        client: GitHubClient,
        credentials: AppCredentials,
        installations: &[Installation],
    ) -> Self {
        Self {
            client,
            credentials,
            installations: installations
                .iter()
                .map(|i| (i.owner.to_ascii_lowercase(), i.installation_id))
                .collect(),
            tokens: auth::TokenCache::default(),
        }
    }
    pub fn client(&self) -> &GitHubClient {
        &self.client
    }
    pub fn installation_id(&self, owner: &str) -> GitHubResult<u64> {
        self.installations
            .get(&owner.to_ascii_lowercase())
            .copied()
            .ok_or_else(|| GitHubApiError::NoInstallation {
                owner: owner.into(),
            })
    }
    /// An installation access token limited to `owner/repository`, from the
    /// cache while it is more than 60 s from `expires_at`, else freshly minted.
    pub async fn installation_token(
        &self,
        owner: &str,
        repository: &str,
    ) -> GitHubResult<InstallationToken> {
        if !valid_login(owner) || !valid_repository(repository) {
            return Err(GitHubApiError::InvalidInput("invalid owner or repository"));
        }
        let installation = self.installation_id(owner)?;
        self.tokens
            .get_or_mint(installation, repository, || {
                auth::mint(&self.client, &self.credentials, installation, repository)
            })
            .await
    }
}

#[cfg(test)]
mod tests;
