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
    /// Bytes of staged transfers and pushed quarantines, over all attempts.
    #[serde(default = "default_quota")]
    pub max_staging_bytes: u64,
    /// One upload's bundle, one push, and one attempt's quarantine.
    #[serde(default = "default_bundle")]
    pub max_bundle_bytes: u64,
    /// Bulk transfers running at once. Uploads are refused beyond it; pushes wait.
    #[serde(default = "default_concurrency")]
    pub bulk_concurrency: usize,
    /// Age at which an incomplete upload or an abandoned quarantine is removed.
    /// It also bounds how long one push may take.
    #[serde(default = "default_ttl")]
    pub incomplete_ttl_secs: u64,
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
                && self.incomplete_ttl_secs <= 365 * 86400,
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
    /// Refuses symlinked or non-directory roots, then creates the staging root
    /// private to this process. Nothing is changed before the checks pass.
    pub fn prepare_roots(&self) -> anyhow::Result<()> {
        let config = self;
        if let Ok(meta) = std::fs::symlink_metadata(&config.git_root) {
            anyhow::ensure!(
                !meta.file_type().is_symlink() && meta.is_dir(),
                "symlink or non-directory git root refused"
            );
        }
        {
            let p = &config.staging_root;
            if let Ok(meta) = std::fs::symlink_metadata(p) {
                anyhow::ensure!(
                    !meta.file_type().is_symlink() && meta.is_dir(),
                    "symlink or non-directory staging root refused"
                );
            }
            std::fs::create_dir_all(p)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
            anyhow::ensure!(
                !std::fs::symlink_metadata(p)?.file_type().is_symlink(),
                "symlink staging root refused"
            );
        }
        Ok(())
    }
}
