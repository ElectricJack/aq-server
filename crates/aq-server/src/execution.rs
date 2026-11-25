//! Durable execution authority. Transactions serialize per node; the partial
//! task index prevents conflicting attempts on different nodes. All network
//! effects occur outside these transactions. A lease expiry never proves stop.
use aq_worker_protocol::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{profiled_execution::journal::revoke_if, store::Store};

pub mod api;
pub mod authority;
pub mod profiled {
    pub use aq_profiled_state::*;
}
#[cfg(feature = "recovery-test-support")]
pub mod testing;
#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{1}")]
    Refused(ErrorCode, &'static str),
    #[error("execution storage failure")]
    Database(#[from] sqlx::Error),
    #[error("invalid execution state")]
    Json(#[from] serde_json::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Request handlers supply a live clock. Storage tests can explicitly freeze
/// time. Eligibility is sampled after database locks, never before queue waits.
#[derive(Clone)]
pub enum AdmissionClock {
    Frozen(u64),
    Live(std::sync::Arc<dyn Fn() -> u64 + Send + Sync>),
}
impl From<u64> for AdmissionClock {
    fn from(now: u64) -> Self {
        Self::Frozen(now)
    }
}
impl AdmissionClock {
    fn now_ms(&self) -> u64 {
        match self {
            Self::Frozen(now) => *now,
            Self::Live(clock) => clock(),
        }
    }
}

fn refuse(code: ErrorCode, message: &'static str) -> Error {
    Error::Refused(code, message)
}
fn validate<T: Validate>(value: &T) -> Result<()> {
    value
        .validate()
        .map_err(|_| refuse(ErrorCode::InvalidRequest, "invalid execution request"))
}
fn encode<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registered {
    identity: NodeIdentity,
    capabilities: Vec<String>,
    worker_version: String,
    #[serde(default)]
    replacement_boot: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native: Option<crate::profiled_execution::registration::NativeRegistration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityStatus {
    Offered,
    Granted,
    Cancelling,
    Finished,
    Declined,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptState {
    pub task_id: Uuid,
    pub offer: Offer,
    pub status: AuthorityStatus,
    pub grant: Option<Grant>,
    pub grant_expires_at_ms: u64,
    pub last_observation: Option<Observation>,
    pub report: Option<ResultReport>,
    pub disposition: Option<ResultResponse>,
    pub cancellation_id: Option<Uuid>,
    pub cancellation_reason: Option<String>,
    pub stopped: bool,
    /// Monotonic conservative evidence: later reports cannot claim nonstart
    /// after running, uncertain, or stopped execution was reported.
    #[serde(default)]
    pub may_have_started: bool,
}

impl AttemptState {
    fn grant_for_renewal(&self, request: &RenewRequest, now: u64) -> Result<Grant> {
        if self.offer.identity != request.identity
            || self.status != AuthorityStatus::Granted
            || self.stopped
        {
            return Err(refuse(ErrorCode::Cancelled, "attempt no longer authorized"));
        }
        if self.grant_expires_at_ms <= now {
            return Err(refuse(
                ErrorCode::GrantExpired,
                "expired grants cannot be renewed",
            ));
        }
        let mut grant = self
            .grant
            .clone()
            .ok_or_else(|| refuse(ErrorCode::InvalidRequest, "grant missing"))?;
        if request.grant_sequence != grant.grant_sequence {
            return Err(refuse(ErrorCode::StaleGeneration, "grant sequence changed"));
        }
        grant.grant_sequence = grant
            .grant_sequence
            .checked_add(1)
            .ok_or_else(|| refuse(ErrorCode::InvalidRequest, "grant sequence exhausted"))?;
        Ok(grant)
    }

    fn observe(&mut self, request: &Observation) -> Result<()> {
        if self.offer.identity != request.identity {
            return Err(refuse(
                ErrorCode::StaleBoot,
                "observation must name original boot",
            ));
        }
        if let Some(previous) = &self.last_observation {
            if request.sequence <= previous.sequence {
                return Err(refuse(
                    ErrorCode::IdConflict,
                    "observation sequence already used",
                ));
            }
            if phase_rank(request.phase) < phase_rank(previous.phase) {
                return Err(refuse(
                    ErrorCode::InvalidRequest,
                    "observation phase regressed",
                ));
            }
        }
        if self.stopped
            && request.stop_certainty != StopCertainty::ConfirmedStopped
            && request.stop_certainty != StopCertainty::NotStarted
        {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                "stopped execution cannot resume",
            ));
        }
        if request.stop_certainty == StopCertainty::ConfirmedStopped {
            self.stopped = true;
        }
        self.may_have_started |= observation_may_have_started(request);
        self.last_observation = Some(request.clone());
        Ok(())
    }

    fn validate_result(&self, request: &ResultReport) -> Result<()> {
        if self.offer.identity != request.identity
            || self.offer.spec.resolved_spec_digest != request.resolved_spec_digest
        {
            return Err(refuse(
                ErrorCode::StaleGeneration,
                "result binding mismatch",
            ));
        }
        if request.supersedes_report_id != self.report.as_ref().map(|r| r.report_id) {
            return Err(refuse(
                ErrorCode::IdConflict,
                "correction must identify previous report",
            ));
        }
        if self.report.is_none() && self.grant.is_none() {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                "result requires an accepted assignment",
            ));
        }
        Ok(())
    }

    fn validate_stop_evidence(&self, request: &ResultReport, stopped: bool) -> Result<()> {
        if self.stopped && !stopped {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                "confirmed stopped writers cannot become unresolved",
            ));
        }
        if self
            .disposition
            .as_ref()
            .is_some_and(|r| r.retention_release)
            && !request.pending_operation_ids.is_empty()
        {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                "released execution cannot introduce pending operations",
            ));
        }
        if request.stop_certainty == StopCertainty::NotStarted && self.may_have_started {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                "execution evidence cannot be downgraded to nonstart",
            ));
        }
        Ok(())
    }

    fn result_disposition(
        &self,
        identity: &NodeIdentity,
        stopped: bool,
        source_retained: bool,
        now: u64,
    ) -> ResultDisposition {
        if !stopped || !source_retained {
            ResultDisposition::Rejected
        } else if self.cancellation_id.is_some() {
            ResultDisposition::RetainedCancelled
        } else if *identity != self.offer.identity
            || now >= self.grant_expires_at_ms
            || self.status != AuthorityStatus::Granted
        {
            ResultDisposition::RetainedStale
        } else {
            ResultDisposition::Accepted
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GrantReceipt {
    grant: Grant,
    expires_at_ms: u64,
}

async fn lock_node(tx: &mut Transaction<'_, Postgres>, node: Uuid) -> Result<()> {
    let row =
        sqlx::query("SELECT revoked_at IS NOT NULL AS revoked FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(node)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| refuse(ErrorCode::InvalidCredential, "unknown node"))?;
    if row.get::<bool, _>("revoked") {
        return Err(refuse(ErrorCode::CredentialRevoked, "node revoked"));
    }
    Ok(())
}
async fn registration(tx: &mut Transaction<'_, Postgres>, node: Uuid) -> Result<Registered> {
    let body: String =
        sqlx::query_scalar("SELECT state::text FROM execution_nodes WHERE node_id=$1")
            .bind(node)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| refuse(ErrorCode::StaleBoot, "register execution incarnation first"))?;
    Ok(serde_json::from_str(&body)?)
}
async fn active(tx: &mut Transaction<'_, Postgres>, identity: &NodeIdentity) -> Result<Registered> {
    let record = registration(tx, identity.node_id).await?;
    if record.native.is_some() {
        return Err(refuse(
            ErrorCode::UnsupportedCapability,
            "native runtime requires profiled execution protocol",
        ));
    }
    if record.identity != *identity {
        return Err(refuse(
            ErrorCode::StaleBoot,
            "runtime incarnation is not active",
        ));
    }
    Ok(record)
}
fn observation_may_have_started(observation: &Observation) -> bool {
    observation.stop_certainty != StopCertainty::NotStarted
        || matches!(observation.phase, Phase::Running | Phase::LaunchUncertain)
}
async fn load_attempt(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<AttemptState> {
    let body: String = sqlx::query_scalar(
        "SELECT state::text FROM execution_attempts WHERE attempt_id=$1 AND kind='legacy_fixture' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| refuse(ErrorCode::NotFound, "attempt not found"))?;
    let raw: serde_json::Value = serde_json::from_str(&body)?;
    let legacy = raw.get("may_have_started").is_none();
    let mut state: AttemptState = serde_json::from_value(raw)?;
    state.may_have_started |= state
        .report
        .as_ref()
        .is_some_and(|r| r.stop_certainty != StopCertainty::NotStarted)
        || state
            .last_observation
            .as_ref()
            .is_some_and(observation_may_have_started);
    // Older projections could replace their latest report with a nonstart
    // correction. Immutable report bodies retain the earlier evidence. Consult
    // the indexed history only when upgrading a projection without this marker;
    // the next save persists the monotonic fact, avoiding a new per-call scan.
    // Observations retain only the latest body (receipts contain Ack, not input),
    // so there is no older observation body to reconstruct beyond that projection.
    if legacy && !state.may_have_started {
        state.may_have_started = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM execution_reports WHERE attempt_id=$1 AND request->>'stop_certainty' IS DISTINCT FROM 'not_started')",
        ).bind(id).fetch_one(&mut **tx).await?;
    }
    Ok(state)
}
fn bind_attempt(state: &AttemptState, node: Uuid, attempt: &AttemptRef) -> Result<()> {
    if state.offer.identity.node_id != node || state.offer.attempt != *attempt {
        return Err(refuse(
            ErrorCode::StaleGeneration,
            "attempt binding does not match",
        ));
    }
    Ok(())
}
async fn save_attempt(tx: &mut Transaction<'_, Postgres>, state: &AttemptState) -> Result<()> {
    let holds = match state.status {
        AuthorityStatus::Declined => false,
        AuthorityStatus::Finished => {
            !state.stopped
                || !state
                    .disposition
                    .as_ref()
                    .is_some_and(|r| r.retention_release)
                || state
                    .report
                    .as_ref()
                    .is_some_and(|r| !r.pending_operation_ids.is_empty())
        }
        _ => true,
    };
    sqlx::query("UPDATE execution_attempts SET state=$2::jsonb, holds_task=$3 WHERE attempt_id=$1 AND kind='legacy_fixture'")
        .bind(state.offer.attempt.attempt_id)
        .bind(encode(state)?)
        .bind(holds)
        .execute(&mut **tx)
        .await?;
    let s = state.stopped || state.cancellation_id.is_some(); // revokes the agent credential
    Ok(revoke_if(tx, state.offer.attempt.attempt_id, s).await?)
}

/// Source evidence is admitted from durable central receipts, never a worker's
/// assertion that its upload completed. Caller holds node -> attempt locks.
async fn retained_source(
    tx: &mut Transaction<'_, Postgres>,
    state: &AttemptState,
    report: &ResultReport,
) -> Result<bool> {
    let source = state
        .offer
        .spec
        .bindings
        .as_ref()
        .and_then(|b| b.source.as_ref());
    let Some(source) = source else {
        if !report.artifacts.is_empty() {
            return Err(refuse(
                ErrorCode::UnsupportedCapability,
                "attempt has no source binding",
            ));
        }
        return Ok(true);
    };
    let mut seen = std::collections::HashSet::new();
    for artifact in &report.artifacts {
        if !seen.insert(artifact.artifact_id) {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                "duplicate source artifact",
            ));
        }
        let row = sqlx::query("SELECT manifest::text AS manifest,receipt::text AS receipt FROM source_transfers WHERE artifact_id=$1 AND node_id=$2 AND attempt_id=$3 AND state='retained'")
            .bind(artifact.artifact_id).bind(report.identity.node_id).bind(report.attempt.attempt_id)
            .fetch_optional(&mut **tx).await?
            .ok_or_else(|| refuse(ErrorCode::InvalidRequest, "source retention is not confirmed"))?;
        let manifest: TransferManifest = serde_json::from_str(row.get("manifest"))?;
        let receipt: CentralRetentionReceipt = serde_json::from_str(row.get("receipt"))?;
        validate_retained_artifact(source, report, artifact, &manifest, &receipt)?;
    }
    let pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM source_transfers WHERE node_id=$1 AND attempt_id=$2 AND state IN ('uploading','import_pending'))")
        .bind(report.identity.node_id).bind(report.attempt.attempt_id).fetch_one(&mut **tx).await?;
    Ok(!pending
        && (report.stop_certainty == StopCertainty::NotStarted || !report.artifacts.is_empty()))
}
fn validate_retained_artifact(
    source: &SourceBinding,
    report: &ResultReport,
    artifact: &ArtifactRef,
    manifest: &TransferManifest,
    receipt: &CentralRetentionReceipt,
) -> Result<()> {
    validate(manifest)?;
    validate(receipt)?;
    if &manifest.source != source
        || manifest.role != SourceRole::Checkpoint
        || manifest.attempt != report.attempt
        || manifest.resolved_spec_digest != report.resolved_spec_digest
        || manifest.artifact_id != artifact.artifact_id
        || artifact.digest != manifest.manifest_digest
        || Some(artifact.bytes) != manifest.manifest_bytes.checked_add(manifest.bundle_bytes)
        || artifact.central_receipt.as_deref() != Some(receipt.receipt_id.to_string().as_str())
        || receipt.transfer_id != manifest.transfer_id
        || receipt.repository_id != manifest.source.repository_id
        || receipt.binding_generation != manifest.source.binding_generation
        || receipt.source != manifest.checkpoint
        || receipt.retention_handle != manifest.artifact_id
    {
        return Err(refuse(
            ErrorCode::InvalidRequest,
            "source receipt binding mismatch",
        ));
    }
    Ok(())
}

async fn receipt<T: DeserializeOwned>(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    operation: Uuid,
    kind: &str,
    digest: &str,
) -> Result<Option<T>> {
    let row = sqlx::query("SELECT kind,payload_digest,response::text AS response FROM execution_receipts WHERE node_id=$1 AND operation_id=$2")
        .bind(node).bind(operation).fetch_optional(&mut **tx).await?;
    let Some(row) = row else { return Ok(None) };
    if row.get::<String, _>("kind") != kind || row.get::<String, _>("payload_digest") != digest {
        return Err(refuse(
            ErrorCode::IdConflict,
            "operation ID reused with different request",
        ));
    }
    Ok(Some(serde_json::from_str(
        &row.get::<String, _>("response"),
    )?))
}
async fn record<T: Serialize>(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    operation: Uuid,
    kind: &str,
    digest: &str,
    response: &T,
) -> Result<()> {
    sqlx::query("INSERT INTO execution_receipts(node_id,operation_id,kind,payload_digest,response) VALUES($1,$2,$3,$4,$5::jsonb)")
        .bind(node).bind(operation).bind(kind).bind(digest).bind(encode(response)?).execute(&mut **tx).await?;
    Ok(())
}

async fn register_incarnation(
    tx: &mut Transaction<'_, Postgres>,
    request: &RegisterRequest,
    kind: &str,
    digest: &str,
) -> Result<RegisterResponse> {
    let body = Registered {
        identity: request.identity.clone(),
        capabilities: request.capabilities.clone(),
        worker_version: request.worker_version.clone(),
        replacement_boot: None,
        native: None,
    };
    sqlx::query("INSERT INTO execution_nodes(node_id,state) VALUES($1,$2::jsonb) ON CONFLICT(node_id) DO UPDATE SET state=excluded.state")
        .bind(request.identity.node_id).bind(encode(&body)?).execute(&mut **tx).await?;
    let response = RegisterResponse {
        identity: request.identity.clone(),
        protocol_version: EXECUTION_PROTOCOL_VERSION,
        capabilities: request.capabilities.clone(),
    };
    record(
        tx,
        request.identity.node_id,
        request.operation_id,
        kind,
        digest,
        &response,
    )
    .await?;
    Ok(response)
}
// All checks run under the node row lock. A recovery request cannot hide a
// writer by omitting it from the local inventory, and expiry is not stop proof.
async fn replacement_safe(tx: &mut Transaction<'_, Postgres>, node: Uuid) -> Result<bool> {
    // Bootstrap cannot establish profiled stop/source/effect ownership from a
    // receipt reference alone. Keep the node fenced until full verified release
    // clears every common claim; do not load unbounded launch manifests here.
    let held_profiled:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE node_id=$1 AND kind='profiled_v1' AND holds_task)").bind(node).fetch_one(&mut **tx).await?;
    if held_profiled {
        return Ok(false);
    }
    let unsafe_legacy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE node_id=$1 AND kind='legacy_fixture' AND holds_task AND state->>'status'<>'offered' AND NOT (state->>'stopped')::boolean)").bind(node).fetch_one(&mut **tx).await?;
    Ok(!unsafe_legacy)
}

async fn retire_unaccepted(tx: &mut Transaction<'_, Postgres>, node: Uuid) -> Result<()> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT state::text FROM execution_attempts WHERE node_id=$1 AND kind='legacy_fixture' AND state->>'status'='offered' FOR UPDATE",
    ).bind(node).fetch_all(&mut **tx).await?;
    for row in rows {
        let mut state: AttemptState = serde_json::from_str(&row)?;
        state.status = AuthorityStatus::Declined;
        state.stopped = true;
        save_attempt(tx, &state).await?;
    }
    Ok(())
}

fn remaining_grant(mut receipt: GrantReceipt, state: &AttemptState, now: u64) -> Result<Grant> {
    if state.status != AuthorityStatus::Granted || state.stopped {
        return Err(refuse(
            ErrorCode::Cancelled,
            "attempt is no longer authorized",
        ));
    }
    let remaining = receipt.expires_at_ms.saturating_sub(now);
    if remaining == 0 {
        return Err(refuse(
            ErrorCode::GrantExpired,
            "grant expired; reconcile instead of restarting",
        ));
    }
    receipt.grant.valid_for_ms = remaining.min(MAX_GRANT_MS);
    Ok(receipt.grant)
}

fn reconcile_attempt(
    state: Option<AttemptState>,
    current: &NodeIdentity,
    request: &ReconcileRequest,
    local: &ReconcileAttempt,
    clock: &AdmissionClock,
) -> Result<ReconcileDisposition> {
    let mut disposition = ReconcileDisposition {
        attempt: local.attempt.clone(),
        action: ReconcileAction::Unknown,
        grant: None,
        result: None,
    };
    let Some(state) = state else {
        return Ok(disposition);
    };
    let now = clock.now_ms();
    if state.offer.attempt != local.attempt
        || state.offer.identity.boot_id != local.previous_boot_id
    {
        return Ok(disposition);
    }
    if let Some(result) = &state.disposition {
        disposition.action = ReconcileAction::Acknowledged;
        disposition.result = Some(result.clone());
    } else if *current == request.identity
        && state.offer.identity == request.identity
        && state.status == AuthorityStatus::Granted
        && state.grant_expires_at_ms > now
    {
        if let Some(grant) = state.grant.clone() {
            disposition.action = ReconcileAction::Renewed;
            disposition.grant = Some(remaining_grant(
                GrantReceipt {
                    grant,
                    expires_at_ms: state.grant_expires_at_ms,
                },
                &state,
                now,
            )?);
        }
    } else {
        disposition.action = if matches!(
            local.stop_certainty,
            StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
        ) {
            ReconcileAction::AwaitReport
        } else {
            ReconcileAction::StopAndPreserve
        };
    }
    Ok(disposition)
}

impl Store {
    fn execution_grant_ttl(&self) -> u64 {
        #[cfg(feature = "recovery-test-support")]
        {
            self.recovery_test.grant_ttl_ms()
        }
        #[cfg(not(feature = "recovery-test-support"))]
        {
            MAX_GRANT_MS
        }
    }

    pub async fn execution_register(&self, request: &RegisterRequest) -> Result<RegisterResponse> {
        validate(request)?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT state::text FROM execution_nodes WHERE node_id=$1")
                .bind(request.identity.node_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(existing) = existing {
            let existing: Registered = serde_json::from_str(&existing)?;
            if existing.native.is_some() {
                let digest = payload_digest(request);
                if let Some(response) = receipt(
                    &mut tx,
                    request.identity.node_id,
                    request.operation_id,
                    "register",
                    &digest,
                )
                .await?
                {
                    return Ok(response);
                }
                return Err(refuse(
                    ErrorCode::UnsupportedCapability,
                    "legacy registration cannot replace native runtime",
                ));
            }
            if existing.identity != request.identity {
                if existing.replacement_boot != Some(request.identity.boot_id)
                    || !replacement_safe(&mut tx, request.identity.node_id).await?
                {
                    return Err(refuse(
                        ErrorCode::StaleBoot,
                        "incarnation replacement requires reconciliation and stopped writers",
                    ));
                }
                retire_unaccepted(&mut tx, request.identity.node_id).await?;
            }
        }
        let digest = payload_digest(request);
        if let Some(response) = receipt(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "register",
            &digest,
        )
        .await?
        {
            return Ok(response);
        }
        let response = register_incarnation(&mut tx, request, "register", &digest).await?;
        tx.commit().await?;
        Ok(response)
    }

    /// Admission used by the explicitly development-only fixture scheduler.
    /// Production graph scheduling must supply checked immutable inputs here.
    pub async fn execution_offer(&self, task_id: Uuid, offer: &Offer) -> Result<Offer> {
        validate(offer)?;
        if task_id.is_nil() {
            return Err(refuse(ErrorCode::InvalidRequest, "invalid task identity"));
        }
        if offer.spec.harness != FAKE_HARNESS_CAPABILITY {
            return Err(refuse(
                ErrorCode::UnsupportedCapability,
                "only deterministic fixture harness is supported",
            ));
        }
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, offer.identity.node_id).await?;
        let node = active(&mut tx, &offer.identity).await?;
        if offer
            .spec
            .required_capabilities
            .iter()
            .any(|c| !node.capabilities.contains(c))
        {
            return Err(refuse(
                ErrorCode::UnsupportedCapability,
                "node lacks required capability",
            ));
        }
        let existing: Option<String> =
            sqlx::query_scalar("SELECT state::text FROM execution_attempts WHERE attempt_id=$1 AND kind='legacy_fixture'")
                .bind(offer.attempt.attempt_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(existing) = existing {
            let existing: AttemptState = serde_json::from_str(&existing)?;
            if existing.task_id != task_id || existing.offer != *offer {
                return Err(refuse(ErrorCode::IdConflict, "attempt ID already used"));
            }
            return Ok(existing.offer);
        }
        let state = AttemptState {
            task_id,
            offer: offer.clone(),
            status: AuthorityStatus::Offered,
            grant: None,
            grant_expires_at_ms: 0,
            last_observation: None,
            report: None,
            disposition: None,
            cancellation_id: None,
            cancellation_reason: None,
            stopped: false,
            may_have_started: false,
        };
        let result = sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state) VALUES($1,$2,$3,$4,$5,$6::jsonb)")
            .bind(offer.attempt.attempt_id).bind(task_id).bind(offer.identity.node_id).bind(offer.offer_id).bind(offer.attempt.session_id).bind(encode(&state)?).execute(&mut *tx).await;
        if let Err(sqlx::Error::Database(ref e)) = result
            && e.is_unique_violation()
        {
            return Err(refuse(
                ErrorCode::IdConflict,
                "task, session or offer already has an unresolved assignment",
            ));
        }
        result?;
        tx.commit().await?;
        Ok(offer.clone())
    }

    pub async fn execution_poll(
        &self,
        request: &PollRequest,
        clock: impl Into<AdmissionClock>,
    ) -> Result<PollResponse> {
        let clock = clock.into();
        validate(request)?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        active(&mut tx, &request.identity).await?;
        let rows: Vec<String> = sqlx::query_scalar("SELECT state::text FROM execution_attempts WHERE node_id=$1 AND kind='legacy_fixture' AND state->>'status'='offered' ORDER BY attempt_id LIMIT $2")
            .bind(request.identity.node_id).bind(i64::from(request.max_items)).fetch_all(&mut *tx).await?;
        let mut offers = Vec::new();
        let mut available = request.available.clone();
        for row in rows {
            let mut state: AttemptState = serde_json::from_str(&row)?;
            let now = clock.now_ms();
            if state.offer.offer_expires_at_ms <= now {
                state.status = AuthorityStatus::Declined;
                state.stopped = true;
                save_attempt(&mut tx, &state).await?;
                continue;
            }
            let r = &state.offer.spec.resources;
            if state.offer.identity == request.identity
                && r.slots <= available.slots
                && r.cpu_millis <= available.cpu_millis
                && r.memory_bytes <= available.memory_bytes
                && r.disk_bytes <= available.disk_bytes
            {
                available.slots -= r.slots;
                available.cpu_millis -= r.cpu_millis;
                available.memory_bytes -= r.memory_bytes;
                available.disk_bytes -= r.disk_bytes;
                offers.push(state.offer);
            }
        }
        let rows: Vec<String> = sqlx::query_scalar("SELECT state::text FROM execution_directives WHERE node_id=$1 AND NOT acknowledged ORDER BY directive_id LIMIT $2")
            .bind(request.identity.node_id).bind(i64::from(request.max_items)).fetch_all(&mut *tx).await?;
        let directives = rows
            .iter()
            .map(|s| serde_json::from_str(s))
            .collect::<std::result::Result<Vec<Directive>, _>>()?;
        tx.commit().await?;
        Ok(PollResponse {
            offers,
            directives,
            server_time_ms: clock.now_ms(),
        })
    }

    pub async fn execution_accept(
        &self,
        request: &AcceptRequest,
        clock: impl Into<AdmissionClock>,
    ) -> Result<AcceptResponse> {
        let clock = clock.into();
        validate(request)?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        active(&mut tx, &request.identity).await?;
        let digest = payload_digest(request);
        let mut state = load_attempt(&mut tx, request.attempt.attempt_id).await?;
        bind_attempt(&state, request.identity.node_id, &request.attempt)?;
        let now = clock.now_ms();
        if state.offer.identity != request.identity
            || state.offer.offer_id != request.offer_id
            || state.offer.spec.resolved_spec_digest != request.resolved_spec_digest
        {
            return Err(refuse(ErrorCode::StaleGeneration, "offer binding changed"));
        }
        if let Some(receipt) = receipt(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "accept",
            &digest,
        )
        .await?
        {
            return Ok(AcceptResponse {
                grant: remaining_grant(receipt, &state, now)?,
            });
        }
        if state.status != AuthorityStatus::Offered {
            return Err(refuse(ErrorCode::IdConflict, "offer already resolved"));
        }
        if state.offer.offer_expires_at_ms <= now {
            return Err(refuse(ErrorCode::OfferExpired, "offer expired"));
        }
        if request.reservation_remaining_ms > state.offer.reservation_ttl_ms {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                "reservation exceeds offered TTL",
            ));
        }
        let grant = Grant {
            identity: request.identity.clone(),
            attempt: request.attempt.clone(),
            offer_id: request.offer_id,
            reservation_id: request.reservation_id,
            reservation_deadline_ms: request.reservation_deadline_ms,
            resolved_spec_digest: request.resolved_spec_digest.clone(),
            grant_sequence: 1,
            valid_for_ms: self.execution_grant_ttl(),
        };
        state.status = AuthorityStatus::Granted;
        state.grant_expires_at_ms = now.saturating_add(grant.valid_for_ms);
        state.grant = Some(grant.clone());
        save_attempt(&mut tx, &state).await?;
        record(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "accept",
            &digest,
            &GrantReceipt {
                grant: grant.clone(),
                expires_at_ms: state.grant_expires_at_ms,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(AcceptResponse { grant })
    }

    pub async fn execution_renew(
        &self,
        request: &RenewRequest,
        clock: impl Into<AdmissionClock>,
    ) -> Result<RenewResponse> {
        let clock = clock.into();
        validate(request)?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        active(&mut tx, &request.identity).await?;
        let mut state = load_attempt(&mut tx, request.attempt.attempt_id).await?;
        bind_attempt(&state, request.identity.node_id, &request.attempt)?;
        let now = clock.now_ms();
        let digest = payload_digest(request);
        if let Some(receipt) = receipt(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "renew",
            &digest,
        )
        .await?
        {
            return Ok(RenewResponse {
                grant: remaining_grant(receipt, &state, now)?,
            });
        }
        let mut grant = state.grant_for_renewal(request, now)?;
        grant.valid_for_ms = self.execution_grant_ttl();
        state.grant = Some(grant.clone());
        state.grant_expires_at_ms = now.saturating_add(grant.valid_for_ms);
        save_attempt(&mut tx, &state).await?;
        record(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "renew",
            &digest,
            &GrantReceipt {
                grant: grant.clone(),
                expires_at_ms: state.grant_expires_at_ms,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(RenewResponse { grant })
    }

    pub async fn execution_observe(&self, request: &Observation) -> Result<Ack> {
        validate(request)?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        let digest = payload_digest(request);
        if let Some(ack) = receipt(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "observe",
            &digest,
        )
        .await?
        {
            return Ok(ack);
        }
        let mut state = load_attempt(&mut tx, request.attempt.attempt_id).await?;
        bind_attempt(&state, request.identity.node_id, &request.attempt)?;
        state.observe(request)?;
        save_attempt(&mut tx, &state).await?;
        let ack = Ack {
            operation_id: request.operation_id,
        };
        record(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "observe",
            &digest,
            &ack,
        )
        .await?;
        tx.commit().await?;
        Ok(ack)
    }

    pub async fn execution_result(
        &self,
        request: &ResultReport,
        clock: impl Into<AdmissionClock>,
    ) -> Result<ResultResponse> {
        let clock = clock.into();
        validate(request)?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        let digest = payload_digest(request);
        if let Some(response) = receipt(
            &mut tx,
            request.identity.node_id,
            request.report_id,
            "result",
            &digest,
        )
        .await?
        {
            return Ok(response);
        }
        let mut state = load_attempt(&mut tx, request.attempt.attempt_id).await?;
        bind_attempt(&state, request.identity.node_id, &request.attempt)?;
        state.validate_result(request)?;
        let node = registration(&mut tx, request.identity.node_id).await?;
        let stopped = matches!(
            request.stop_certainty,
            StopCertainty::ConfirmedStopped | StopCertainty::NotStarted
        );
        state.validate_stop_evidence(request, stopped)?;
        let source_retained = retained_source(&mut tx, &state, request).await?;
        if state
            .disposition
            .as_ref()
            .is_some_and(|r| r.retention_release)
            && !source_retained
        {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                "released execution cannot reacquire source retention",
            ));
        }
        let now = clock.now_ms();
        let disposition = state.result_disposition(&node.identity, stopped, source_retained, now);
        // This releases retained execution evidence only. Graph completion and
        // result policy remain separate AQ-10 operations.
        let response = ResultResponse {
            report_id: request.report_id,
            disposition,
            retention_release: stopped
                && source_retained
                && request.pending_operation_ids.is_empty(),
        };
        state.status = if stopped {
            AuthorityStatus::Finished
        } else {
            AuthorityStatus::Cancelling
        };
        state.stopped = stopped;
        state.may_have_started |= request.stop_certainty != StopCertainty::NotStarted;
        state.report = Some(request.clone());
        state.disposition = Some(response.clone());
        sqlx::query("INSERT INTO execution_reports(node_id,report_id,attempt_id,request,response) VALUES($1,$2,$3,$4::jsonb,$5::jsonb)")
            .bind(request.identity.node_id).bind(request.report_id).bind(request.attempt.attempt_id)
            .bind(encode(request)?).bind(encode(&response)?).execute(&mut *tx).await?;
        save_attempt(&mut tx, &state).await?;
        record(
            &mut tx,
            request.identity.node_id,
            request.report_id,
            "result",
            &digest,
            &response,
        )
        .await?;
        tx.commit().await?;
        Ok(response)
    }

    pub async fn execution_cancel(
        &self,
        attempt_id: Uuid,
        operation_id: Uuid,
        reason: &str,
    ) -> Result<AttemptState> {
        if operation_id.is_nil() || reason.is_empty() || reason.len() > MAX_TEXT {
            return Err(refuse(ErrorCode::InvalidRequest, "invalid cancellation"));
        }
        // Resolve node without locking the attempt first: all writers use the
        // same node-then-attempt lock order.
        let node: Uuid =
            sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1")
                .bind(attempt_id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or_else(|| refuse(ErrorCode::NotFound, "attempt not found"))?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, node).await?;
        let digest = payload_digest(&(attempt_id, reason));
        if let Some(state) = receipt(&mut tx, node, operation_id, "cancel", &digest).await? {
            return Ok(state);
        }
        let mut state = load_attempt(&mut tx, attempt_id).await?;
        if state.status != AuthorityStatus::Finished
            && state.status != AuthorityStatus::Declined
            && state.cancellation_id.is_none()
        {
            if state.status == AuthorityStatus::Offered {
                state.status = AuthorityStatus::Declined;
                state.stopped = true;
            } else {
                state.status = AuthorityStatus::Cancelling;
            }
            state.cancellation_id = Some(operation_id);
            state.cancellation_reason = Some(reason.to_owned());
            if state.grant.is_some() {
                let directive = Directive {
                    directive_id: operation_id,
                    attempt: state.offer.attempt.clone(),
                    kind: DirectiveKind::Cancel,
                };
                sqlx::query("INSERT INTO execution_directives(directive_id,node_id,attempt_id,state) VALUES($1,$2,$3,$4::jsonb)")
                .bind(operation_id).bind(node).bind(attempt_id).bind(encode(&directive)?).execute(&mut *tx).await?;
            }
            save_attempt(&mut tx, &state).await?;
        }
        record(&mut tx, node, operation_id, "cancel", &digest, &state).await?;
        tx.commit().await?;
        Ok(state)
    }

    /// Original immutable report and admission result, including superseded reports.
    pub async fn execution_report(
        &self,
        node: Uuid,
        report: Uuid,
    ) -> Result<(ResultReport, ResultResponse)> {
        let row = sqlx::query("SELECT request::text AS request,response::text AS response FROM execution_reports WHERE node_id=$1 AND report_id=$2")
            .bind(node).bind(report).fetch_optional(&self.pool).await?
            .ok_or_else(|| refuse(ErrorCode::NotFound, "report not found"))?;
        Ok((
            serde_json::from_str(&row.get::<String, _>("request"))?,
            serde_json::from_str(&row.get::<String, _>("response"))?,
        ))
    }

    pub async fn execution_get(&self, attempt_id: Uuid) -> Result<AttemptState> {
        let mut tx = self.pool.begin().await?;
        load_attempt(&mut tx, attempt_id).await
    }

    pub async fn execution_decline(&self, request: &DeclineRequest) -> Result<Ack> {
        validate(request)?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        let digest = payload_digest(request);
        if let Some(ack) = receipt(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "decline",
            &digest,
        )
        .await?
        {
            return Ok(ack);
        }
        let attempt_id: Uuid = sqlx::query_scalar(
            "SELECT attempt_id FROM execution_attempts WHERE offer_id=$1 AND node_id=$2",
        )
        .bind(request.offer_id)
        .bind(request.identity.node_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| refuse(ErrorCode::NotFound, "offer not found"))?;
        let mut state = load_attempt(&mut tx, attempt_id).await?;
        if state.offer.identity != request.identity {
            return Err(refuse(
                ErrorCode::StaleBoot,
                "decline must name the original offer incarnation",
            ));
        }
        if state.grant.is_some()
            || !matches!(
                state.status,
                AuthorityStatus::Offered | AuthorityStatus::Declined
            )
        {
            return Err(refuse(
                ErrorCode::IdConflict,
                "accepted assignment requires a result, not decline",
            ));
        }
        // This only removes authority: a terminal no-grant decline remains
        // acknowledgeable after expiry, cancellation or incarnation replacement.
        // The node lock fences a concurrent acceptance before releasing capacity.
        state.status = AuthorityStatus::Declined;
        state.stopped = true;
        save_attempt(&mut tx, &state).await?;
        let ack = Ack {
            operation_id: request.operation_id,
        };
        record(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "decline",
            &digest,
            &ack,
        )
        .await?;
        tx.commit().await?;
        Ok(ack)
    }

    pub async fn execution_directive_ack(&self, request: &DirectiveAck) -> Result<Ack> {
        validate(request)?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        let digest = payload_digest(request);
        if let Some(ack) = receipt(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "directive_ack",
            &digest,
        )
        .await?
        {
            return Ok(ack);
        }
        let body: String = sqlx::query_scalar(
            "SELECT state::text FROM execution_directives WHERE directive_id=$1 AND node_id=$2",
        )
        .bind(request.directive_id)
        .bind(request.identity.node_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| refuse(ErrorCode::NotFound, "directive not found"))?;
        let directive: Directive = serde_json::from_str(&body)?;
        if directive.attempt != request.attempt {
            return Err(refuse(
                ErrorCode::StaleGeneration,
                "directive binding changed",
            ));
        }
        let mut state = load_attempt(&mut tx, request.attempt.attempt_id).await?;
        if state.offer.identity != request.identity {
            return Err(refuse(
                ErrorCode::StaleBoot,
                "directive acknowledgement must name owning incarnation",
            ));
        }
        if request.disposition == DirectiveDisposition::Stopped {
            state.stopped = true;
            save_attempt(&mut tx, &state).await?;
        }
        sqlx::query("UPDATE execution_directives SET acknowledged=true WHERE directive_id=$1")
            .bind(request.directive_id)
            .execute(&mut *tx)
            .await?;
        let ack = Ack {
            operation_id: request.operation_id,
        };
        record(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "directive_ack",
            &digest,
            &ack,
        )
        .await?;
        tx.commit().await?;
        Ok(ack)
    }

    /// Reconciliation never extends a lease. It can permit an exact new boot
    /// to register after every centrally known prior writer is proven stopped.
    /// Registration rechecks that proof atomically before consuming the permit.
    pub async fn execution_reconcile(
        &self,
        request: &ReconcileRequest,
        clock: impl Into<AdmissionClock>,
    ) -> Result<ReconcileResponse> {
        let clock = clock.into();
        validate(request)?;
        let offset: usize = request
            .cursor
            .as_deref()
            .unwrap_or("0")
            .parse()
            .map_err(|_| refuse(ErrorCode::InvalidRequest, "invalid reconciliation cursor"))?;
        if offset > request.attempts.len() {
            return Err(refuse(ErrorCode::InvalidRequest, "cursor outside request"));
        }
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        let mut current = registration(&mut tx, request.identity.node_id).await?;
        if current.native.is_some() {
            return Err(refuse(
                ErrorCode::UnsupportedCapability,
                "legacy reconciliation cannot change native runtime",
            ));
        }
        if current.identity != request.identity {
            current.replacement_boot =
                if replacement_safe(&mut tx, request.identity.node_id).await? {
                    Some(request.identity.boot_id)
                } else {
                    None
                };
            sqlx::query("UPDATE execution_nodes SET state=$2::jsonb WHERE node_id=$1")
                .bind(request.identity.node_id)
                .bind(encode(&current)?)
                .execute(&mut *tx)
                .await?;
        }
        let end = (offset + request.max_items as usize).min(request.attempts.len());
        let mut attempts = Vec::new();
        for local in &request.attempts[offset..end] {
            let body: Option<String> = sqlx::query_scalar(
                "SELECT state::text FROM execution_attempts WHERE attempt_id=$1 AND node_id=$2 AND kind='legacy_fixture'",
            )
            .bind(local.attempt.attempt_id)
            .bind(request.identity.node_id)
            .fetch_optional(&mut *tx)
            .await?;
            let state = body.as_deref().map(serde_json::from_str).transpose()?;
            attempts.push(reconcile_attempt(
                state,
                &current.identity,
                request,
                local,
                &clock,
            )?);
        }
        tx.commit().await?;
        Ok(ReconcileResponse {
            attempts,
            next_cursor: (end < request.attempts.len()).then(|| end.to_string()),
        })
    }

    /// Owner-only incarnation replacement. No live/uncertain writer is cleared
    /// by time or by a new boot's registration request.
    pub async fn execution_replace_incarnation(
        &self,
        request: &RegisterRequest,
    ) -> Result<RegisterResponse> {
        validate(request)?;
        let mut tx = self.pool.begin().await?;
        lock_node(&mut tx, request.identity.node_id).await?;
        if registration(&mut tx, request.identity.node_id)
            .await?
            .native
            .is_some()
        {
            return Err(refuse(
                ErrorCode::UnsupportedCapability,
                "legacy replacement cannot change native runtime",
            ));
        }
        if !replacement_safe(&mut tx, request.identity.node_id).await? {
            return Err(refuse(
                ErrorCode::CapacityUnavailable,
                "old execution writers are not confirmed stopped",
            ));
        }
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT state::text FROM execution_attempts WHERE node_id=$1 AND kind='legacy_fixture' AND holds_task FOR UPDATE",
        )
        .bind(request.identity.node_id)
        .fetch_all(&mut *tx)
        .await?;
        for row in rows {
            let mut state: AttemptState = serde_json::from_str(&row)?;
            if state.status == AuthorityStatus::Offered {
                state.status = AuthorityStatus::Declined;
                state.stopped = true;
                save_attempt(&mut tx, &state).await?;
            } else if !state.stopped {
                return Err(refuse(
                    ErrorCode::CapacityUnavailable,
                    "old execution writers are not confirmed stopped",
                ));
            }
        }
        let digest = payload_digest(request);
        if let Some(response) = receipt(
            &mut tx,
            request.identity.node_id,
            request.operation_id,
            "replace",
            &digest,
        )
        .await?
        {
            return Ok(response);
        }
        let response = register_incarnation(&mut tx, request, "replace", &digest).await?;
        tx.commit().await?;
        Ok(response)
    }
}

fn phase_rank(phase: Phase) -> u8 {
    match phase {
        Phase::Reserved => 0,
        Phase::Authorized => 1,
        Phase::Preparing => 2,
        Phase::Running => 3,
        Phase::LaunchUncertain => 4,
        Phase::Stopping => 5,
        Phase::Collecting => 6,
        Phase::Reporting => 7,
        Phase::Acknowledged => 8,
        Phase::Retired => 9,
    }
}
