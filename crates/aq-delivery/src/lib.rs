//! Central delivery primitives. Deliberately not mounted on a worker HTTP router;
//! only `git_grant` (spec 0016 §5.2) serves the server's `/v1/git` route.
//! Policy callers supply retained receipt IDs; this module resolves all source
//! OIDs and retention handles itself. Remote credentials remain inside aq-git.
pub mod artifact_files;
pub mod commands;
pub mod diff_patch;
pub mod git_grant;
pub mod github;
mod intent;
pub mod main_sync;
pub mod offer_source;
use intent::IntentRequest;
mod dispatch;
mod refs;
use refs::{branch, destination_key, oid};
mod policy;
mod protocol;
pub mod publication_evidence;
mod reconcile;
pub mod repository_onboarding;
pub mod result_patch;
pub mod result_work;
mod retained_source;
pub mod source_checks;
pub mod source_transport;
use aq_worker_protocol::{ObjectId, Validate};
pub use policy::{
    BranchDeletion, Checks, GitHubRepository, MergeMethod, MergeOutcome, MergeRefusal,
    Mergeability, PullRequest, PullRequestState, QueueWait,
};
pub use policy::{
    ChecksState, ClearFenceRequest, DeliverRequest, DeliveryOutcome, GitHubAdapter,
    PublicationMode, PublicationPolicy, PublicationRefusal, RecoveryBlocked, RefusalReason,
    UnconfiguredGitHub, WaitForChecks,
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
use std::path::{Path, PathBuf};
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
        let valid_ids = !self.operation_id.is_nil() && !self.repository_id.is_nil();
        if !valid_ids || !(1..=i64::MAX as u64).contains(&self.binding_generation) {
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
impl DeliveryService {
    /// `git_socket` is the configured aq-git socket (`SourceRetentionConfig::git_socket`).
    pub fn new(pool: PgPool, git_socket: &Path) -> Self {
        Self {
            pool,
            socket: git_socket.to_path_buf(),
        }
    }
    /// The caller inputs frozen with an operation's first intent, if any. A
    /// policy owner replays these exact inputs, so a retry under the same
    /// operation ID can never change a possibly dispatched payload.
    pub async fn frozen_inputs(&self, operation_id: Uuid) -> Result<Option<Value>> {
        Ok(
            sqlx::query_scalar("SELECT inputs FROM delivery_intents WHERE operation_id=$1")
                .bind(operation_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }
    /// Create from active, non-historical server-owned retained source records.
    /// Acceptance and delivery policy are the caller's responsibility.
    pub async fn create_candidate(&self, r: CandidateRequest) -> Result<Receipt<CandidateResult>> {
        let op = IntentRequest::new(&r.context, "candidate.create", &r)?;
        let (mut tx, old) = op.prepare(&self.pool).await?;
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
                sources.push(retained_source::load(&mut tx, &r.context, *id).await?);
            }
            op.freeze(&mut tx,
                json!({"base":r.base,"sources":sources,"commit":r.commit,"merge_profile":"git-ort-v1"})).await?
        };
        tx.commit().await?;
        self.execute(&op, intent).await
    }
    pub async fn publish(&self, r: PublishRequest) -> Result<Receipt<PublishResult>> {
        let op = IntentRequest::new(&r.context, "publish", &r)?;
        let (mut tx, old) = op.prepare(&self.pool).await?;
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
            let intent = op.freeze(&mut tx,
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
        self.execute(&op, intent).await
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
        let op = IntentRequest::new(&r.context, operation, &r)?;
        let (mut tx, old) = op.prepare(&self.pool).await?;
        let intent = if let Some(old) = old {
            old
        } else {
            if r.target_id.is_nil() {
                return Err(DeliveryError::InvalidInput("invalid observation target"));
            }
            op.freeze(&mut tx, json!({key:r.target_id})).await?
        };
        tx.commit().await?;
        self.execute(&op, intent).await
    }
}
