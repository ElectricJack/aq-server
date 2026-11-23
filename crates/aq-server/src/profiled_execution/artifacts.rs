//! Immutable nonsecret inline bytes with an actual database retention receipt.
//! Retain outside node/project transactions; linking never acquires quota locks.
use crate::{
    AppState,
    error::{AppError, db},
};
use aq_admission::{ArtifactKind, Digest, RetainedArtifact};
use aq_profiled_state::inline_artifact::receipt_ref;
pub use aq_profiled_state::inline_artifact::{LinkScope, OwnerKind, Receipt, Retain, Retained};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("inline artifact owner unavailable or inconsistent".into())
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())
}
fn decode(row: sqlx::postgres::PgRow) -> Result<Retained, AppError> {
    let id: Uuid = row.get("artifact_id");
    let descriptor: RetainedArtifact =
        serde_json::from_value(row.get("descriptor")).map_err(|_| refused())?;
    descriptor.validate().map_err(|_| refused())?;
    let bytes: Vec<u8> = row.get::<Option<Vec<u8>>, _>("body").ok_or_else(refused)?;
    let receipt: Vec<u8> = row
        .get::<Option<Vec<u8>>, _>("receipt_bytes")
        .ok_or_else(refused)?;
    let proof: Receipt = serde_json::from_slice(&receipt).map_err(|_| refused())?;
    let metadata = proof.matches_descriptor(&descriptor) && proof.matches_record(&row, bytes.len());
    if !metadata {
        return Err(refused());
    }
    let exact = receipt == proof.canonical_bytes().map_err(|_| AppError::Internal)?
        && descriptor.retention_receipt == receipt_ref(id, &receipt);
    if !exact {
        return Err(refused());
    }
    if descriptor.kind == ArtifactKind::Source {
        super::source::validate_body(&bytes, proof.project_id, descriptor.retained_at_ms)?;
    }
    descriptor
        .verify(&bytes, descriptor.retained_at_ms)
        .map_err(|_| refused())?;
    Ok(Retained {
        artifact_id: id,
        project_id: proof.project_id,
        owner: proof.owner_kind,
        descriptor,
        receipt,
        bytes,
    })
}
/// Revalidate both exact stored bytes and the immutable receipt. Caller owns
/// assignment/authorization and final time checks; this read grants neither.
pub async fn load_locked(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Retained, AppError> {
    let row = sqlx::query(
        "SELECT a.*,r.receipt_bytes FROM admission_artifacts a LEFT JOIN admission_inline_receipts r USING(artifact_id) WHERE a.artifact_id=$1 FOR SHARE OF a"
    )
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(refused)?;
    decode(row)
}
/// Independent transaction. No external I/O, eviction or fabricated source
/// bundle receipt. The caller must not hold node/project locks while retaining.
pub async fn retain(state: &AppState, request: Retain<'_>) -> Result<RetainedArtifact, AppError> {
    validate_request(&request)?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    let exists = lock_artifact(&mut tx, request.artifact_id).await?;
    if exists {
        let old = load_locked(&mut tx, request.artifact_id).await?;
        if !request.matches(&old) {
            return Err(refused());
        }
        old.descriptor
            .verify(&old.bytes, now(state)?)
            .map_err(|_| refused())?;
        tx.commit().await.map_err(db)?;
        return Ok(old.descriptor);
    }
    let time = now(state)?;
    fence_definition(&mut tx, &request).await?;
    if request.kind == ArtifactKind::Source {
        super::source::validate_body(request.bytes, request.project_id, time)?;
    }
    let descriptor = insert(&mut tx, &request, time).await?;
    tx.commit().await.map_err(|_| refused())?;
    Ok(descriptor)
}
fn validate_request(request: &Retain<'_>) -> Result<(), AppError> {
    if !request.valid() {
        return Err(refused());
    }
    if request.kind == ArtifactKind::Source {
        let source = super::source::validate_stored_body(request.bytes, request.project_id)?;
        let artifact =
            aq_delivery::source_checks::observation::artifact(request.bytes).ok_or_else(refused)?;
        let scope =
            request.owner == OwnerKind::SourceRetention && source.project_id == request.project_id;
        if !scope || artifact != request.artifact {
            return Err(refused());
        }
    }
    Ok(())
}
async fn lock_artifact(tx: &mut Tx<'_>, id: Uuid) -> Result<bool, AppError> {
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-inline-artifact-quota-v1',0))")
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admission_artifacts WHERE artifact_id=$1)")
            .bind(id)
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    Ok(exists)
}
async fn fence_definition(tx: &mut Tx<'_>, request: &Retain<'_>) -> Result<(), AppError> {
    // The trigger is the final database fence. Check under its same quota lock
    // first so poll callers receive a stable public reason instead of an opaque
    // insert conflict. Identity/version is global, just as in schema27.
    if request.kind == ArtifactKind::Definition {
        let changed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM admission_artifacts WHERE body IS NOT NULL AND descriptor->>'kind'='definition' AND descriptor#>>'{artifact,id}'=$1 AND descriptor#>>'{artifact,version}'=$2 AND (content_digest IS DISTINCT FROM $3 OR body IS DISTINCT FROM $4))"
        )
            .bind(request.artifact.id.as_str())
            .bind(request.artifact.version.as_str())
            .bind(request.artifact.digest.as_str())
            .bind(request.bytes)
            .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
        if changed {
            return Err(AppError::DefinitionIdentityConflict);
        }
    }
    Ok(())
}
async fn insert(
    tx: &mut Tx<'_>,
    request: &Retain<'_>,
    time: u64,
) -> Result<RetainedArtifact, AppError> {
    let proof = request.receipt(time);
    let receipt = proof.canonical_bytes().map_err(|_| AppError::Internal)?;
    let descriptor = proof.descriptor(&receipt);
    descriptor
        .verify(request.bytes, time)
        .map_err(|_| refused())?;
    if receipt.len() > 8192 {
        return Err(refused());
    }
    sqlx::query(
        "INSERT INTO admission_artifacts(artifact_id,descriptor,content_digest,byte_length,owner_kind,owner_key,body,created_at_ms,retain_until_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)"
    )
        .bind(request.artifact_id)
        .bind(serde_json::to_value(&descriptor)
        .map_err(|_|AppError::Internal)?)
        .bind(descriptor.artifact.digest.as_str())
        .bind(descriptor.bytes as i64)
        .bind(request.owner.name())
        .bind(request.project_id.to_string())
        .bind(request.bytes)
        .bind(time as i64)
        .bind(request.retain_until_ms as i64)
        .execute(&mut **tx)
        .await
        .map_err(|_|refused())?;
    sqlx::query(
        "INSERT INTO admission_inline_receipts(artifact_id,receipt_bytes,receipt_digest) VALUES($1,$2,$3)"
    )
    .bind(request.artifact_id)
    .bind(&receipt)
    .bind(Digest::of(&receipt).as_str())
    .execute(&mut **tx)
    .await
    .map_err(db)?;
    Ok(descriptor)
}
/// Caller holds the exact node/attempt admission locks. Existing artifacts are
/// immutable; final offer links only this project's actual retained bytes.
pub async fn link_locked(
    tx: &mut Transaction<'_, Postgres>,
    scope: LinkScope,
    artifacts: &[Uuid],
) -> Result<Vec<RetainedArtifact>, AppError> {
    let ids = scope.ordered_ids(artifacts).ok_or_else(refused)?;
    let mut descriptors = Vec::with_capacity(ids.len());
    for id in ids {
        let item = load_locked(tx, id).await?;
        if !scope.covers(&item) {
            return Err(refused());
        }
        item.descriptor
            .verify(&item.bytes, scope.required.start)
            .map_err(|_| refused())?;
        sqlx::query(
            "INSERT INTO profiled_launch_artifacts(attempt_id,artifact_id) VALUES($1,$2) ON CONFLICT DO NOTHING"
        )
        .bind(scope.attempt)
        .bind(id)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        descriptors.push(item.descriptor);
    }
    Ok(descriptors)
}
