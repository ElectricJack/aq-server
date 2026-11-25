//! Operator replacement of a versioned subscription login.
use crate::{Origin, append_locked, identity, lineage_locked, store};
use aq_intelligence_config::LoadedRegistry;

/// Why an operator upload was refused. Carries no secret bytes.
#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    #[error("no enabled subscription-login credential {0} at that revision")]
    UnknownCredential(String),
    #[error("[intelligence] material_store is not configured")]
    NoStore,
    #[error("the login must be 1..={} bytes", store::MAX_BYTES)]
    Size,
    #[error("database error")]
    Internal,
}

pub struct OperatorUpload<'a> {
    pub profile: &'a aq_selection::ArtifactRef,
    pub revision: u64,
    pub bytes: std::sync::Arc<zeroize::Zeroizing<Vec<u8>>>,
    pub now_ms: i64,
}
/// Operator import/replace: stores `bytes` as the newest version of the
/// credential's login and returns that version. Always wins over node
/// write-backs; live leases issued from older versions stop renewing.
pub async fn operator_replace(
    pool: &sqlx::PgPool,
    loaded: &LoadedRegistry,
    upload: OperatorUpload<'_>,
) -> Result<i64, ReplaceError> {
    let OperatorUpload {
        profile,
        revision,
        bytes,
        now_ms,
    } = upload;
    let name = identity(profile);
    if bytes.is_empty() || bytes.len() > store::MAX_BYTES {
        return Err(ReplaceError::Size);
    }
    let index = loaded
        .credentials
        .iter()
        .position(|c| &c.profile == profile && c.revision == revision)
        .ok_or_else(|| ReplaceError::UnknownCredential(name.clone()))?;
    let (Some(configured), Some(Some(base))) = (
        loaded.config.credentials.get(index),
        loaded.fingerprints.get(index),
    ) else {
        return Err(ReplaceError::UnknownCredential(name));
    };
    if configured.kind != aq_selection::CredentialKind::SubscriptionLogin
        || !loaded.credentials[index].enabled
    {
        return Err(ReplaceError::UnknownCredential(name));
    }
    let dir = loaded
        .config
        .material_store
        .as_ref()
        .ok_or(ReplaceError::NoStore)?;
    let internal = |_| ReplaceError::Internal;
    let mut tx = pool.begin().await.map_err(|_| ReplaceError::Internal)?;
    let lineage = lineage_locked(&mut tx, &name, revision, base)
        .await
        .map_err(internal)?;
    let (version, _) = append_locked(
        &mut tx,
        dir,
        &lineage,
        crate::NewRevision {
            origin: Origin::Operator,
            lease_id: None,
            bytes,
            now_ms,
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(|_| ReplaceError::Internal)?;
    Ok(version)
}
