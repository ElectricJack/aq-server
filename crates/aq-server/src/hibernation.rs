//! H5 server side of CON-03 0.1.0 (`aq-lifecycle-protocol`): Suspend directives
//! bound to an H4 continuation's exact predecessor binding, the worker's
//! Checkpoint / ParkingReport / BootstrapAck packets, the explicit
//! protected-disk release directive, and the successor's ContinuationAttachment.
//! Receipts are deduplicated by `message_id` + `body_digest`. The worker's
//! reports are claims: they never complete or fail the task (see
//! [`suspended_hold`]), and a BootstrapAck counts only with the exact AQ
//! acknowledgment receipt H4 recorded for the attachment's admission.
use crate::{
    AppState,
    agent_access::waits::{self, WaitError},
    authz::RequireOwner,
    error::AppError,
    worker_api::AuthenticatedNode,
};
use aq_lifecycle_api::worker_lifecycle as lc;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, header},
    routing::{get, post},
};
use lc::{
    ArtifactRef, Binding, CheckpointClaim, ContinuationAttachment, ContinuationRef, Digest,
    Message, Packet, ParkingReport, Receipt, ReceiptDisposition, RemainingBudgets,
    SuspensionDisposition, SuspensionIntent, Text, Validate,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

type Result<T> = std::result::Result<T, AppError>;

/// CON-03 maximum suspension window and checkpoint budget.
pub const SUSPEND_WINDOW_MS: u64 = 300_000;
pub const MAX_SUMMARY_BYTES: u32 = 16_384;

fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn json_err(_: serde_json::Error) -> AppError {
    AppError::Internal
}
fn wait_err(error: WaitError) -> AppError {
    match error.0 {
        "internal" => AppError::Internal,
        "unauthorized" => AppError::Forbidden("attempt is not held by this node".into()),
        other => AppError::Forbidden(other.into()),
    }
}
fn text(s: impl Into<String>) -> Text {
    Text::new(s).expect("static hibernation text")
}
fn artifact(id: String, digest: Digest) -> ArtifactRef {
    ArtifactRef {
        id: text(id),
        version: text("1"),
        digest,
    }
}
fn to_json<T: Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(json_err)
}
fn from_json<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(json_err)
}
fn now(state: &AppState) -> u64 {
    crate::execution::api::now(state)
}
fn ms(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}
async fn begin(state: &AppState) -> Result<Transaction<'static, Postgres>> {
    state.store.pool.begin().await.map_err(db)
}
/// The default retention policy reference the server's Suspend directives name.
pub fn retention_policy() -> ArtifactRef {
    artifact(
        "aq-server-retention-policy-default".into(),
        Digest::of(b"aq-server-retention-policy-default:v1"),
    )
}
/// The exact current binding the node holds for `attempt`, re-derived from
/// authority (never from the packet).
async fn current_binding(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    attempt: Uuid,
) -> Result<Binding> {
    Ok(waits::load_caller(state, tx, node, attempt)
        .await
        .map_err(wait_err)?
        .fence
        .binding)
}

struct Row5 {
    wait_id: Uuid,
    intent: SuspensionIntent,
    phase: String,
    directive_packet: Packet,
    checkpoint: Option<lc::Checkpoint>,
    parking: Option<ParkingReport>,
    disk_release_directive: Option<ArtifactRef>,
    successor_attempt_id: Option<Uuid>,
    attachment: Option<ContinuationAttachment>,
    attachment_digest: Option<String>,
    continuation_packet: Option<Packet>,
}
async fn lock_suspension(
    tx: &mut Transaction<'_, Postgres>,
    continuation_id: Uuid,
) -> Result<Option<Row5>> {
    let Some(row) = sqlx::query("SELECT wait_id,intent,phase,directive_packet,checkpoint,parking,disk_release_directive,successor_attempt_id,attachment,attachment_digest,continuation_packet FROM hibernation_suspensions WHERE continuation_id=$1 FOR UPDATE")
        .bind(continuation_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
    else {
        return Ok(None);
    };
    let opt = |name: &str| -> Option<Value> { row.get::<Option<Value>, _>(name) };
    Ok(Some(Row5 {
        wait_id: row.get("wait_id"),
        intent: from_json(row.get("intent"))?,
        phase: row.get("phase"),
        directive_packet: from_json(row.get("directive_packet"))?,
        checkpoint: opt("checkpoint").map(from_json).transpose()?,
        parking: opt("parking").map(from_json).transpose()?,
        disk_release_directive: opt("disk_release_directive").map(from_json).transpose()?,
        successor_attempt_id: row.get("successor_attempt_id"),
        attachment: opt("attachment").map(from_json).transpose()?,
        attachment_digest: row.get("attachment_digest"),
        continuation_packet: opt("continuation_packet").map(from_json).transpose()?,
    }))
}

/// (1) Issue (or replay) the Suspend directive for a wait's continuation,
/// bound to the exact predecessor binding the wait was registered with.
pub async fn issue_suspension(state: &AppState, wait_id: Uuid) -> Result<Packet> {
    let mut tx = begin(state).await?;
    let wait = waits::continuation_of(&mut tx, wait_id)
        .await
        .map_err(wait_err)?
        .ok_or_else(|| AppError::NotFound("wait not found".into()))?;
    if let Some(existing) = lock_suspension(&mut tx, wait.continuation.continuation_id).await? {
        tx.commit().await.map_err(db)?;
        return Ok(existing.directive_packet);
    }
    if wait.terminal {
        return Err(AppError::Conflict("wait already has an outcome".into()));
    }
    let current = current_binding(state, &mut tx, wait.node_id, wait.binding.attempt_id).await?;
    if current != wait.binding {
        return Err(AppError::Conflict(
            "predecessor binding no longer current".into(),
        ));
    }
    let issued = now(state);
    let intent = SuspensionIntent {
        binding: wait.binding.clone(),
        continuation: wait.continuation.clone(),
        issued_at_ms: issued,
        deadline_ms: issued + SUSPEND_WINDOW_MS,
        max_summary_bytes: MAX_SUMMARY_BYTES,
        retention_policy: retention_policy(),
    };
    let packet = Packet::seal(Uuid::new_v4(), Message::Suspend(intent.clone()))
        .map_err(|_| AppError::Internal)?;
    let inserted = sqlx::query("INSERT INTO hibernation_suspensions(continuation_id,wait_id,predecessor_attempt_id,predecessor_node_id,intent,directive_packet,phase,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$5,$6,'directed',$7,$7)")
        .bind(wait.continuation.continuation_id)
        .bind(wait_id)
        .bind(wait.binding.attempt_id)
        .bind(wait.node_id)
        .bind(to_json(&intent)?)
        .bind(to_json(&packet)?)
        .bind(ms(issued))
        .execute(&mut *tx)
        .await;
    if let Err(sqlx::Error::Database(e)) = &inserted
        && e.is_unique_violation()
    {
        return Err(AppError::Conflict(
            "attempt already has an open suspension".into(),
        ));
    }
    inserted.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(packet)
}

/// (5) Explicitly issue the protected-disk release directive. Only after a
/// RetainedSuspended parking report; replays return the same directive.
pub async fn issue_disk_release(state: &AppState, continuation_id: Uuid) -> Result<ArtifactRef> {
    let mut tx = begin(state).await?;
    let row = lock_suspension(&mut tx, continuation_id)
        .await?
        .ok_or_else(|| AppError::NotFound("suspension not found".into()))?;
    if let Some(directive) = row.disk_release_directive {
        tx.commit().await.map_err(db)?;
        return Ok(directive);
    }
    let retained = row
        .parking
        .as_ref()
        .is_some_and(|p| p.disposition == SuspensionDisposition::RetainedSuspended);
    if !retained {
        return Err(AppError::Conflict(
            "protected disk release needs a retained suspension".into(),
        ));
    }
    let directive = artifact(
        format!("aq-disk-release-{continuation_id}"),
        Digest::of(
            format!(
                "aq-disk-release:{continuation_id}:{}",
                row.intent.binding.attempt_id
            )
            .as_bytes(),
        ),
    );
    sqlx::query("UPDATE hibernation_suspensions SET disk_release_directive=$2,updated_at_ms=$3 WHERE continuation_id=$1")
        .bind(continuation_id)
        .bind(to_json(&directive)?)
        .bind(ms(now(state)))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(directive)
}

/// (3) Build the successor grant's ContinuationAttachment once the wait has a
/// ready outcome. Retained receipts are exactly the parking report's
/// central receipts. Replays for the same successor return the same packet.
pub async fn attach_continuation(
    state: &AppState,
    continuation_id: Uuid,
    successor_node: Uuid,
    successor_attempt: Uuid,
    fresh_offer_digest: Digest,
    remaining: RemainingBudgets,
) -> Result<Packet> {
    let mut tx = begin(state).await?;
    let row = lock_suspension(&mut tx, continuation_id)
        .await?
        .ok_or_else(|| AppError::NotFound("suspension not found".into()))?;
    if let Some(packet) = row.continuation_packet {
        if row.successor_attempt_id != Some(successor_attempt) {
            return Err(AppError::Conflict(
                "continuation already attached to another successor".into(),
            ));
        }
        tx.commit().await.map_err(db)?;
        return Ok(packet);
    }
    let parking = row
        .parking
        .filter(|p| {
            row.phase == "parked_retained"
                && p.disposition == SuspensionDisposition::RetainedSuspended
        })
        .ok_or_else(|| AppError::Conflict("continuation needs a retained suspension".into()))?;
    let wait = waits::continuation_of(&mut tx, row.wait_id)
        .await
        .map_err(wait_err)?
        .ok_or(AppError::Internal)?;
    let outcome = wait
        .outcome
        .filter(|_| wait.intent_ready)
        .ok_or_else(|| AppError::Conflict("wait outcome not ready".into()))?;
    let successor = current_binding(state, &mut tx, successor_node, successor_attempt).await?;
    let pending_operation_ids = match row.checkpoint.as_ref().map(|c| &c.claim) {
        Some(CheckpointClaim::Supplied { summary, .. }) => summary.pending_operation_ids.clone(),
        _ => Vec::new(),
    };
    let attachment = ContinuationAttachment {
        admission_id: Uuid::new_v4(),
        continuation: row.intent.continuation.clone(),
        predecessor: row.intent.binding.clone(),
        successor,
        fresh_offer_digest,
        outcome,
        retained: parking.central_receipts.clone(),
        pending_operation_ids,
        held_task_message_ids: Vec::new(),
        remaining,
        native_resume: None,
    };
    attachment
        .validate()
        .map_err(|e| AppError::Conflict(format!("invalid continuation attachment: {e}")))?;
    let digest = lc::canonical_digest(&attachment).map_err(|_| AppError::Internal)?;
    let packet = Packet::seal(
        Uuid::new_v4(),
        Message::Continuation(Box::new(attachment.clone())),
    )
    .map_err(|_| AppError::Internal)?;
    sqlx::query("UPDATE hibernation_suspensions SET phase='continued',successor_attempt_id=$2,attachment=$3,attachment_digest=$4,continuation_packet=$5,updated_at_ms=$6 WHERE continuation_id=$1")
        .bind(continuation_id)
        .bind(successor_attempt)
        .bind(to_json(&attachment)?)
        .bind(digest.as_str())
        .bind(to_json(&packet)?)
        .bind(ms(now(state)))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(packet)
}

/// Directives pending for one attempt the authenticated node holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directives {
    pub suspend: Option<Packet>,
    pub disk_release: Option<ArtifactRef>,
    pub continuation: Option<Packet>,
}
pub async fn directives(
    state: &AppState,
    auth: &AuthenticatedNode,
    attempt: Uuid,
) -> Result<Directives> {
    crate::delegated_permits::runtime::require_attempts(auth, [attempt])?;
    let mut tx = begin(state).await?;
    let current = current_binding(state, &mut tx, auth.id(), attempt).await?;
    let mut out = Directives {
        suspend: None,
        disk_release: None,
        continuation: None,
    };
    if let Some(row) = sqlx::query("SELECT intent,directive_packet,disk_release_directive FROM hibernation_suspensions WHERE predecessor_attempt_id=$1 ORDER BY created_at_ms DESC LIMIT 1")
        .bind(attempt)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
    {
        let intent: SuspensionIntent = from_json(row.get("intent"))?;
        // Only the exact predecessor binding receives the directive.
        if intent.binding == current {
            out.suspend = Some(from_json(row.get("directive_packet"))?);
            out.disk_release = row
                .get::<Option<Value>, _>("disk_release_directive")
                .map(from_json)
                .transpose()?;
        }
    }
    if let Some(row) = sqlx::query(
        "SELECT attachment,continuation_packet FROM hibernation_suspensions WHERE successor_attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    {
        let attachment: ContinuationAttachment = from_json(row.get("attachment"))?;
        if attachment.successor == current {
            out.continuation = Some(from_json(row.get("continuation_packet"))?);
        }
    }
    tx.commit().await.map_err(db)?;
    Ok(out)
}

fn conflict(why: &str) -> AppError {
    AppError::Conflict(why.into())
}

/// (2) Record a worker packet and return its CON-03 receipt. A duplicate
/// message_id with the same body digest replays the original receipt; changed
/// bytes under the same message_id are a conflict.
pub async fn receive(
    state: &AppState,
    auth: &AuthenticatedNode,
    packet: &Packet,
) -> Result<Receipt> {
    packet
        .validate()
        .map_err(|e| AppError::BadRequest(format!("invalid lifecycle packet: {e}")))?;
    let (binding, continuation): (&Binding, &ContinuationRef) = match &packet.body.message {
        Message::Checkpoint(c) => (&c.binding, &c.continuation),
        Message::Parking(p) => (&p.binding, &p.continuation),
        Message::BootstrapAck(a) => (&a.successor, &a.continuation),
        _ => {
            return Err(AppError::BadRequest(
                "unsupported lifecycle message on this route".into(),
            ));
        }
    };
    crate::delegated_permits::runtime::require_attempts(auth, [binding.attempt_id])?;
    crate::delegated_permits::runtime::require_session(auth, &binding.session)?;
    let mut tx = begin(state).await?;
    let current = current_binding(state, &mut tx, auth.id(), binding.attempt_id).await?;
    if &current != binding {
        return Err(AppError::Forbidden("binding is not current".into()));
    }
    let message_id = packet.body.message_id;
    if let Some(row) =
        sqlx::query("SELECT body_digest,receipt FROM hibernation_receipts WHERE message_id=$1")
            .bind(message_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
    {
        if row.get::<String, _>("body_digest") != packet.body_digest.as_str() {
            return Err(conflict("message_id reused with a different body"));
        }
        let receipt = from_json(row.get("receipt"))?;
        tx.commit().await.map_err(db)?;
        return Ok(receipt);
    }
    let row = lock_suspension(&mut tx, continuation.continuation_id)
        .await?
        .ok_or_else(|| AppError::NotFound("suspension not found".into()))?;
    if &row.intent.continuation != continuation {
        return Err(conflict("stale continuation"));
    }
    let time = ms(now(state));
    let cid = continuation.continuation_id;
    let disposition = match &packet.body.message {
        Message::Checkpoint(c) => {
            if &row.intent.binding != binding {
                return Err(AppError::Forbidden("not the suspended binding".into()));
            }
            if row.phase != "directed" || row.checkpoint.is_some() {
                return Err(conflict("checkpoint already recorded"));
            }
            if let CheckpointClaim::Supplied { summary, .. } = &c.claim
                && summary.text.as_str().len() > row.intent.max_summary_bytes as usize
            {
                return Err(AppError::BadRequest(
                    "checkpoint summary exceeds budget".into(),
                ));
            }
            sqlx::query("UPDATE hibernation_suspensions SET checkpoint=$2,checkpoint_message_id=$3,updated_at_ms=$4 WHERE continuation_id=$1")
                .bind(cid).bind(to_json(c)?).bind(message_id).bind(time)
                .execute(&mut *tx).await.map_err(db)?;
            ReceiptDisposition::CheckpointRecorded
        }
        Message::Parking(p) => {
            if &row.intent.binding != binding {
                return Err(AppError::Forbidden("not the suspended binding".into()));
            }
            record_parking(&mut tx, &row, p, message_id, time).await?;
            ReceiptDisposition::ParkingRecorded
        }
        Message::BootstrapAck(a) => {
            let (Some(attachment), Some(digest)) = (&row.attachment, &row.attachment_digest) else {
                return Err(conflict("no continuation attachment issued"));
            };
            if row.phase != "continued" {
                return Err(conflict("bootstrap already acknowledged"));
            }
            if a.admission_id != attachment.admission_id
                || a.continuation != attachment.continuation
                || a.successor != attachment.successor
                || a.outcome != attachment.outcome
                || a.attachment_digest.as_str() != digest
            {
                return Err(conflict("acknowledgment does not match the attachment"));
            }
            let recorded: Option<Value> = sqlx::query_scalar(
                "SELECT response#>'{body,body,acknowledgment_receipt}' FROM agent_wait_acks WHERE admission_id=$1",
            )
            .bind(a.admission_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .flatten();
            let recorded: Option<ArtifactRef> = recorded.map(from_json).transpose()?;
            if recorded.as_ref() != Some(&a.acknowledgment_receipt) {
                return Err(conflict("acknowledgment receipt not recorded by AQ"));
            }
            sqlx::query("UPDATE hibernation_suspensions SET phase='acknowledged',ack=$2,ack_message_id=$3,updated_at_ms=$4 WHERE continuation_id=$1")
                .bind(cid).bind(to_json(a)?).bind(message_id).bind(time)
                .execute(&mut *tx).await.map_err(db)?;
            ReceiptDisposition::BootstrapAcknowledgmentRecorded
        }
        _ => unreachable!("filtered above"),
    };
    let receipt = Receipt {
        schema_version: lc::SCHEMA_VERSION,
        message_id,
        body_digest: packet.body_digest.clone(),
        disposition,
        through_sequence: None,
    };
    receipt
        .validate_against(packet)
        .map_err(|_| AppError::Internal)?;
    sqlx::query("INSERT INTO hibernation_receipts(message_id,continuation_id,body_digest,packet,receipt,recorded_at_ms) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(message_id)
        .bind(cid)
        .bind(packet.body_digest.as_str())
        .bind(to_json(packet)?)
        .bind(to_json(&receipt)?)
        .bind(time)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(receipt)
}

async fn record_parking(
    tx: &mut Transaction<'_, Postgres>,
    row: &Row5,
    p: &ParkingReport,
    message_id: Uuid,
    time: i64,
) -> Result<()> {
    let cid = row.intent.continuation.continuation_id;
    if p.protected_disk_released {
        // The second report: the exact directive AQ issued, same retention.
        let Some(stored) = &row.parking else {
            return Err(conflict("no retained parking report"));
        };
        if row.disk_release_directive.is_none()
            || p.disk_release_directive != row.disk_release_directive
        {
            return Err(conflict("protected disk release was not directed"));
        }
        if stored.disposition != SuspensionDisposition::RetainedSuspended
            || p.central_receipts != stored.central_receipts
        {
            return Err(conflict("disk release changes the retained suspension"));
        }
        sqlx::query("UPDATE hibernation_suspensions SET disk_released=true,updated_at_ms=$2 WHERE continuation_id=$1")
            .bind(cid).bind(time).execute(&mut **tx).await.map_err(db)?;
        return Ok(());
    }
    if !matches!(row.phase.as_str(), "directed" | "parked_held") {
        return Err(conflict("suspension already parked"));
    }
    if p.disk_release_directive.is_some() {
        return Err(conflict("protected disk release was not directed"));
    }
    let phase = match &p.disposition {
        SuspensionDisposition::Preparing | SuspensionDisposition::StoppedRetaining => {
            return Ok(());
        }
        SuspensionDisposition::Held { .. } => "parked_held",
        SuspensionDisposition::RetainedSuspended => {
            if row.checkpoint.is_none() {
                return Err(conflict("retained suspension needs a checkpoint"));
            }
            "parked_retained"
        }
    };
    sqlx::query("UPDATE hibernation_suspensions SET phase=$2,parking=$3,parking_message_id=$4,updated_at_ms=$5 WHERE continuation_id=$1")
        .bind(cid).bind(phase).bind(to_json(p)?).bind(message_id).bind(time)
        .execute(&mut **tx).await.map_err(db)?;
    Ok(())
}

/// (4) D3: a predecessor whose parking report was recorded as Held or
/// RetainedSuspended is suspended, neither success nor failure. The hold
/// stays until the successor's exact BootstrapAck, after which the successor
/// owns the task and the predecessor's report still never settles it.
pub(crate) async fn suspended_hold(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<Option<(&'static str, Value)>> {
    let row = sqlx::query("SELECT continuation_id,phase FROM hibernation_suspensions WHERE predecessor_attempt_id=$1 AND phase IN ('parked_held','parked_retained','continued','acknowledged') ORDER BY created_at_ms DESC LIMIT 1")
        .bind(attempt)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?;
    Ok(row.map(|row| {
        let phase: String = row.get("phase");
        let reason = if phase == "acknowledged" {
            "continued_by_successor"
        } else {
            "suspended"
        };
        let evidence = json!({
            "suspension": {
                "continuation_id": row.get::<Uuid, _>("continuation_id"),
                "phase": phase,
            }
        });
        (reason, evidence)
    }))
}

// ---- routes ----

fn no_store<T>(v: T) -> ([(header::HeaderName, &'static str); 1], Json<T>) {
    ([(header::CACHE_CONTROL, "no-store")], Json(v))
}
type Reply<T> = std::result::Result<([(header::HeaderName, &'static str); 1], Json<T>), AppError>;

/// Worker routes (node bearer or delegated permit).
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/lifecycle/directives", get(get_directives))
        .route("/v1/worker/lifecycle/packets", post(post_packet))
        .layer(DefaultBodyLimit::max(lc::MAX_EVENT_BYTES))
        .with_state(state)
}
/// Owner routes: explicit suspension and protected-disk release issuance.
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/waits/{wait_id}/suspend", post(owner_suspend))
        .route(
            "/api/hibernation/{continuation_id}/disk-release",
            post(owner_disk_release),
        )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttemptQuery {
    attempt_id: Uuid,
}
async fn auth(
    state: &AppState,
    headers: &HeaderMap,
    attempt: Option<Uuid>,
) -> Result<AuthenticatedNode> {
    crate::delegated_permits::runtime::authenticate_runner(
        state,
        headers,
        aq_admission_sandbox::sandbox::RunnerOperation::Observe,
        attempt,
    )
    .await
}
async fn get_directives(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AttemptQuery>,
) -> Reply<Directives> {
    let node = auth(&state, &headers, Some(q.attempt_id)).await?;
    Ok(no_store(directives(&state, &node, q.attempt_id).await?))
}
async fn post_packet(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Reply<Receipt> {
    let packet = lc::decode(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid lifecycle packet: {e}")))?;
    let node = auth(&state, &headers, None).await?;
    Ok(no_store(receive(&state, &node, &packet).await?))
}
async fn owner_suspend(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(wait_id): Path<Uuid>,
) -> Reply<Packet> {
    Ok(no_store(issue_suspension(&state, wait_id).await?))
}
async fn owner_disk_release(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(continuation_id): Path<Uuid>,
) -> Reply<ArtifactRef> {
    Ok(no_store(issue_disk_release(&state, continuation_id).await?))
}
