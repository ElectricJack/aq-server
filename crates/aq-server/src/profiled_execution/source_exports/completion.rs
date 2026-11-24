//! Completion consumes the actual private source owner's typed fact. Historical
//! completion may be recorded after freshness expires; only fresh inspections
//! can link bytes into a new offer. Neither path grants execution.
use super::{ReservedExport, canonical, database, unavailable};
use crate::{AppState, error::AppError, source_retention::profiled_base::VerifiedExport};
use aq_admission::Digest;
use aq_worker_protocol::TransferManifest;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub async fn finalize(
    state: &AppState,
    token: &ReservedExport,
    fact: &VerifiedExport,
) -> Result<(), AppError> {
    fact.validate_for(token)?;
    let bytes = fact.canonical_bytes()?;
    let digest = Digest::of(&bytes);
    let charged = fact
        .bundle_bytes()
        .checked_add(fact.manifest_bytes().len() as u64)
        .ok_or_else(unavailable)?;
    if charged > token.reserved_bytes() {
        return Err(unavailable());
    }
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    use aq_history_caps::source_exports as storage;
    storage::set_timeouts(&mut tx).await.map_err(database)?;
    storage::lock_staging(&mut tx).await.map_err(database)?;
    let operation = token.request().operation_id;
    let row = storage::load(&mut tx, operation, storage::Access::ForUpdate)
        .await
        .map_err(database)?
        .ok_or_else(unavailable)?;
    if !row.matches(
        &canonical(token.request())?,
        token.request_digest().as_str(),
    ) || row.reserved_bytes != token.reserved_bytes()
    {
        return Err(unavailable());
    }
    let verified = storage::Verification {
        bytes: &bytes,
        digest: digest.as_str(),
        charged_bytes: charged,
    };
    if row.state == "verified" {
        if !verified.matches(&row) {
            return Err(unavailable());
        }
    } else {
        verified.save(&mut tx, operation).await.map_err(database)?;
    }
    tx.commit().await.map_err(database)?;
    Ok(())
}
/// Immutable stored facts only, never cached/current execution authorization.
pub struct LinkedExport {
    token: ReservedExport,
    fact: VerifiedExport,
    manifest: TransferManifest,
}
impl LinkedExport {
    pub(crate) fn into_parts(self) -> (ReservedExport, VerifiedExport) {
        (self.token, self.fact)
    }
    pub fn token(&self) -> &ReservedExport {
        &self.token
    }
    pub fn fact(&self) -> &VerifiedExport {
        &self.fact
    }
    pub fn manifest(&self) -> &TransferManifest {
        &self.manifest
    }
}
pub async fn load_linked_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<LinkedExport, AppError> {
    use aq_profiled_state::source_export::stored::StoredLink;
    let stored = StoredLink::load_locked(tx, attempt)
        .await
        .map_err(database)?
        .ok_or_else(unavailable)?;
    let token = ReservedExport::from_encoded(
        stored.request().ok_or_else(unavailable)?,
        stored.reserved_bytes(),
    );
    let bytes = stored.verified_bytes().ok_or_else(unavailable)?;
    let fact = VerifiedExport::decode_stored(&token, &bytes)?;
    let manifest = stored.manifest().ok_or_else(unavailable)?;
    let identity = (manifest.attempt.attempt_id, manifest.artifact_id);
    if identity != (attempt, token.request().operation_id) || !matches_fact(&manifest, &fact) {
        return Err(unavailable());
    }
    Ok(LinkedExport {
        token,
        fact,
        manifest,
    })
}
fn matches_fact(manifest: &TransferManifest, fact: &VerifiedExport) -> bool {
    let digests = manifest.manifest_digest == fact.manifest_digest().as_str()
        && manifest.bundle_digest.as_deref() == Some(fact.bundle_digest().as_str());
    let size = (manifest.bundle_bytes, manifest.manifest_bytes);
    digests && size == (fact.bundle_bytes(), fact.manifest_bytes().len() as u64)
}
