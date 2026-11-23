//! Locks and persistence of an inline body, its canonical receipt and offer link.
use super::{Receipt, Retain, Retained, receipt_ref};
use aq_admission::{ArtifactKind, Digest, RetainedArtifact};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;

pub async fn set_timeouts(tx: &mut Tx<'_>) -> Result<(), sqlx::Error> {
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
/// Takes the inline quota lock; `Some(offer_staged)` when the artifact already exists.
pub async fn lock_retention(tx: &mut Tx<'_>, id: Uuid) -> Result<Option<bool>, sqlx::Error> {
    set_timeouts(tx).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-inline-artifact-quota-v1',0))")
        .execute(&mut **tx)
        .await?;
    // to_jsonb: stores before migration 0167 have no offer_staged column.
    sqlx::query_scalar("SELECT COALESCE((to_jsonb(a)->>'offer_staged')::boolean,false) FROM admission_artifacts a WHERE artifact_id=$1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
}
pub async fn row_locked(tx: &mut Tx<'_>, id: Uuid) -> Result<Option<PgRow>, sqlx::Error> {
    sqlx::query("SELECT a.*,r.receipt_bytes FROM admission_artifacts a LEFT JOIN admission_inline_receipts r USING(artifact_id) WHERE a.artifact_id=$1 FOR SHARE OF a").bind(id).fetch_optional(&mut **tx).await
}
struct Stored {
    proof: Receipt,
    descriptor: RetainedArtifact,
    receipt: Vec<u8>,
    bytes: Vec<u8>,
}
impl Stored {
    fn read(row: &PgRow) -> Option<Self> {
        let descriptor: RetainedArtifact = serde_json::from_value(row.get("descriptor")).ok()?;
        descriptor.validate().ok()?;
        let bytes = row.get::<Option<Vec<u8>>, _>("body")?;
        let receipt = row.get::<Option<Vec<u8>>, _>("receipt_bytes")?;
        let proof: Receipt = serde_json::from_slice(&receipt).ok()?;
        let matches =
            proof.matches_descriptor(&descriptor) && proof.matches_record(row, bytes.len());
        matches.then_some(Self {
            proof,
            descriptor,
            receipt,
            bytes,
        })
    }
}
impl Retained {
    pub fn decode(row: PgRow) -> Result<Option<Self>, serde_json::Error> {
        let Some(stored) = Stored::read(&row) else {
            return Ok(None);
        };
        let Stored {
            proof,
            descriptor,
            receipt,
            bytes,
        } = stored;
        let id = proof.artifact_id;
        let canonical = receipt == proof.canonical_bytes()?;
        let exact = canonical && descriptor.retention_receipt == receipt_ref(id, &receipt);
        Ok(exact.then_some(Self {
            artifact_id: id,
            project_id: proof.project_id,
            owner: proof.owner_kind,
            descriptor,
            receipt,
            bytes,
        }))
    }
}
impl Retain<'_> {
    /// Check under the quota lock so conflicts receive a stable public identity error.
    /// The database trigger still fences the global definition identity and version.
    pub async fn definition_conflict(&self, tx: &mut Tx<'_>) -> Result<bool, sqlx::Error> {
        if self.kind != ArtifactKind::Definition {
            return Ok(false);
        }
        let changed=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admission_artifacts WHERE body IS NOT NULL AND descriptor->>'kind'='definition' AND descriptor#>>'{artifact,id}'=$1 AND descriptor#>>'{artifact,version}'=$2 AND (content_digest IS DISTINCT FROM $3 OR body IS DISTINCT FROM $4))")
            .bind(self.artifact.id.as_str()).bind(self.artifact.version.as_str())
            .bind(self.artifact.digest.as_str()).bind(self.bytes)
            .fetch_one(&mut **tx).await?;
        Ok(changed)
    }
}
pub struct Record {
    descriptor: RetainedArtifact,
    value: Value,
    receipt: Vec<u8>,
}
impl Record {
    pub fn prepare(request: &Retain<'_>, time: u64) -> Result<Option<Self>, serde_json::Error> {
        let proof = request.receipt(time);
        let receipt = proof.canonical_bytes()?;
        let descriptor = proof.descriptor(&receipt);
        let valid = descriptor.verify(request.bytes, time).is_ok() && receipt.len() <= 8192;
        if !valid {
            return Ok(None);
        }
        let value = serde_json::to_value(&descriptor)?;
        Ok(Some(Self {
            descriptor,
            value,
            receipt,
        }))
    }
    pub async fn insert_body(
        &self,
        tx: &mut Tx<'_>,
        request: &Retain<'_>,
        offer_staged: bool,
    ) -> Result<(), sqlx::Error> {
        // Unstaged rows omit the column, which stores before migration 0167 lack.
        let sql = if offer_staged {
            "INSERT INTO admission_artifacts(artifact_id,descriptor,content_digest,byte_length,owner_kind,owner_key,body,created_at_ms,retain_until_ms,offer_staged) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,true)"
        } else {
            "INSERT INTO admission_artifacts(artifact_id,descriptor,content_digest,byte_length,owner_kind,owner_key,body,created_at_ms,retain_until_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)"
        };
        sqlx::query(sql)
            .bind(request.artifact_id)
            .bind(&self.value)
            .bind(self.descriptor.artifact.digest.as_str())
            .bind(self.descriptor.bytes as i64)
            .bind(request.owner.name())
            .bind(request.project_id.to_string())
            .bind(request.bytes)
            .bind(self.descriptor.retained_at_ms as i64)
            .bind(request.retain_until_ms as i64)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
    pub async fn insert_receipt(
        self,
        tx: &mut Tx<'_>,
        id: Uuid,
    ) -> Result<RetainedArtifact, sqlx::Error> {
        sqlx::query("INSERT INTO admission_inline_receipts(artifact_id,receipt_bytes,receipt_digest) VALUES($1,$2,$3)")
            .bind(id)
            .bind(&self.receipt)
            .bind(Digest::of(&self.receipt).as_str())
            .execute(&mut **tx).await?;
        Ok(self.descriptor)
    }
}
pub async fn link(tx: &mut Tx<'_>, attempt: Uuid, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO profiled_launch_artifacts(attempt_id,artifact_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(attempt).bind(id).execute(&mut **tx).await?;
    Ok(())
}
pub async fn linked_id(
    tx: &mut Tx<'_>,
    attempt: Uuid,
    artifact: Value,
) -> Result<Option<Uuid>, sqlx::Error> {
    let rows=sqlx::query_scalar("SELECT a.artifact_id FROM profiled_launch_artifacts l JOIN admission_artifacts a USING(artifact_id) WHERE l.attempt_id=$1 AND a.descriptor->'artifact'=$2 LIMIT 2").bind(attempt).bind(artifact).fetch_all(&mut **tx).await?;
    Ok(match rows.as_slice() {
        [id] => Some(*id),
        _ => None,
    })
}
