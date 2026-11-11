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
    /// Exact owner-selected catalog profile, retained with this generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_profile: Option<serde_json::Value>,
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
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
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
        admit_swarm(tx, &current).await?;
        Ok(())
    }
}

async fn cell(
    conn: &mut sqlx::PgConnection,
    digest: &str,
    profile: Option<&serde_json::Value>,
) -> Result<Option<aq_swarm_settings::admission::Cell>> {
    let Some(profile) = profile else {
        return Ok(None);
    };
    let reference = profile["profile_id"]
        .as_str()
        .zip(profile["profile_version"].as_u64())
        .map(|(id, version)| format!("{id}@{version}"));
    Ok(
        aq_swarm_settings::admission::resolve(conn, Some(digest), reference.as_deref(), profile)
            .await?,
    )
}

/// Only a new generation consumes capacity. All later lifecycle operations,
/// including stop and permit renewal, retain its original admission.
async fn admit_swarm(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    config: &Config,
) -> Result<()> {
    use aq_swarm_settings::admission;
    if !admission::available(tx).await? {
        return Ok(());
    }
    let plan = &config.central;
    let existing: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM central_session_generations WHERE actor_id=$1 AND generation=$2)")
        .bind(plan.subject.actor_id()).bind(plan.generation as i64).fetch_one(&mut **tx).await?;
    if existing {
        return Ok(());
    }
    let selected = cell(
        tx,
        plan.profiles.catalog.digest.as_str(),
        config.swarm_profile.as_ref(),
    )
    .await?;
    if let Some(selected) = &selected {
        ensure!(
            selected.provider == config.model.provider.as_str()
                && config
                    .swarm_profile
                    .as_ref()
                    .is_some_and(|p| p["model"] == config.model.model.as_str()),
            "supervisor profile differs from installed model"
        );
    }
    if let Some(reason) = admission::check(tx, selected.as_ref()).await? {
        anyhow::bail!("{reason}");
    }
    let provider = selected
        .as_ref()
        .map_or(config.model.provider.as_str(), |s| s.provider.as_str());
    let class = selected
        .as_ref()
        .map_or("__unclassified", |s| s.intelligence_class.as_str());
    // The generation FK is deferred until SessionOwner inserts its exact plan
    // in this transaction. A failed prepare rolls back this reservation too.
    sqlx::query("INSERT INTO swarm_supervisor_admissions(actor_id,generation,provider,intelligence_class) VALUES($1,$2,$3,$4)")
        .bind(plan.subject.actor_id()).bind(plan.generation as i64).bind(provider).bind(class).execute(&mut **tx).await?;
    Ok(())
}

/// A UI reason before expensive preparation; the final owning transaction
/// repeats the check because another machine may claim capacity meanwhile.
pub(crate) async fn swarm_wait(
    pool: &sqlx::PgPool,
    digest: &str,
    profile: Option<&serde_json::Value>,
) -> Result<Option<&'static str>> {
    let mut tx = pool.begin().await?;
    if !aq_swarm_settings::admission::available(&mut tx).await? {
        return Ok(None);
    }
    let selected = cell(&mut tx, digest, profile).await?;
    let reason = aq_swarm_settings::admission::check(&mut tx, selected.as_ref()).await?;
    tx.rollback().await?;
    Ok(reason)
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
