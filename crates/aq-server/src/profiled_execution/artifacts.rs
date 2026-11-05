//! Immutable nonsecret inline bytes with an actual database retention receipt.
//! Retain outside node/project transactions; linking never acquires quota locks.
use crate::{AppState, error::AppError};
use aq_admission::{ArtifactKind, ArtifactRef, Digest, RetainedArtifact, Text};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    Catalog,
    GraphSnapshot,
    Composition,
    Selection,
    Outcome,
    SourceRetention,
    Startup,
}
impl OwnerKind {
    fn name(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::GraphSnapshot => "graph_snapshot",
            Self::Composition => "composition",
            Self::Selection => "selection",
            Self::Outcome => "outcome",
            Self::SourceRetention => "source_retention",
            Self::Startup => "startup",
        }
    }
    fn accepts(self, kind: ArtifactKind) -> bool {
        matches!(
            (self, kind),
            (Self::Catalog, ArtifactKind::Definition)
                | (Self::GraphSnapshot, ArtifactKind::Context)
                | (Self::Composition, ArtifactKind::Composition)
                | (Self::Composition, ArtifactKind::Definition)
                | (Self::Selection, ArtifactKind::Selection)
                | (Self::Outcome, ArtifactKind::Outcome)
                | (Self::SourceRetention, ArtifactKind::Context)
                | (Self::SourceRetention, ArtifactKind::Source)
                | (Self::Startup, ArtifactKind::Context)
                | (Self::Startup, ArtifactKind::Definition)
        )
    }
}
/// Stable artifact_id is the retain operation identity. Changed reuse conflicts.
pub struct Retain<'a> {
    pub artifact_id: Uuid,
    pub project_id: Uuid,
    pub owner: OwnerKind,
    pub artifact: ArtifactRef,
    pub kind: ArtifactKind,
    pub bytes: &'a [u8],
    pub retain_until_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u32,
    pub artifact_id: Uuid,
    pub project_id: Uuid,
    pub owner_kind: OwnerKind,
    pub artifact: ArtifactRef,
    pub kind: ArtifactKind,
    pub bytes: u64,
    pub retained_at_ms: u64,
    pub retain_until_ms: u64,
}
pub struct Retained {
    pub artifact_id: Uuid,
    pub project_id: Uuid,
    pub owner: OwnerKind,
    pub descriptor: RetainedArtifact,
    pub receipt: Vec<u8>,
    pub bytes: Vec<u8>,
}
fn refused() -> AppError {
    AppError::Conflict("inline artifact owner unavailable or inconsistent".into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())
}
fn canonical(value: &impl Serialize) -> Result<Vec<u8>, AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
fn receipt_ref(id: Uuid, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(format!("aq.inline-retention/{id}")).expect("bounded UUID identity"),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    }
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
    if proof.schema_version != 1
        || proof.artifact_id != id
        || proof.project_id.is_nil()
        || proof.project_id.to_string() != row.get::<String, _>("owner_key")
        || proof.owner_kind.name() != row.get::<String, _>("owner_kind")
        || !proof.owner_kind.accepts(proof.kind)
        || proof.artifact != descriptor.artifact
        || proof.kind != descriptor.kind
        || proof.bytes != descriptor.bytes
        || proof.retained_at_ms != descriptor.retained_at_ms
        || proof.retain_until_ms != descriptor.retain_until_ms
        || proof.retained_at_ms != row.get::<i64, _>("created_at_ms") as u64
        || proof.retain_until_ms != row.get::<i64, _>("retain_until_ms") as u64
        || row.get::<i64, _>("byte_length") as u64 != bytes.len() as u64
        || row.get::<String, _>("content_digest") != descriptor.artifact.digest.as_str()
        || receipt != canonical(&proof)?
        || descriptor.retention_receipt != receipt_ref(id, &receipt)
    {
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
    let row=sqlx::query("SELECT a.*,r.receipt_bytes FROM admission_artifacts a LEFT JOIN admission_inline_receipts r USING(artifact_id) WHERE a.artifact_id=$1 FOR SHARE OF a")
        .bind(id).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(refused)?;
    decode(row)
}
/// Independent transaction. No external I/O, eviction or fabricated source
/// bundle receipt. The caller must not hold node/project locks while retaining.
pub async fn retain(state: &AppState, request: Retain<'_>) -> Result<RetainedArtifact, AppError> {
    if request.artifact_id.is_nil()
        || request.project_id.is_nil()
        || !request.owner.accepts(request.kind)
        || request.bytes.is_empty()
        || request.bytes.len() > 2_097_152
        || Digest::of(request.bytes) != request.artifact.digest
    {
        return Err(refused());
    }
    if request.kind == ArtifactKind::Source {
        let source = super::source::validate_stored_body(request.bytes, request.project_id)?;
        if request.owner != OwnerKind::SourceRetention
            || request.artifact.id.as_str()
                != format!(
                    "aq.source-admission-observation/{}",
                    Digest::of(request.bytes).as_str()
                )
            || request.artifact.version.as_str() != "1"
            || source.project_id != request.project_id
        {
            return Err(refused());
        }
    }
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-inline-artifact-quota-v1',0))")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admission_artifacts WHERE artifact_id=$1)")
            .bind(request.artifact_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
    if exists {
        let old = load_locked(&mut tx, request.artifact_id).await?;
        if old.project_id != request.project_id
            || old.owner != request.owner
            || old.descriptor.artifact != request.artifact
            || old.descriptor.kind != request.kind
            || old.descriptor.retain_until_ms != request.retain_until_ms
            || old.bytes != request.bytes
        {
            return Err(refused());
        }
        old.descriptor
            .verify(&old.bytes, now(state)?)
            .map_err(|_| refused())?;
        tx.commit().await.map_err(db)?;
        return Ok(old.descriptor);
    }
    let time = now(state)?;
    // The trigger is the final database fence. Check under its same quota lock
    // first so poll callers receive a stable public reason instead of an opaque
    // insert conflict. Identity/version is global, just as in schema27.
    if request.kind == ArtifactKind::Definition {
        let changed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admission_artifacts WHERE body IS NOT NULL AND descriptor->>'kind'='definition' AND descriptor#>>'{artifact,id}'=$1 AND descriptor#>>'{artifact,version}'=$2 AND (content_digest IS DISTINCT FROM $3 OR body IS DISTINCT FROM $4))")
            .bind(request.artifact.id.as_str())
            .bind(request.artifact.version.as_str())
            .bind(request.artifact.digest.as_str())
            .bind(request.bytes)
            .fetch_one(&mut *tx).await.map_err(db)?;
        if changed {
            return Err(AppError::DefinitionIdentityConflict);
        }
    }
    if request.kind == ArtifactKind::Source {
        super::source::validate_body(request.bytes, request.project_id, time)?;
    }
    let receipt = canonical(&Receipt {
        schema_version: 1,
        artifact_id: request.artifact_id,
        project_id: request.project_id,
        owner_kind: request.owner,
        artifact: request.artifact.clone(),
        kind: request.kind,
        bytes: request.bytes.len() as u64,
        retained_at_ms: time,
        retain_until_ms: request.retain_until_ms,
    })?;
    let descriptor = RetainedArtifact {
        artifact: request.artifact,
        kind: request.kind,
        bytes: request.bytes.len() as u64,
        retention_receipt: receipt_ref(request.artifact_id, &receipt),
        retained_at_ms: time,
        retain_until_ms: request.retain_until_ms,
    };
    descriptor
        .verify(request.bytes, time)
        .map_err(|_| refused())?;
    if receipt.len() > 8192 {
        return Err(refused());
    }
    sqlx::query("INSERT INTO admission_artifacts(artifact_id,descriptor,content_digest,byte_length,owner_kind,owner_key,body,created_at_ms,retain_until_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(request.artifact_id).bind(serde_json::to_value(&descriptor).map_err(|_|AppError::Internal)?).bind(descriptor.artifact.digest.as_str()).bind(descriptor.bytes as i64).bind(request.owner.name()).bind(request.project_id.to_string()).bind(request.bytes).bind(time as i64).bind(request.retain_until_ms as i64).execute(&mut *tx).await.map_err(|_|refused())?;
    sqlx::query("INSERT INTO admission_inline_receipts(artifact_id,receipt_bytes,receipt_digest) VALUES($1,$2,$3)").bind(request.artifact_id).bind(&receipt).bind(Digest::of(&receipt).as_str()).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(|_| refused())?;
    Ok(descriptor)
}
/// Caller holds the exact node/attempt admission locks. Existing artifacts are
/// immutable; final offer links only this project's actual retained bytes.
pub async fn link_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    project: Uuid,
    artifacts: &[Uuid],
    now_ms: u64,
    required_until_ms: u64,
) -> Result<Vec<RetainedArtifact>, AppError> {
    if attempt.is_nil() || project.is_nil() || artifacts.len() > 128 || required_until_ms <= now_ms
    {
        return Err(refused());
    }
    let mut ids = artifacts.to_vec();
    ids.sort();
    ids.dedup();
    if ids.len() != artifacts.len() {
        return Err(refused());
    }
    let mut descriptors = Vec::with_capacity(ids.len());
    for id in ids {
        let item = load_locked(tx, id).await?;
        if item.project_id != project || item.descriptor.retain_until_ms < required_until_ms {
            return Err(refused());
        }
        item.descriptor
            .verify(&item.bytes, now_ms)
            .map_err(|_| refused())?;
        sqlx::query("INSERT INTO profiled_launch_artifacts(attempt_id,artifact_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(attempt).bind(id).execute(&mut **tx).await.map_err(db)?;
        descriptors.push(item.descriptor);
    }
    Ok(descriptors)
}
