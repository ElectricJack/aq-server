//! Vault `config.toml` loading and validation.

use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const SUPPORTED_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, deserialize_with = "crate::playbooks::config::deserialize")]
    pub playbooks: Option<std::sync::Arc<crate::playbooks::config::Loaded>>,
    #[serde(default, deserialize_with = "crate::terminal::config::deserialize")]
    pub terminals: Option<std::sync::Arc<crate::terminal::config::Loaded>>,
    #[serde(
        default,
        deserialize_with = "crate::notifications::config::deserialize"
    )]
    pub notifications: Option<std::sync::Arc<crate::notifications::config::Loaded>>,
    #[serde(
        default,
        deserialize_with = "crate::intelligence::config::deserialize_registry"
    )]
    pub intelligence: Option<std::sync::Arc<crate::intelligence::config::LoadedRegistry>>,
    #[serde(
        default,
        deserialize_with = "crate::profiled_execution::config::deserialize"
    )]
    pub profiled_execution:
        Option<std::sync::Arc<crate::profiled_execution::config::ProfiledConfig>>,
    pub format_version: u32,
    pub environment: Environment,
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub bootstrap: BootstrapConfig,
    #[serde(default)]
    pub enrollment: EnrollmentConfig,
    #[serde(default)]
    pub workers: WorkersConfig,
    #[serde(default)]
    pub projects: crate::project_bindings::domain::ProjectConfig,
    /// Absent in development: the update API then answers `updates_unavailable`.
    #[serde(default)]
    pub updates: Option<UpdatesConfig>,
    #[serde(default)]
    pub agent_access: Option<crate::agent_access::AgentAccessConfig>,
    #[serde(default)]
    pub source_retention: Option<crate::source_retention::SourceRetentionConfig>,
    #[serde(default)]
    pub result_media: Option<crate::result_media::config::MediaConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatesConfig {
    /// Spool shared with the host updater: `requests/` (rw) and `status/` (ro).
    pub spool_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Development,
    Production,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub admin_bind: SocketAddr,
    pub worker_bind: SocketAddr,
    pub public_origin: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthConfig {
    Dev {
        dev_email: String,
    },
    CloudflareAccess {
        team_domain: String,
        audience: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    pub owner_email: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EnrollmentConfig {
    pub code_ttl_secs: u64,
}

impl Default for EnrollmentConfig {
    fn default() -> Self {
        Self { code_ttl_secs: 600 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkersConfig {
    pub heartbeat_interval_secs: u64,
    pub offline_after_secs: u64,
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_secs: 15,
            offline_after_secs: 60,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error(
        "AQ_DEPLOYMENT=production but the vault is not a production configuration: \
         environment is {environment:?} and auth.mode is {auth_mode:?}; \
         set environment = \"production\" and auth.mode = \"cloudflare_access\""
    )]
    NotProduction {
        environment: &'static str,
        auth_mode: &'static str,
    },
    #[error("invalid config {}: {key}: {reason}", path.display())]
    Invalid {
        path: PathBuf,
        key: String,
        reason: String,
    },
}

/// Reads and validates `vault_dir/config.toml`.
pub fn load(vault_dir: &Path) -> Result<Config, ConfigError> {
    let path = vault_dir.join("config.toml");
    let text = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let config: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.clone(),
        message: e.to_string(),
    })?;
    validate(&config).map_err(|(key, reason)| ConfigError::Invalid { path, key, reason })?;
    Ok(config)
}

fn validate(c: &Config) -> Result<(), (String, String)> {
    let invalid = |key: &str, reason: String| Err((key.to_string(), reason));
    if c.format_version != SUPPORTED_FORMAT_VERSION {
        return invalid(
            "format_version",
            format!(
                "unsupported format_version {}; this server reads {SUPPORTED_FORMAT_VERSION}",
                c.format_version
            ),
        );
    }
    let production = c.environment == Environment::Production;
    if production && matches!(c.auth, AuthConfig::Dev { .. }) {
        return invalid(
            "auth.mode",
            "dev authentication cannot be enabled in production".into(),
        );
    }
    if production && !c.server.public_origin.starts_with("https://") {
        return invalid(
            "server.public_origin",
            "must start with https:// in production".into(),
        );
    }
    if let AuthConfig::CloudflareAccess { team_domain, .. } = &c.auth
        && !team_domain.starts_with("https://")
    {
        return invalid("auth.team_domain", "must start with https://".into());
    }
    for (key, v) in [
        ("enrollment.code_ttl_secs", c.enrollment.code_ttl_secs),
        (
            "workers.heartbeat_interval_secs",
            c.workers.heartbeat_interval_secs,
        ),
        ("workers.offline_after_secs", c.workers.offline_after_secs),
    ] {
        if v < 1 {
            return invalid(key, "must be at least 1".into());
        }
    }
    if c.workers.offline_after_secs <= c.workers.heartbeat_interval_secs {
        return invalid(
            "workers.offline_after_secs",
            "must be greater than workers.heartbeat_interval_secs".into(),
        );
    }
    if let Some(u) = &c.updates
        && !u.spool_dir.is_absolute()
    {
        return invalid("updates.spool_dir", "must be an absolute path".into());
    }
    c.projects.validate().map_err(|e| ("projects".into(), e))?;
    if let Some(settings) = &c.result_media {
        settings
            .validate()
            .map_err(|reason| ("result_media".into(), reason.into()))?;
    }
    if let Some(settings) = &c.source_retention {
        settings
            .validate()
            .map_err(|e| ("source_retention".into(), e.to_string()))?;
    }
    if let Some(settings) = &c.agent_access {
        if !settings.graph_token_file.is_absolute() {
            return invalid(
                "agent_access.graph_token_file",
                "must be an absolute path".into(),
            );
        }
        if !(1000..=120_000).contains(&settings.credential_ttl_ms) {
            return invalid(
                "agent_access.credential_ttl_ms",
                "must be between 1000 and 120000".into(),
            );
        }
        let url = reqwest::Url::parse(&settings.graph_url).map_err(|_| {
            (
                "agent_access.graph_url".into(),
                "must be an HTTP origin".into(),
            )
        })?;
        if !matches!(url.scheme(), "http" | "https")
            || url.path() != "/"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return invalid(
                "agent_access.graph_url",
                "must be an HTTP origin without credentials".into(),
            );
        }
    }
    if c.bootstrap.enabled && c.bootstrap.owner_email.is_none() {
        return invalid(
            "bootstrap.owner_email",
            "required when bootstrap.enabled is true".into(),
        );
    }
    Ok(())
}

/// Refuses a vault that is not a production configuration when the process is
/// told it runs in production (`AQ_DEPLOYMENT=production`, set by compose.yaml).
/// This stops a development vault (dev auth) from being mounted behind a tunnel.
pub fn check_deployment(config: &Config, deployment: Option<&str>) -> Result<(), ConfigError> {
    if deployment != Some("production") {
        return Ok(());
    }
    let dev_auth = matches!(config.auth, AuthConfig::Dev { .. });
    if config.environment == Environment::Production && !dev_auth {
        return Ok(());
    }
    Err(ConfigError::NotProduction {
        environment: match config.environment {
            Environment::Development => "development",
            Environment::Production => "production",
        },
        auth_mode: if dev_auth { "dev" } else { "cloudflare_access" },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(env: Environment, dev: bool) -> Config {
        Config {
            playbooks: None,
            notifications: None,
            profiled_execution: None,
            format_version: 1,
            environment: env,
            server: ServerConfig {
                admin_bind: "127.0.0.1:8080".parse().unwrap(),
                worker_bind: "127.0.0.1:8081".parse().unwrap(),
                public_origin: "https://x".into(),
            },
            auth: if dev {
                AuthConfig::Dev {
                    dev_email: "a@b".into(),
                }
            } else {
                AuthConfig::CloudflareAccess {
                    team_domain: "https://t".into(),
                    audience: "a".into(),
                }
            },
            bootstrap: BootstrapConfig {
                owner_email: None,
                enabled: false,
            },
            enrollment: Default::default(),
            workers: Default::default(),
            projects: Default::default(),
            updates: None,
            agent_access: None,
            source_retention: None,
            result_media: None,
            terminals: None,

            intelligence: None,
        }
    }

    #[test]
    fn production_deployment_rejects_dev_vault() {
        let e = check_deployment(&cfg(Environment::Development, true), Some("production"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("development") && e.contains("dev"), "{e}");
    }

    #[test]
    fn production_deployment_rejects_dev_auth_in_production_env() {
        assert!(check_deployment(&cfg(Environment::Production, true), Some("production")).is_err());
    }

    #[test]
    fn production_deployment_rejects_development_env_with_access_auth() {
        assert!(
            check_deployment(&cfg(Environment::Development, false), Some("production")).is_err()
        );
    }

    #[test]
    fn production_deployment_accepts_production_vault() {
        check_deployment(&cfg(Environment::Production, false), Some("production")).unwrap();
    }

    #[test]
    fn other_deployments_accept_dev_vault() {
        for d in [None, Some("development"), Some("")] {
            check_deployment(&cfg(Environment::Development, true), d).unwrap();
        }
    }
}
