//! Exact linkage between a profiled launch, its durable source export and its base.
use super::{EncodedRequest, Request};
use aq_admission::Digest;
use aq_worker_protocol::{TransferManifest, Validate};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;

impl Request {
    /// The caller has fenced the offer, repository, and export in this transaction.
    /// Returns false if the attempt already has a different export or base.
    pub async fn bind_base_locked(
        &self,
        tx: &mut Tx<'_>,
        node: Uuid,
        manifest: &TransferManifest,
    ) -> Result<bool, sqlx::Error> {
        let attempt = manifest.attempt.attempt_id;
        if !link_operation(tx, attempt, self.operation_id).await? {
            return Ok(false);
        }
        let existing=sqlx::query("SELECT node_id,operation_id,retention_handle,reserved_bytes,manifest,profiled_export_operation_id FROM source_bases WHERE attempt_id=$1 FOR UPDATE").bind(attempt).fetch_optional(&mut **tx).await?;
        let Ok(body) = serde_json::to_value(manifest) else {
            return Ok(false);
        };
        if let Some(old) = existing {
            let identity = (
                old.get::<Uuid, _>("node_id"),
                old.get::<Uuid, _>("operation_id"),
                old.get::<Uuid, _>("retention_handle"),
            );
            let scope = (node, self.operation_id, self.source_retention_handle);
            let accounting = old.get::<i64, _>("reserved_bytes") == 0
                && old.get::<Option<Uuid>, _>("profiled_export_operation_id")
                    == Some(self.operation_id);
            let exact =
                accounting && old.get::<Option<serde_json::Value>, _>("manifest") == Some(body);
            if identity != scope || !exact {
                return Ok(false);
            }
        } else {
            sqlx::query("INSERT INTO source_bases(attempt_id,node_id,operation_id,retention_handle,reserved_bytes,manifest,profiled_export_operation_id) VALUES($1,$2,$3,$4,0,$5,$3)").bind(attempt).bind(node).bind(self.operation_id).bind(self.source_retention_handle).bind(body).execute(&mut **tx).await?;
        }
        Ok(true)
    }
}
async fn link_operation(
    tx: &mut Tx<'_>,
    attempt: Uuid,
    operation: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query("INSERT INTO profiled_launch_source_exports(attempt_id,operation_id) VALUES($1,$2) ON CONFLICT(attempt_id) DO NOTHING").bind(attempt).bind(operation).execute(&mut **tx).await?;
    let linked: Uuid = sqlx::query_scalar(
        "SELECT operation_id FROM profiled_launch_source_exports WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&mut **tx)
    .await?;
    Ok(linked == operation)
}

pub struct StoredLink(PgRow);
impl StoredLink {
    pub async fn load_locked(tx: &mut Tx<'_>, attempt: Uuid) -> Result<Option<Self>, sqlx::Error> {
        Ok(sqlx::query("SELECT e.request_bytes,e.request_digest,e.reserved_bytes,e.state,e.verified_bytes,e.verified_digest,b.manifest,b.profiled_export_operation_id FROM profiled_launch_source_exports x JOIN profiled_source_exports e USING(operation_id) JOIN source_bases b ON b.attempt_id=x.attempt_id AND b.profiled_export_operation_id=e.operation_id WHERE x.attempt_id=$1 FOR SHARE OF e,b").bind(attempt).fetch_optional(&mut **tx).await?.map(Self))
    }
    pub fn request(&self) -> Option<EncodedRequest> {
        let row = &self.0;
        let bytes: Vec<u8> = row.get("request_bytes");
        let request: Request = serde_json::from_slice(&bytes).ok()?;
        let encoded = request.encode().ok()?;
        let exact = encoded.bytes == bytes
            && encoded.digest.as_str() == row.get::<String, _>("request_digest");
        (exact && row.get::<String, _>("state") == "verified").then_some(encoded)
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.0.get::<i64, _>("reserved_bytes") as u64
    }
    pub fn verified_bytes(&self) -> Option<Vec<u8>> {
        let bytes = self.0.get::<Option<Vec<u8>>, _>("verified_bytes")?;
        let digest = self.0.get::<Option<String>, _>("verified_digest")?;
        (Digest::of(&bytes).as_str() == digest).then_some(bytes)
    }
    pub fn manifest(&self) -> Option<TransferManifest> {
        let value = self.0.get::<Option<serde_json::Value>, _>("manifest")?;
        let manifest: TransferManifest = serde_json::from_value(value).ok()?;
        manifest.validate().ok()?;
        Some(manifest)
    }
}

/// A launch and its sealed source must both still be current after the export link's locks are acquired.
pub struct LaunchWindow {
    offer_expires: i64,
    admission: std::ops::Range<u64>,
}
impl LaunchWindow {
    pub async fn load(
        tx: &mut Tx<'_>,
        attempt: Uuid,
        request: &Request,
    ) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT execution_spec,offer_expires_at_ms FROM profiled_launches WHERE attempt_id=$1",
        )
        .bind(attempt)
        .fetch_one(&mut **tx)
        .await?;
        Ok(Self::decode(row, request))
    }
    fn decode(row: PgRow, request: &Request) -> Option<Self> {
        let offer_expires = row.get("offer_expires_at_ms");
        let mut value: serde_json::Value = row.get("execution_spec");
        value.sort_all_objects();
        let bytes = serde_json::to_vec(&value).ok()?;
        let spec = aq_admission::execution_spec::decode_profiled_spec(&bytes).ok()?;
        let source = spec.source;
        let scope =
            request.matches_admission_scope(&source) && request.matches_resolved_object(&source);
        scope.then_some(Self {
            offer_expires,
            admission: source.observed_at_ms..source.expires_at_ms,
        })
    }
    pub fn current(&self, now: u64, retention: std::ops::Range<u64>) -> bool {
        let offered = self.offer_expires > 0 && now < self.offer_expires as u64;
        let fresh = self.admission.contains(&now) && retention.contains(&now);
        now > 0 && offered && fresh
    }
}

/// The original owned operation can be read even after its current policy has retired.
pub struct OriginalRequest(Vec<u8>);
impl OriginalRequest {
    pub async fn load(
        pool: &sqlx::PgPool,
        operation: Uuid,
        project: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        let raw:Option<Vec<u8>>=sqlx::query_scalar("SELECT request_bytes FROM profiled_source_exports WHERE operation_id=$1 AND project_id=$2")
            .bind(operation).bind(project).fetch_optional(pool).await?;
        Ok(raw.map(Self))
    }
    pub fn into_request(self, operation: Uuid, project: Uuid) -> Option<Request> {
        if self.0.len() > 8192 {
            return None;
        }
        let request: Request = serde_json::from_slice(&self.0).ok()?;
        let matches = (request.operation_id, request.project_id) == (operation, project);
        matches.then_some(request)
    }
}

/// The verification is read under the same transaction as the launch and base links.
pub struct VerificationRow(PgRow);
impl VerificationRow {
    pub async fn load(tx: &mut Tx<'_>, operation: Uuid) -> Result<Option<Self>, sqlx::Error> {
        let row=sqlx::query("SELECT request_digest,verified_bytes,verified_digest,state FROM profiled_source_exports WHERE operation_id=$1 FOR SHARE")
            .bind(operation).fetch_optional(&mut **tx).await?;
        Ok(row.map(Self))
    }
    pub fn matches(&self, request_digest: &str, bytes: &[u8]) -> bool {
        let row = &self.0;
        let identity = row.get::<String, _>("state") == "verified"
            && row.get::<String, _>("request_digest") == request_digest;
        let same_bytes = row.get::<Option<Vec<u8>>, _>("verified_bytes").as_deref() == Some(bytes);
        let same_digest = row.get::<Option<String>, _>("verified_digest").as_deref()
            == Some(Digest::of(bytes).as_str());
        identity && same_bytes && same_digest
    }
}
