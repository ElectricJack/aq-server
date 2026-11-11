//! Installed local admission for F1a. It is not a remote plan submission API.
use anyhow::{Result, ensure};
use aq_central_authority::{ArtifactRef, CentralPlan, Digest};
use aq_session_host::HostPlan;
use aq_session_protocol::ModelSettings;
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub database_url: String,
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub helper: PathBuf,
    pub tmux: PathBuf,
    pub tmux_version: String,
    pub native: PathBuf,
    pub native_digest: Digest,
    pub homes: PathBuf,
    /// Same protected-file placement primitive as CredentialConfig.secret_file.
    /// Only the guardian-owned transport reads provider bytes.
    pub secret_file: PathBuf,
    pub model: ModelSettings,
    pub central: CentralPlan,
    pub host: HostPlan,
    pub startup_file: PathBuf,
    /// Caller admission is not hard resource enforcement qualification.
    pub resource_admission: ArtifactRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<Subscription>,
}
pub fn protected_read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "protected path rejected"
    );
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.nlink() == 1
            && m.len() <= limit,
        "protected file rejected"
    );
    let mut b = Vec::new();
    f.take(limit + 1).read_to_end(&mut b)?;
    ensure!(b.len() as u64 <= limit, "protected input bound");
    Ok(b)
}
impl Config {
    pub fn load(path: &Path) -> Result<(Self, Digest)> {
        let bytes = protected_read(path, 65536)?;
        let config: Self = serde_json::from_slice(&bytes)?;
        crate::validate_join(&config.central, &config.host)?;
        ensure!(
            [
                &config.root,
                &config.workspace,
                &config.helper,
                &config.tmux,
                &config.native,
                &config.homes,
                &config.secret_file,
                &config.startup_file
            ]
            .iter()
            .all(|p| p.is_absolute()),
            "absolute installed paths required"
        );
        ensure!(
            !config.secret_file.starts_with(&config.workspace)
                && !config.root.starts_with(&config.workspace),
            "private owner files overlap workspace"
        );
        Ok((config, Digest::of(&bytes)))
    }
}
pub struct InstalledAuthority {
    pub path: PathBuf,
    pub digest: Digest,
}
#[async_trait::async_trait]
impl crate::PlanAuthority for InstalledAuthority {
    async fn authorize(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        c: &CentralPlan,
        h: &HostPlan,
    ) -> Result<()> {
        let (current, digest) = Config::load(&self.path)?;
        ensure!(
            digest == self.digest
                && current.central == *c
                && current.host.digest()? == h.digest()?,
            "installed admission changed"
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub server_origin: String,
    pub instructions: String,
    pub control_revision: u64,
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, serde_json::Value>,
}
