//! Central absence of execution authorization. This never claims physical stop.
use crate::{AppState, error::AppError};
use aq_admission::{ArtifactRef, Digest, LaunchBinding, Text};
use serde::Deserialize;
use sqlx::Row;
use uuid::Uuid;
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCheckpoint {
    pub task_checkpoint_transfers: u32,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectCheckpoint {
    pub graph_operations: u32,
    pub created_records: u32,
    pub observation_sources: u32,
    pub observation_events: u32,
    pub mailbox_receipts: u32,
    pub uncertain_deliveries: u32,
    pub session_messages: u32,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureProof {
    pub schema_version: u32,
    pub closure_id: Uuid,
    pub operation_id: Uuid,
    pub binding: LaunchBinding,
    pub observed_record_revision: u64,
    pub closed_at_ms: u64,
    pub reason: String,
    pub applicable_deadline_ms: u64,
    pub material_leases_revoked: Vec<Uuid>,
    pub pending_deliveries_staled: Vec<Uuid>,
    pub pending_deliveries_staled_count: u32,
    pub source_checkpoint: SourceCheckpoint,
    pub effect_checkpoint: EffectCheckpoint,
}
pub struct Closure {
    pub proof: ClosureProof,
    pub artifact: ArtifactRef,
    pub bytes: Vec<u8>,
    pub changed: bool,
}
/// Trusted internal expiry owner; HTTP callers must use authenticated node
/// context and its locked credential recheck before invoking the locked variant.
pub async fn close_expired(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
) -> Result<Option<Closure>, AppError> {
    let mut tx = state
        .store
        .pool
        .begin()
        .await
        .map_err(|_| AppError::Internal)?;
    // Bound contention for housekeeping as well as direct internal callers.
    // Expiry is sampled only after node ownership below; a lock wait cannot
    // manufacture an earlier proof or authorize a grant.
    for sql in [
        "SET LOCAL lock_timeout='250ms'",
        "SET LOCAL statement_timeout='2s'",
    ] {
        sqlx::query(sql)
            .execute(&mut *tx)
            .await
            .map_err(|_| AppError::Internal)?;
    }
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?
        .ok_or_else(|| AppError::NotFound("node not found".into()))?;
    // Same node -> exact attempt/hot order as grant issuance. History loading
    // below performs no external IO and cannot mint a live worker principal.
    let current = sqlx::query("SELECT l.launch_binding,h.state AS hot FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) JOIN profiled_launches l USING(attempt_id) WHERE a.attempt_id=$1 AND a.node_id=$2 FOR UPDATE OF a,h")
        .bind(attempt).bind(node).fetch_one(&mut *tx).await.map_err(|_|AppError::Internal)?;
    let binding: LaunchBinding =
        serde_json::from_value(current.get("launch_binding")).map_err(|_| AppError::Internal)?;
    let hot: crate::execution::authority::hot::HotState =
        serde_json::from_value(current.get("hot")).map_err(|_| AppError::Internal)?;
    hot.validate_against(&binding)
        .map_err(|_| AppError::Internal)?;
    let phase = super::phase_profile::for_attempt_locked(&mut tx, attempt).await?;
    if hot.status == crate::execution::profiled::Status::Preparing && phase.is_some() {
        // Historical validators retain the exact Frozen/Refresh nominal types;
        // neither lease is reinterpreted as a legacy PreparationPlan.
        macro_rules! validate_phase {
            ($owner:path) => {{
                use $owner as owner;
                let history = owner::load_accepted_history_locked(&mut tx, &binding).await?;
                let lease = &history.receipt.lease;
                let owner = hot
                    .owner
                    .as_ref()
                    .ok_or_else(|| AppError::Conflict("phase absence owner missing".into()))?;
                if lease.binding != binding
                    || Some(history.request.original.operation_id) != hot.preparation_operation_id
                    || lease.guardian_id != owner.guardian_id
                    || lease.launch_id != owner.launch_id
                    || lease.hold.reservation.reservation_id != owner.reservation_id
                {
                    return Err(AppError::Conflict(
                        "phase absence historical owner differs".into(),
                    ));
                }
            }};
        }
        match phase.unwrap() {
            super::phase_profile::Profile::Frozen => validate_phase!(super::accepted_preparation),
            super::phase_profile::Profile::Refresh => validate_phase!(super::accepted_refresh),
        }
        sqlx::query(
            "SELECT set_config('aq.phase_no_grant_owner','aq.phase-no-grant-owner.1',true)",
        )
        .execute(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?;
    }
    let preparation:Option<serde_json::Value>=sqlx::query_scalar("SELECT r.response FROM profiled_authority_hot h LEFT JOIN profiled_execution_receipts r ON r.node_id=$2 AND r.operation_id=(h.state->>'preparation_operation_id')::UUID WHERE h.attempt_id=$1 AND h.state->>'status'='preparing'")
        .bind(attempt).bind(node).fetch_optional(&mut *tx).await.map_err(|_|AppError::Internal)?.flatten();
    let preparing:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_authority_hot WHERE attempt_id=$1 AND state->>'status'='preparing')").bind(attempt).fetch_one(&mut *tx).await.map_err(|_|AppError::Internal)?;
    if preparing && phase.is_none() {
        let document: aq_admission::transport::TransportDocument =
            serde_json::from_value(preparation.ok_or_else(|| {
                AppError::Conflict("accepted preparation authority unavailable".into())
            })?)
            .map_err(|_| AppError::Conflict("invalid accepted preparation authority".into()))?;
        if !matches!(
            document,
            aq_admission::transport::TransportDocument::PreparationPlan(_)
        ) || document.validate().is_err()
        {
            return Err(AppError::Conflict(
                "unsupported accepted preparation authority".into(),
            ));
        }
    }
    let now = (state.clock)().timestamp_millis();
    let row = sqlx::query("SELECT * FROM profiled_close_expired_ungranted($1,$2,$3,$4,$5)")
        .bind(node)
        .bind(attempt)
        .bind(now)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| AppError::Conflict("ungranted closure refused by current owners".into()))?;
    let result = row
        .map(|row| {
            let bytes: Vec<u8> = row.get("body_bytes");
            let digest: String = row.get("body_digest");
            let proof: ClosureProof =
                serde_json::from_slice(&bytes).map_err(|_| AppError::Internal)?;
            aq_session_protocol::Validate::validate(&proof.binding)
                .map_err(|_| AppError::Internal)?;
            let actual = Digest::of(&bytes);
            if actual.as_str() != digest
                || proof.binding.attempt_id != attempt
                || proof.schema_version != 1
                || proof.closure_id.is_nil()
                || proof.operation_id.is_nil()
                || proof.observed_record_revision == 0
                || proof.applicable_deadline_ms == 0
                || proof.closed_at_ms < proof.applicable_deadline_ms
                || !["offer_expired", "preparation_expired"].contains(&proof.reason.as_str())
                || proof.material_leases_revoked.len() > 64
                || proof.material_leases_revoked.iter().any(Uuid::is_nil)
                || proof.pending_deliveries_staled.len() > 256
                || proof.pending_deliveries_staled.iter().any(Uuid::is_nil)
                || proof.pending_deliveries_staled_count as usize
                    != proof.pending_deliveries_staled.len()
                || proof.source_checkpoint.task_checkpoint_transfers != 0
                || [
                    proof.effect_checkpoint.graph_operations,
                    proof.effect_checkpoint.created_records,
                    proof.effect_checkpoint.observation_sources,
                    proof.effect_checkpoint.observation_events,
                    proof.effect_checkpoint.mailbox_receipts,
                    proof.effect_checkpoint.uncertain_deliveries,
                    proof.effect_checkpoint.session_messages,
                ]
                .iter()
                .any(|count| *count != 0)
            {
                return Err(AppError::Internal);
            }
            Ok(Closure {
                artifact: ArtifactRef {
                    id: Text::new(format!("aq.never-granted/{}", proof.closure_id))
                        .map_err(|_| AppError::Internal)?,
                    version: Text::new("1").map_err(|_| AppError::Internal)?,
                    digest: actual,
                },
                proof,
                bytes,
                changed: row.get("changed"),
            })
        })
        .transpose()?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    Ok(result)
}

pub const MAX_UNGRANTED_RETRIES: u32 = 8;
#[derive(Debug, Clone, Copy)]
pub struct AttemptCounts {
    pub started: u32,
    pub history: u32,
    pub never_granted: u32,
}
const COUNTS: &str = "SELECT count(*)::BIGINT AS history,count(*) FILTER(WHERE NOT (NOT a.holds_task AND profiled_never_granted_valid(a.attempt_id)))::BIGINT AS started,count(c.attempt_id)::BIGINT AS never_granted FROM execution_attempts a LEFT JOIN profiled_never_granted_closures c ON c.attempt_id=a.attempt_id WHERE a.task_id=$1";
fn counts(row: sqlx::postgres::PgRow) -> Result<AttemptCounts, AppError> {
    let value = |name: &str| {
        u32::try_from(row.get::<i64, _>(name))
            .map_err(|_| AppError::Conflict("task attempt history bound".into()))
    };
    Ok(AttemptCounts {
        started: value("started")?,
        history: value("history")?,
        never_granted: value("never_granted")?,
    })
}
impl AttemptCounts {
    pub fn check_retry_bound(self) -> Result<Self, AppError> {
        if self.never_granted >= MAX_UNGRANTED_RETRIES {
            Err(AppError::Conflict(
                "never-granted retry budget exhausted".into(),
            ))
        } else {
            Ok(self)
        }
    }
    pub fn budget_revision(self) -> u64 {
        u64::from(self.history) + 1
    }
}
pub async fn attempt_counts(pool: &sqlx::PgPool, task: Uuid) -> Result<AttemptCounts, AppError> {
    counts(
        sqlx::query(COUNTS)
            .bind(task)
            .fetch_one(pool)
            .await
            .map_err(|_| AppError::Internal)?,
    )
}
pub async fn attempt_counts_locked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task: Uuid,
) -> Result<AttemptCounts, AppError> {
    counts(
        sqlx::query(COUNTS)
            .bind(task)
            .fetch_one(&mut **tx)
            .await
            .map_err(|_| AppError::Internal)?,
    )
}

pub async fn next_generation(pool: &sqlx::PgPool, task: Uuid) -> Result<u64, AppError> {
    let last:Option<i64>=sqlx::query_scalar("SELECT max(COALESCE((state#>>'{offer,binding,assignment_generation}')::BIGINT,(state#>>'{offer,attempt,generation}')::BIGINT)) FROM execution_attempts WHERE task_id=$1").bind(task).fetch_one(pool).await.map_err(|_|AppError::Internal)?;
    last.unwrap_or(0)
        .checked_add(1)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| AppError::Conflict("assignment generation exhausted".into()))
}
