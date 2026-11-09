//! Production H3 ports: evidence from the observation ledger, node presence,
//! terminal input and B3 stop state; effects through the F3 mailbox, F4
//! notifications, B3 cancellation and B4 replacement. Every effect is
//! idempotent by its ladder action ID.
use super::engine::{Ports, Target};
use super::evidence::{Admission, Evidence, Explanation, InputState, PROGRESS_SQL};
use super::ladder::{Delivery, Intent, Outcome};
use super::policy::ExecutionPolicy;
use crate::{
    AppState,
    agent_access::messaging::{self, OwnerSendError, Recipient, Send},
    error::AppError,
    profiled_execution::{
        cancellation,
        replacement::{self, Assessment},
    },
};
use async_trait::async_trait;
use serde_json::json;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

/// `$1` is the attempt. A human controller holds the attempt's pane.
pub(crate) const HUMAN_CONTROL_SQL: &str = "EXISTS(SELECT 1 FROM terminal_sessions hc WHERE hc.attempt_id=$1 AND hc.controller_principal IS NOT NULL)";
/// `$1` is the attempt. A terminal input is still in flight or its outcome is
/// unknown, so the pane's input state cannot be trusted (0003 §5.1).
pub(crate) const INPUT_UNKNOWN_SQL: &str = "EXISTS(SELECT 1 FROM terminal_sessions iu JOIN terminal_input_receipts ir ON ir.terminal_id=iu.terminal_id WHERE iu.attempt_id=$1 AND ir.disposition IN ('queued','received','injecting','delivery_unknown'))";

const NUDGE_BODY: &str = "AQ stall check: no progress has been observed for this session. At your next safe boundary, continue the task or report what is blocking you.";

use crate::error::db;

pub async fn input_state(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<InputState, AppError> {
    let row = sqlx::query(&format!(
        "SELECT {HUMAN_CONTROL_SQL} AS human,{INPUT_UNKNOWN_SQL} AS unknown"
    ))
    .bind(attempt)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    Ok(if row.get::<bool, _>("human") {
        InputState::HumanControlled
    } else if row.get::<bool, _>("unknown") {
        InputState::Unknown
    } else {
        InputState::Ready
    })
}

/// Queue a nudge through the F3 session mailbox. Input that is not `Ready`
/// at send time (or a ladder-requested deferral) stores it inbox-only; the
/// node poll filter withholds every stall nudge while input is unsafe.
pub async fn nudge(
    tx: &mut Transaction<'_, Postgres>,
    target: &Target,
    action_id: Uuid,
    body: &str,
    defer: bool,
    now: u64,
) -> Result<Outcome, AppError> {
    nudge_as(tx, target, action_id, body, defer, None, now)
        .await
        .map(|(outcome, _)| outcome)
        .map_err(|e| match e {
            OwnerSendError::Invalid => AppError::BadRequest("invalid nudge".into()),
            OwnerSendError::NotFound => AppError::NotFound("nudge recipient not found".into()),
            OwnerSendError::Conflict => AppError::Conflict("nudge id conflict".into()),
            OwnerSendError::Unknown => AppError::Internal,
        })
}

/// The one nudge path for the ladder (`owner: None`, sender `system`) and the
/// owner `session.nudge` action. The operation ID names the message, so a
/// replay returns the first send and its first recorded delivery.
pub(crate) async fn nudge_as(
    tx: &mut Transaction<'_, Postgres>,
    target: &Target,
    operation_id: Uuid,
    body: &str,
    defer: bool,
    owner: Option<Uuid>,
    now: u64,
) -> Result<(Outcome, serde_json::Value), OwnerSendError> {
    let unknown = |_| OwnerSendError::Unknown;
    let input = input_state(tx, target.attempt_id)
        .await
        .map_err(|_| OwnerSendError::Unknown)?;
    let message_id = Uuid::new_v5(&operation_id, b"stall-nudge-message");
    let request = Send {
        operation_id,
        message_id,
        thread_id: Uuid::new_v5(&target.attempt_id, b"stall-nudge-thread"),
        recipient: Recipient::Session {
            workspace_id: target.workspace_id,
            project_id: target.project_id,
            task_id: target.task_id,
            session_id: target.session_id,
            attempt_id: target.attempt_id,
            generation: target.generation,
        },
        body: body.to_owned(),
    };
    let response = match owner {
        Some(user) => messaging::owner_send_in_tx(tx, request, user, now).await?,
        None => messaging::system_send_in_tx(tx, request, now).await?,
    };
    sqlx::query("INSERT INTO stall_nudges(message_id,attempt_id,action_id,inbox_only,created_at_ms) VALUES($1,$2,(SELECT action_id FROM stall_actions WHERE action_id=$3),$4,$5) ON CONFLICT(message_id) DO NOTHING")
        .bind(message_id)
        .bind(target.attempt_id)
        .bind(operation_id)
        .bind(defer || input != InputState::Ready)
        .bind(i64::try_from(now).map_err(|_| OwnerSendError::Invalid)?)
        .execute(&mut **tx)
        .await
        .map_err(unknown)?;
    // A replay reports what was first recorded, not today's input state.
    let inbox_only: bool =
        sqlx::query_scalar("SELECT inbox_only FROM stall_nudges WHERE message_id=$1")
            .bind(message_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(unknown)?;
    let delivery = if inbox_only {
        Delivery::Deferred
    } else {
        Delivery::Attempted
    };
    Ok((
        Outcome::Nudged {
            message_id,
            delivery,
        },
        response,
    ))
}

/// Message IDs `node_poll` would hand this attempt's harness now.
pub async fn injectable(pool: &sqlx::PgPool, attempt: Uuid) -> Result<Vec<Uuid>, AppError> {
    let binding: serde_json::Value =
        sqlx::query_scalar("SELECT binding FROM agent_launch_bindings WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(pool)
            .await
            .map_err(db)?;
    let id = |k: &str| -> Result<Uuid, AppError> {
        binding[k]
            .as_str()
            .and_then(|v| v.parse().ok())
            .ok_or(AppError::Internal)
    };
    let task = Recipient::Task {
        workspace_id: id("workspace_id")?,
        project_id: id("project_id")?,
        task_id: id("task_id")?,
    };
    let session = Recipient::Session {
        workspace_id: id("workspace_id")?,
        project_id: id("project_id")?,
        task_id: id("task_id")?,
        session_id: id("session_id")?,
        attempt_id: attempt,
        generation: binding["generation"].as_u64().ok_or(AppError::Internal)?,
    };
    let mut tx = pool.begin().await.map_err(db)?;
    let ids = messaging::injectable_ids(&mut tx, attempt, &task, &session, 16)
        .await
        .map_err(|_| AppError::Internal)?;
    tx.rollback().await.map_err(db)?;
    Ok(ids)
}

pub struct DbPorts {
    pub state: AppState,
}

#[async_trait]
impl Ports for DbPorts {
    async fn evidence(
        &self,
        target: &Target,
        policy: &ExecutionPolicy,
        now: u64,
    ) -> Result<Evidence, AppError> {
        let pool = &self.state.store.pool;
        let row = sqlx::query(&format!("SELECT \
            (SELECT max(recorded_at_ms) FROM observation_events WHERE attempt_id=$1 AND {PROGRESS_SQL}) AS progress, \
            GREATEST((SELECT max(recorded_at_ms) FROM observation_events WHERE attempt_id=$1), \
                     (SELECT (extract(epoch FROM n.last_seen_at)*1000)::bigint FROM nodes n JOIN execution_attempts a ON a.node_id=n.id WHERE a.attempt_id=$1)) AS observed, \
            (SELECT record->'body'->'kind' FROM observation_events WHERE attempt_id=$1 AND record->'body'->'kind'->>'kind'<>'usage' ORDER BY recorded_at_ms DESC,sequence DESC LIMIT 1) AS latest, \
            (SELECT max(recorded_at_ms) FROM observation_events s WHERE s.attempt_id=$1 AND s.record->'body'->'kind'->>'kind'='tool_started' AND NOT EXISTS(SELECT 1 FROM observation_events r WHERE r.attempt_id=$1 AND r.record->'body'->'kind'->>'kind'='tool_result' AND r.record->'body'->'kind'->>'call_id'=s.record->'body'->'kind'->>'call_id')) AS open_tool, \
            NOT a.holds_task AS released, \
            EXISTS(SELECT 1 FROM profiled_cancellations c WHERE c.attempt_id=$1) AS cancelled, \
            COALESCE(h.state#>>'{{stop,kind}}' IN ('stopped','not_started'),FALSE) AS stopped \
            FROM execution_attempts a LEFT JOIN profiled_authority_hot h ON h.attempt_id=a.attempt_id WHERE a.attempt_id=$1"))
            .bind(target.attempt_id)
            .fetch_one(pool)
            .await
            .map_err(db)?;
        let ms = |c: &str| row.get::<Option<i64>, _>(c).map(|v| v.max(0) as u64);
        let observed = ms("observed");
        let latest: Option<serde_json::Value> = row.get("latest");
        let mut tx = pool.begin().await.map_err(db)?;
        let input = input_state(&mut tx, target.attempt_id).await?;
        tx.rollback().await.map_err(db)?;
        let explanation = if observed.is_none_or(|t| now.saturating_sub(t) > policy.grace_ms) {
            Some(Explanation::ObservationsUnavailable)
        } else if input == InputState::HumanControlled {
            Some(Explanation::HumanControl)
        } else if latest
            .as_ref()
            .is_some_and(|k| k["kind"] == "provider_error" && k["retryable"] == json!(true))
        {
            Some(Explanation::ProviderBackoff)
        } else if ms("open_tool").is_some_and(|t| now.saturating_sub(t) < policy.long_check_ms) {
            Some(Explanation::KnownLongCheck)
        } else {
            None
        };
        // Held, hibernated and H4 registered waits have no durable source yet.
        Ok(Evidence {
            last_progress_ms: ms("progress"),
            observed_through_ms: observed,
            explanation,
            input,
            admission: Admission {
                cancelled: row.get("cancelled"),
                held: false,
                hibernated: false,
                ownership_lost: row.get("released"),
            },
            stop_confirmed: row.get("stopped"),
        })
    }

    async fn effect(
        &self,
        target: &Target,
        action_id: Uuid,
        intent: &Intent,
        policy: &ExecutionPolicy,
        now: u64,
    ) -> Result<Outcome, AppError> {
        let state = &self.state;
        match intent {
            Intent::MonitoringIncident { ordinal } => {
                // The receipted action row is the durable incident record.
                tracing::warn!(attempt = %target.attempt_id, %action_id, ordinal, "stall evidence unavailable; monitoring incident, no recovery");
                Ok(Outcome::Done)
            }
            Intent::Nudge { defer, .. } => {
                let mut tx = state.store.pool.begin().await.map_err(db)?;
                let outcome = nudge(&mut tx, target, action_id, NUDGE_BODY, *defer, now).await?;
                tx.commit().await.map_err(db)?;
                Ok(outcome)
            }
            Intent::Notify => {
                let policy_f4 = crate::notifications::policy::Policy::new(
                    crate::notifications::api::service(state),
                );
                policy_f4
                    .observe_stall(
                        action_id,
                        aq_lifecycle_api::Scope {
                            project_id: target.project_id,
                            workspace_id: target.workspace_id,
                        },
                        target.attempt_id,
                        "Session stalled after nudges".into(),
                    )
                    .await
                    .map_err(|_| AppError::Internal)?;
                Ok(Outcome::Done)
            }
            Intent::Stop => {
                cancellation::cancel(state, target.attempt_id, action_id, "stall_ladder_stop")
                    .await?;
                Ok(Outcome::Done)
            }
            Intent::Replace => match replacement::assess_task(state, target.task_id, now).await? {
                Assessment::Replace(r) => {
                    replacement::release(state, target.task_id, &r).await?;
                    Ok(Outcome::Replaced { replacement: None })
                }
                Assessment::Fresh => Ok(Outcome::Replaced { replacement: None }),
                Assessment::Blocked(reason) => Ok(Outcome::Blocked {
                    reason: reason.into(),
                }),
            },
            Intent::Quarantine { reason } => {
                let held = json!({"task_id":target.task_id,"attempt_id":target.attempt_id,"session_id":target.session_id,"generation":target.generation,"claim":"task","worktree":"retained"});
                sqlx::query("INSERT INTO stall_quarantines(attempt_id,lineage_id,reason,escalation,held,action_id,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(attempt_id) DO NOTHING")
                    .bind(target.attempt_id)
                    .bind(target.task_id)
                    .bind(reason)
                    .bind(&policy.escalation)
                    .bind(held)
                    .bind(action_id)
                    .bind(i64::try_from(now).map_err(|_| AppError::Internal)?)
                    .execute(&state.store.pool)
                    .await
                    .map_err(db)?;
                tracing::warn!(attempt = %target.attempt_id, %reason, escalation = %policy.escalation, "stall ladder quarantined lineage");
                Ok(Outcome::Done)
            }
        }
    }
}
