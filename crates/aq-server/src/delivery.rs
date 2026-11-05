//! Central delivery primitives. Deliberately not mounted on a worker HTTP router.
//! Policy callers supply retained receipt IDs; this module resolves all source
//! OIDs and retention handles itself. Remote credentials remain inside aq-git.
mod policy;
mod protocol;
mod reconcile;
use aq_worker_protocol::{
    CentralRetentionReceipt, ObjectId, SourceRole, TransferManifest, Validate,
};
pub use policy::{
    ChecksState, ClearFenceRequest, DeliverRequest, DeliveryOutcome, GitHubAdapter, GitHubError,
    PublicationMode, PublicationPolicy, PublicationRefusal, PullRequest, PullRequestSpec,
    RecoveryBlocked, RefusalReason, WaitForChecks,
};
pub use protocol::{
    CandidateResult, CandidateState, CommitMetadata, ConflictPath, GitError, OperationResult,
    OperationStatus, PublishResult, Receipt, ReconcileResult, Resolution, RetainedSource,
    RetryAdvice,
};
use protocol::{WireResult, decode};
pub use reconcile::{ReconcileRequest, ResolveDisposition, ResolveRequest};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::path::PathBuf;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

type Result<T> = std::result::Result<T, DeliveryError>;
const CANDIDATE_NOT_READY: &str = "candidate is not ready";
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("idempotency_conflict")]
    IdempotencyConflict,
    #[error("operation_indeterminate: publication is never automatically redispatched")]
    Indeterminate,
    #[error("destination_blocked: unresolved publication")]
    DestinationBlocked,
    #[error("invalid delivery input: {0}")]
    InvalidInput(&'static str),
    #[error("invalid or incompatible aq-git response")]
    Protocol,
    #[error("aq-git transport unavailable; retry the same non-publication operation")]
    Transport,
    #[error("delivery database error")]
    Database(#[from] sqlx::Error),
    #[error("delivery serialization error")]
    Json(#[from] serde_json::Error),
}
impl From<std::io::Error> for DeliveryError {
    fn from(_: std::io::Error) -> Self {
        Self::Transport
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct OperationContext {
    pub operation_id: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
}
impl OperationContext {
    fn validate(&self) -> Result<()> {
        if self.operation_id.is_nil()
            || self.repository_id.is_nil()
            || self.binding_generation == 0
            || self.binding_generation > i64::MAX as u64
        {
            return Err(DeliveryError::InvalidInput("invalid operation context"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct CandidateRequest {
    pub context: OperationContext,
    pub base: ObjectId,
    pub source_receipts: Vec<Uuid>,
    pub commit: CommitMetadata,
}
#[derive(Debug, Clone, Serialize)]
pub struct PublishRequest {
    pub context: OperationContext,
    pub candidate_id: Uuid,
    pub target: String,
    pub expected_old: ObjectId,
}
/// Each observation UUID freezes one snapshot. Use a new UUID for a fresh read.
#[derive(Debug, Clone, Serialize)]
pub struct ObservationRequest {
    pub context: OperationContext,
    pub target_id: Uuid,
}
#[derive(Clone)]
pub struct DeliveryService {
    pool: PgPool,
    socket: PathBuf,
}
struct Intent {
    request: Vec<u8>,
    params: Value,
}
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<_, _> =
                map.iter().map(|(k, v)| (k.clone(), canonical(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}
fn digest(v: &Value) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(
        &canonical(v),
    )?)))
}
fn oid(v: &ObjectId, allow_zero: bool) -> Result<()> {
    if v.validate().is_err() || (!allow_zero && v.hex.bytes().all(|b| b == b'0')) {
        return Err(DeliveryError::InvalidInput("invalid typed OID"));
    }
    Ok(())
}
fn branch(target: &str) -> Result<()> {
    if !target.starts_with("refs/heads/")
        || target.ends_with('/')
        || target.ends_with('.')
        || target.contains("..")
        || target.contains("@{")
        || target.contains("//")
        || target
            .bytes()
            .any(|b| b <= 32 || b == 127 || b"~^:?*[\\".contains(&b))
        || target
            .split('/')
            .any(|p| p.is_empty() || p.starts_with('.') || p.ends_with(".lock"))
    {
        return Err(DeliveryError::InvalidInput(
            "invalid full publication branch",
        ));
    }
    Ok(())
}
// Fence case and canonically equivalent Unicode spellings conservatively;
// aq-git still receives and authorizes the caller's exact branch spelling.
fn destination_key(target: &str) -> String {
    target
        .nfc()
        .collect::<String>()
        .to_lowercase()
        .nfc()
        .collect()
}
impl DeliveryService {
    pub fn new(pool: PgPool, config: &crate::source_retention::SourceRetentionConfig) -> Self {
        Self {
            pool,
            socket: config.git_socket.clone(),
        }
    }
    // A transaction-scoped operation lock serializes freezing the original
    // payload, including when the row does not yet exist. Hash collisions only
    // serialize unrelated work; they never equate operation identities.
    async fn prepare(
        &self,
        context: &OperationContext,
        operation: &str,
        inputs: &Value,
    ) -> Result<(Transaction<'_, Postgres>, Option<Intent>)> {
        context.validate()?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 45))")
            .bind(context.operation_id.to_string())
            .execute(&mut *tx)
            .await?;
        let old = sqlx::query("SELECT input_digest,inputs,operation,request FROM delivery_intents WHERE operation_id=$1")
            .bind(context.operation_id).fetch_optional(&mut *tx).await?;
        let old = if let Some(row) = old {
            if row.get::<String, _>("input_digest") != digest(inputs)?
                || row.get::<Value, _>("inputs") != *inputs
                || row.get::<String, _>("operation") != operation
            {
                return Err(DeliveryError::IdempotencyConflict);
            }
            let request: Vec<u8> = row.get("request");
            let wire: Value = serde_json::from_slice(&request)?;
            Some(Intent {
                request,
                params: wire["params"].clone(),
            })
        } else {
            None
        };
        Ok((tx, old))
    }
    async fn freeze(
        tx: &mut Transaction<'_, Postgres>,
        c: &OperationContext,
        operation: &str,
        inputs: &Value,
        params: Value,
    ) -> Result<Intent> {
        let payload = json!({"operation":operation,"repository_id":c.repository_id,"expected_binding_generation":c.binding_generation,"params":params});
        let request = json!({"protocol_version":1,"request_id":c.operation_id.to_string(),"operation_id":c.operation_id,"operation":operation,"repository_id":c.repository_id,"expected_binding_generation":c.binding_generation,"params":params,"deadline_ms":30000});
        let mut bytes = serde_json::to_vec(&canonical(&request))?;
        bytes.push(b'\n');
        if bytes.len() > protocol::MAX_REQUEST {
            return Err(DeliveryError::InvalidInput("request exceeds 1 MiB"));
        }
        sqlx::query("INSERT INTO delivery_intents(operation_id,repository_id,binding_generation,operation,inputs,input_digest,payload_digest,request) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(c.operation_id).bind(c.repository_id).bind(c.binding_generation as i64).bind(operation).bind(inputs).bind(digest(inputs)?).bind(digest(&payload)?).bind(&bytes).execute(&mut **tx).await?;
        Ok(Intent {
            request: bytes,
            params,
        })
    }
    /// Create from active, non-historical server-owned retained source records.
    /// Acceptance and delivery policy are the caller's responsibility.
    pub async fn create_candidate(&self, r: CandidateRequest) -> Result<Receipt<CandidateResult>> {
        let inputs = serde_json::to_value(&r)?;
        let (mut tx, old) = self
            .prepare(&r.context, "candidate.create", &inputs)
            .await?;
        let intent = if let Some(old) = old {
            old
        } else {
            oid(&r.base, false)?;
            r.commit.validate()?;
            if r.source_receipts.is_empty() || r.source_receipts.len() > 64 {
                return Err(DeliveryError::InvalidInput(
                    "between 1 and 64 sources required",
                ));
            }
            let unique: std::collections::HashSet<_> = r.source_receipts.iter().collect();
            if unique.len() != r.source_receipts.len() {
                return Err(DeliveryError::InvalidInput("duplicate source receipts"));
            }
            let mut sources = Vec::with_capacity(r.source_receipts.len());
            for id in &r.source_receipts {
                sources.push(Self::source(&mut tx, &r.context, *id).await?);
            }
            Self::freeze(&mut tx, &r.context, "candidate.create", &inputs,
                json!({"base":r.base,"sources":sources,"commit":r.commit,"merge_profile":"git-ort-v1"})).await?
        };
        tx.commit().await?;
        self.execute(&r.context, "candidate.create", intent).await
    }
    async fn source(
        tx: &mut Transaction<'_, Postgres>,
        c: &OperationContext,
        id: Uuid,
    ) -> Result<RetainedSource> {
        let row = sqlx::query("SELECT transfer_id,attempt_id,repository_id,artifact_id,manifest,receipt,central_receipt FROM source_transfers WHERE import_operation_id=$1 AND state='retained' FOR SHARE")
            .bind(id).fetch_optional(&mut **tx).await?.ok_or(DeliveryError::InvalidInput("source is not retained"))?;
        let bad = || DeliveryError::InvalidInput("source owner acknowledgement mismatch");
        let m: TransferManifest = serde_json::from_value(row.get("manifest")).map_err(|_| bad())?;
        let receipt: CentralRetentionReceipt = serde_json::from_value(
            row.try_get::<Option<Value>, _>("receipt")?
                .ok_or_else(bad)?,
        )
        .map_err(|_| bad())?;
        let ack: Value = row
            .try_get::<Option<Value>, _>("central_receipt")?
            .ok_or_else(bad)?;
        m.validate().map_err(|_| bad())?;
        receipt.validate().map_err(|_| bad())?;
        if m.role != SourceRole::Checkpoint
            || row.get::<Uuid, _>("attempt_id") != m.attempt.attempt_id
            || row.get::<Uuid, _>("transfer_id") != m.transfer_id
            || row.get::<Uuid, _>("artifact_id") != m.artifact_id
            || row.get::<Uuid, _>("repository_id") != c.repository_id
            || m.source.repository_id != c.repository_id
            || m.source.binding_generation != c.binding_generation
            || receipt.receipt_id != id
            || receipt.transfer_id != m.transfer_id
            || receipt.repository_id != c.repository_id
            || receipt.binding_generation != c.binding_generation
            || receipt.source != m.checkpoint
            || receipt.retention_handle != m.artifact_id
            || ack["repository_id"] != c.repository_id.to_string()
            || ack["binding_generation"] != c.binding_generation
            || ack["source"] != json!(receipt.source)
            || ack["retention_handle"] != receipt.retention_handle.to_string()
            || ack["transfer_id"] != m.artifact_id.to_string()
            || ack["manifest_digest"] != receipt.manifest_digest
            || ack["retention_state"] != "active"
            || ack["historical"] != false
            || ack["receiver_installation_id"]
                .as_str()
                .and_then(|v| Uuid::parse_str(v).ok())
                .is_none_or(|v| v.is_nil())
        {
            return Err(bad());
        }
        oid(&receipt.source, false)?;
        Ok(RetainedSource {
            oid: receipt.source,
            retention_handle: receipt.retention_handle,
        })
    }
    pub async fn publish(&self, r: PublishRequest) -> Result<Receipt<PublishResult>> {
        let inputs = serde_json::to_value(&r)?;
        let (mut tx, old) = self.prepare(&r.context, "publish", &inputs).await?;
        let row = sqlx::query("SELECT i.request,r.response FROM delivery_intents i JOIN delivery_receipts r USING(operation_id) WHERE i.operation_id=$1 AND i.repository_id=$2 AND i.binding_generation=$3 AND i.operation='candidate.create'")
            .bind(r.candidate_id).bind(r.context.repository_id).bind(r.context.binding_generation as i64).fetch_optional(&mut *tx).await?
            .ok_or(DeliveryError::InvalidInput("candidate has no confirmed creation receipt"))?;
        let request: Value = serde_json::from_slice(&row.get::<Vec<u8>, _>("request"))?;
        let context = OperationContext {
            operation_id: r.candidate_id,
            ..r.context.clone()
        };
        let candidate: Receipt<CandidateResult> = decode(
            row.get("response"),
            &context,
            "candidate.create",
            &request["params"],
        )?;
        let candidate = candidate
            .outcome
            .ok()
            .filter(|v| v.state == CandidateState::Ready)
            .and_then(|v| v.candidate)
            .ok_or(DeliveryError::InvalidInput(CANDIDATE_NOT_READY))?;
        let mut intent = if let Some(old) = old {
            old
        } else {
            oid(&r.expected_old, true)?;
            branch(&r.target)?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 46))")
                .bind(r.context.repository_id.to_string())
                .execute(&mut *tx)
                .await?;
            let key = destination_key(&r.target);
            // Compare legacy holds as well: pre-canonicalization rows may have
            // stored mixed case or decomposed Unicode spellings.
            let held: Vec<String> = sqlx::query_scalar(
                "SELECT target FROM delivery_destinations WHERE repository_id=$1",
            )
            .bind(r.context.repository_id)
            .fetch_all(&mut *tx)
            .await?;
            if held.iter().any(|target| destination_key(target) == key) {
                return Err(DeliveryError::DestinationBlocked);
            }
            let intent = Self::freeze(&mut tx, &r.context, "publish", &inputs,
                json!({"candidate_id":r.candidate_id,"target":r.target,"expected_old":r.expected_old})).await?;
            let added = sqlx::query("INSERT INTO delivery_destinations(repository_id,target,operation_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                .bind(r.context.repository_id).bind(key).bind(r.context.operation_id).execute(&mut *tx).await?.rows_affected();
            if added != 1 {
                return Err(DeliveryError::DestinationBlocked);
            }
            intent
        };
        tx.commit().await?;
        // Local validation context only; never add fields to aq-git's finite
        // publish wire parameters. Reloaded on replay from immutable evidence.
        intent.params["candidate"] = serde_json::to_value(candidate)?;
        self.execute(&r.context, "publish", intent).await
    }
    pub async fn candidate_show(&self, r: ObservationRequest) -> Result<Receipt<CandidateResult>> {
        self.observe(r, "candidate.show", "candidate_id").await
    }
    pub async fn operation_show(&self, r: ObservationRequest) -> Result<Receipt<OperationResult>> {
        self.observe(r, "operation.show", "target_operation_id")
            .await
    }
    async fn observe<T: WireResult>(
        &self,
        r: ObservationRequest,
        operation: &str,
        key: &str,
    ) -> Result<Receipt<T>> {
        let inputs = serde_json::to_value(&r)?;
        let (mut tx, old) = self.prepare(&r.context, operation, &inputs).await?;
        let intent = if let Some(old) = old {
            old
        } else {
            if r.target_id.is_nil() {
                return Err(DeliveryError::InvalidInput("invalid observation target"));
            }
            Self::freeze(
                &mut tx,
                &r.context,
                operation,
                &inputs,
                json!({key:r.target_id}),
            )
            .await?
        };
        tx.commit().await?;
        self.execute(&r.context, operation, intent).await
    }
    async fn execute<T: WireResult>(
        &self,
        c: &OperationContext,
        operation: &str,
        intent: Intent,
    ) -> Result<Receipt<T>> {
        let mut tx = self.pool.begin().await?;
        let publish = operation == "publish";
        if publish {
            sqlx::query(
                "SELECT operation_id FROM delivery_intents WHERE operation_id=$1 FOR UPDATE",
            )
            .bind(c.operation_id)
            .fetch_one(&mut *tx)
            .await?;
        }
        if let Some(raw) = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT response FROM delivery_receipts WHERE operation_id=$1",
        )
        .bind(c.operation_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return decode(raw, c, operation, &intent.params);
        }
        if publish {
            let added = sqlx::query("INSERT INTO delivery_publish_dispatches(operation_id) VALUES($1) ON CONFLICT DO NOTHING").bind(c.operation_id).execute(&mut *tx).await?.rows_affected();
            tx.commit().await?;
            if added != 1 {
                return Err(DeliveryError::Indeterminate);
            }
            // Marker is committed before any bytes can be sent. Cancellation,
            // database failure, or process death leaves a durable destination hold.
            let raw = protocol::exchange(&self.socket, &intent.request)
                .await
                .map_err(|_| DeliveryError::Indeterminate)?;
            let parsed = decode::<T>(raw.clone(), c, operation, &intent.params);
            let mut tx = self.pool.begin().await?;
            Self::save_receipt(&mut tx, c.operation_id, &raw).await?;
            // After dispatch, missing uncertainty flags are not proof of refusal:
            // aq-git may have pushed before failing to write/read its journal.
            // Only validated success or an explicitly failed, non-uncertain
            // operation releases the hold. All other replies need reconciliation.
            let conclusive = parsed.as_ref().is_ok_and(|r| match &r.outcome {
                Ok(_) => true,
                Err(e) => !e.uncertain() && r.operation_state == Some(OperationStatus::Failed),
            });
            if conclusive {
                sqlx::query("DELETE FROM delivery_destinations WHERE operation_id=$1")
                    .bind(c.operation_id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            parsed
        } else {
            // Concurrent calls use aq-git's same-operation idempotency. Release
            // every row lock and pooled connection before bounded socket I/O;
            // the first persisted terminal reply becomes the immutable receipt.
            tx.commit().await?;
            let raw = protocol::exchange(&self.socket, &intent.request).await?;
            let parsed = decode::<T>(raw.clone(), c, operation, &intent.params);
            if operation == "candidate.create"
                && parsed
                    .as_ref()
                    .is_ok_and(|r| r.outcome.as_ref().is_err_and(GitError::retryable_candidate))
            {
                // A safe transient reply is an attempt outcome, not a terminal
                // receipt. The frozen intent survives for exact same-ID replay.
                return parsed;
            }
            let mut tx = self.pool.begin().await?;
            Self::save_receipt(&mut tx, c.operation_id, &raw).await?;
            let raw = sqlx::query_scalar::<_, Vec<u8>>(
                "SELECT response FROM delivery_receipts WHERE operation_id=$1",
            )
            .bind(c.operation_id)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            decode(raw, c, operation, &intent.params)
        }
    }
    async fn save_receipt(
        tx: &mut Transaction<'_, Postgres>,
        operation: Uuid,
        raw: &[u8],
    ) -> Result<()> {
        sqlx::query("INSERT INTO delivery_receipts(operation_id,response) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(operation)
            .bind(raw)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}
