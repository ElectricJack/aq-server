//! Sources backing newly issued and already-issued material leases.
use crate::{Error, HeadSource, Lineage, identity, lineage_locked, read, renewable, version_of};
use aq_admission::MaterialLease;
use aq_intelligence_config::LoadedRegistry;
use sqlx::{Postgres, Transaction};

async fn lineage_for(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    profile: &aq_selection::ArtifactRef,
    revision: u64,
) -> Result<Option<(Lineage, std::path::PathBuf)>, Error> {
    let Some(index) = loaded
        .credentials
        .iter()
        .position(|c| &c.profile == profile && c.revision == revision)
    else {
        return Ok(None);
    };
    let (Some(configured), Some(Some(base)), Some(dir)) = (
        loaded.config.credentials.get(index),
        loaded.fingerprints.get(index),
        loaded.config.material_store.as_ref(),
    ) else {
        return Ok(None);
    };
    if configured.kind != aq_selection::CredentialKind::SubscriptionLogin {
        return Ok(None);
    }
    let lineage = read(executor, &identity(profile), revision, base).await?;
    Ok(Some((lineage, dir.clone())))
}
/// Newest lineage revision for issuing the next attempt's lease; None means
/// the configured file is newest.
pub async fn newest_source(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    profile: &aq_selection::ArtifactRef,
    revision: u64,
) -> Result<Option<HeadSource>, Error> {
    Ok(lineage_for(executor, loaded, profile, revision)
        .await?
        .and_then(|(l, dir)| l.head_source(&dir)))
}
/// Lineage revision backing an issued lease; None means the lease was issued
/// from the configured file (its digest is checked against the file).
pub async fn lease_source(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    lease: &MaterialLease,
) -> Result<Option<HeadSource>, Error> {
    Ok(lease_versioned(executor, loaded, lease).await?.0)
}
/// [`lease_source`] plus the login version the lease was issued from (`None`
/// for credentials without a versioned login, or a digest no version names).
/// The node sends this version back as `base_version` on write-back.
pub async fn lease_versioned(
    executor: impl sqlx::PgExecutor<'_>,
    loaded: &LoadedRegistry,
    lease: &MaterialLease,
) -> Result<(Option<HeadSource>, Option<i64>), Error> {
    let Some((l, dir)) = lineage_for(
        executor,
        loaded,
        &lease.credential_profile,
        lease.credential_revision,
    )
    .await?
    else {
        return Ok((None, None));
    };
    let digest = lease.material_digest.as_str();
    let source = l
        .entries
        .iter()
        .rev()
        .find(|e| e.content_digest == digest)
        .map(|e| HeadSource::of(e, &dir));
    Ok((source, version_of(&l, digest)))
}

/// Renewal fence. An ApiKey credential keeps the original rule: the startup
/// fingerprint must equal the lease digest. A subscription-login lease stays
/// renewable across refreshed versions of the same credential (see
/// [`renewable`]).
pub async fn continuous_locked(
    tx: &mut Transaction<'_, Postgres>,
    loaded: &LoadedRegistry,
    index: usize,
    lease: &MaterialLease,
) -> Result<bool, Error> {
    let (Some(credential), Some(configured), Some(Some(base))) = (
        loaded.credentials.get(index),
        loaded.config.credentials.get(index),
        loaded.fingerprints.get(index),
    ) else {
        return Ok(false);
    };
    if configured.kind != aq_selection::CredentialKind::SubscriptionLogin {
        return Ok(base == lease.material_digest.as_str());
    }
    let lineage = lineage_locked(
        tx,
        &identity(&credential.profile),
        credential.revision,
        base,
    )
    .await?;
    Ok(renewable(&lineage, lease.material_digest.as_str()))
}
