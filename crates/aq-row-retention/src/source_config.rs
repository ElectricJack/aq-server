//! Configuration of result-source retention (`[source_retention]` in the server config).
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRetentionConfig {
    #[serde(default = "default_keep_per_task")]
    pub keep_per_task: usize,
    /// Seconds after settlement before even the newest handles may be released.
    #[serde(default = "default_max_age")]
    pub max_age: u64,
    pub staging_root: PathBuf,
    pub git_root: PathBuf,
    pub git_socket: PathBuf,
    #[serde(default = "default_quota")]
    pub max_staging_bytes: u64,
    #[serde(default = "default_bundle")]
    pub max_bundle_bytes: u64,
    #[serde(default = "default_concurrency")]
    pub bulk_concurrency: usize,
    #[serde(default = "default_ttl")]
    pub incomplete_ttl_secs: u64,
    /// One aq-git import or base export; a full bundle needs minutes (aq-git allows 300 s).
    #[serde(default = "default_git_deadline_ms")]
    pub git_deadline_ms: u64,
}
pub const fn default_keep_per_task() -> usize {
    2
}
pub const fn default_max_age() -> u64 {
    30 * 86400
}
fn default_quota() -> u64 {
    4 * 1024 * 1024 * 1024
}
fn default_bundle() -> u64 {
    1024 * 1024 * 1024
}
fn default_concurrency() -> usize {
    2
}
fn default_ttl() -> u64 {
    86400
}
fn default_git_deadline_ms() -> u64 {
    300_000
}
impl SourceRetentionConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.staging_root.is_absolute()
                && self.git_root.is_absolute()
                && self.git_socket.is_absolute(),
            "source retention paths must be absolute"
        );
        anyhow::ensure!(
            self.max_age > 0
                && self.max_age <= i64::MAX as u64 / 1000
                && self.bulk_concurrency > 0
                && self.bulk_concurrency <= 64
                && self.max_staging_bytes <= i64::MAX as u64
                && self.staging_root != Path::new("/")
                && self.git_root != Path::new("/")
                && !self.git_root.starts_with(&self.staging_root)
                && !self.staging_root.starts_with(&self.git_root)
                && self.max_bundle_bytes > 0
                && self.max_bundle_bytes <= 1024 * 1024 * 1024
                && self.max_staging_bytes > 0
                && self.incomplete_ttl_secs > 0
                && self.incomplete_ttl_secs <= 365 * 86400
                && (1_000..=300_000).contains(&self.git_deadline_ms),
            "invalid source retention limits"
        );
        for p in [&self.staging_root, &self.git_root, &self.git_socket] {
            anyhow::ensure!(
                !p.components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
                "source retention paths must not contain parent traversal"
            );
        }
        Ok(())
    }
}
