//! Dedicated post-execution Graph effect; never changes the frozen agent ledger.
use super::{actor::ActorPin, effects, owner::*};
use crate::{profiled_execution::results, store::User};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionReceipt {
    pub workspace_id: Uuid,
    pub task_id: Uuid,
    pub change_set_id: Uuid,
    pub version: u64,
    pub content_revision: u64,
    pub response_digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionStatus {
    pub operation_id: Uuid,
    pub evaluation_operation_id: Uuid,
    pub state: String,
    pub dispatch_count: u32,
    pub receipt: Option<CompletionReceipt>,
    pub recovery: String,
}
fn status(row: &sqlx::postgres::PgRow) -> Result<CompletionStatus> {
    let state: String = row.get("state");
    Ok(CompletionStatus {
        operation_id: row.get("operation_id"),
        evaluation_operation_id: row.get("evaluation_operation_id"),
        recovery: if matches!(state.as_str(), "confirmed" | "conflict" | "blocked") {
            "none"
        } else if row.get::<i32, _>("dispatch_count") >= 8 {
            "manual_reconciliation"
        } else {
            "replay_identical"
        }
        .into(),
        state,
        dispatch_count: row.get::<i32, _>("dispatch_count") as u32,
        receipt: row
            .get::<Option<Value>, _>("receipt")
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| crate::error::AppError::Internal)?,
    })
}
pub(super) fn confirmed(raw: &Value, row: &sqlx::postgres::PgRow) -> Result<CompletionReceipt> {
    if !crate::agent_access::settlement_response_valid(raw) {
        return Err(unavailable());
    }
    let nodes = raw["results"]
        .as_array()
        .filter(|v| v.len() == 1)
        .ok_or_else(unavailable)?;
    let node = &nodes[0];
    let version = u64::try_from(row.get::<i64, _>("expected_version"))
        .map_err(|_| unavailable())?
        .checked_add(1)
        .ok_or_else(unavailable)?;
    let logical = u64::try_from(row.get::<i64, _>("expected_content_revision"))
        .map_err(|_| unavailable())?
        .checked_add(1)
        .ok_or_else(unavailable)?;
    let workspace: Uuid = row.get("workspace_id");
    let task: Uuid = row.get("task_id");
    if node["id"] != json!(task)
        || node["workspace_id"] != json!(workspace)
        || node["kind"] != "task"
        || node["task_state"] != "completed"
        || node["deleted"] != false
        || node["retired"] != false
        || node["version"] != json!(version)
        || node["content_revision"] != json!(logical)
    {
        return Err(unavailable());
    }
    let change_set_id = raw["change_set_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
        .filter(|id| !id.is_nil())
        .ok_or_else(unavailable)?;
    Ok(CompletionReceipt {
        workspace_id: workspace,
        task_id: task,
        change_set_id,
        version,
        content_revision: logical,
        response_digest: hash(raw)?,
    })
}
impl Service {
    pub(super) async fn prepare_completion_locked(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        r: &results::RecordedResult,
        e: &EvaluationReceipt,
    ) -> Result<()> {
        let rt = self.state.agent_access.as_deref().ok_or_else(unavailable)?;
        let task = &r.spec.task;
        let op = e.completion_operation_id.ok_or_else(conflict)?;
        let service = crate::agent_access::settlement_service_identity(rt, task.workspace_id)?;
        let payload = bytes(
            &json!({"idempotency_key":format!("aq-settlement-v1:{op}"),"reason":format!("Human-reviewed retained result {}",r.report.report_id),"operations":[{"op":"update_node","id":task.task_id,"expected_version":task.version,"patch":{"task_state":"completed"}}]}),
        )?;
        if payload.len() > 8192 || bytes(&service)?.len() > 4096 {
            return Err(unavailable());
        }
        sqlx::query("INSERT INTO profiled_task_completions(operation_id,attempt_id,evaluation_operation_id,release_id,workspace_id,task_id,expected_version,expected_content_revision,payload_bytes,payload_digest,service_identity,state,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'prepared',$12)")
            .bind(op).bind(e.attempt_id).bind(e.operation_id).bind(e.release_id).bind(task.workspace_id).bind(task.task_id).bind(i64::try_from(task.version).map_err(|_|conflict())?).bind(i64::try_from(task.content_revision).map_err(|_|conflict())?).bind(&payload).bind(aq_admission::Digest::of(&payload).as_str()).bind(service).bind(e.recorded_at_ms as i64).execute(&mut **tx).await.map_err(db)?;
        Ok(())
    }
    /// Explicit recovery only. No background task or scheduler is installed.
    pub async fn complete(&self, actor: &User, operation: Uuid) -> Result<CompletionStatus> {
        let actor = ActorPin::observe(&self.state, actor).await?;
        let peek=sqlx::query("SELECT c.dispatch_count,e.report_id,e.source_operation_id,e.actor_id,e.actor_identity_digest FROM profiled_task_completions c JOIN profiled_settlement_evaluations e ON e.operation_id=c.evaluation_operation_id WHERE c.operation_id=$1").bind(operation).fetch_optional(&self.state.store.pool).await.map_err(db)?.ok_or_else(||crate::error::AppError::NotFound("completion not found".into()))?;
        if peek.get::<Uuid, _>("actor_id") != actor.id()
            || peek.get::<String, _>("actor_identity_digest") != actor.identity_digest()?
        {
            return Err(conflict());
        }
        let observation = if peek.get::<i32, _>("dispatch_count") == 0 {
            Some(
                super::applicability::Observation::observe(
                    &self.state,
                    peek.get("report_id"),
                    peek.get("source_operation_id"),
                )
                .await?,
            )
        } else {
            None
        };
        let mut tx = self.tx().await?;
        let ids=sqlx::query("SELECT e.report_id,e.actor_id,e.actor_identity_digest,e.receipt AS evaluation FROM profiled_task_completions c JOIN profiled_settlement_evaluations e ON e.operation_id=c.evaluation_operation_id WHERE c.operation_id=$1").bind(operation).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(||crate::error::AppError::NotFound("completion not found".into()))?;
        let r = results::load_locked(&self.state, &mut tx, ids.get("report_id")).await?;
        actor.fence(&mut tx).await?;
        if ids.get::<Uuid, _>("actor_id") != actor.id()
            || ids.get::<String, _>("actor_identity_digest") != actor.identity_digest()?
        {
            return Err(conflict());
        }
        let e: EvaluationReceipt = serde_json::from_value(ids.get("evaluation"))
            .map_err(|_| crate::error::AppError::Internal)?;
        if e.disposition != EvaluationDisposition::Accepted
            || e.report != r.receipt_artifact
            || !r.is_current_report_head
        {
            return Err(conflict());
        }
        let row =
            sqlx::query("SELECT * FROM profiled_task_completions WHERE operation_id=$1 FOR UPDATE")
                .bind(operation)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
        if matches!(
            row.get::<String, _>("state").as_str(),
            "confirmed" | "conflict" | "blocked"
        ) {
            if row.get::<String, _>("state") == "confirmed" {
                self.refresh_settled_attempt(&mut tx, e.attempt_id).await?;
            }
            let s = status(&row)?;
            tx.commit().await.map_err(db)?;
            return Ok(s);
        }
        let time = now(&self.state)?;
        let count = row.get::<i32, _>("dispatch_count");
        if row
            .get::<Option<i64>, _>("lease_expires_at_ms")
            .is_some_and(|v| v > time)
            || count >= 8
            || time < row.get::<i64, _>("created_at_ms")
            || time >= row.get::<i64, _>("created_at_ms").saturating_add(60000)
        {
            let s = status(&row)?;
            tx.commit().await.map_err(db)?;
            return Ok(s);
        }
        let rt = self.state.agent_access.as_deref().ok_or_else(unavailable)?;
        if crate::agent_access::settlement_service_identity(rt, row.get("workspace_id"))?
            != row.get::<Value, _>("service_identity")
        {
            return Err(unavailable());
        }
        let ledger = effects::inspect_aq_effects_locked(&self.state, &mut tx, &r).await?;
        if ledger.disposition() != effects::LedgerDisposition::Settled
            || ledger.digest() != &e.ledger_digest
        {
            return Err(conflict());
        }
        // First dispatch is cancellation/project/review fenced. Once possible,
        // retries reconcile the SAME admitted effect even after cancellation.
        if count == 0 {
            let v = crate::execution::authority::load_locked(
                &self.state,
                &mut tx,
                &r.report.binding.session,
                crate::execution::authority::Policy::IdentityOnly,
            )
            .await?;
            if v.revoked || v.cancellation_id.is_some() || !v.holds_task || !v.stopped {
                return Err(conflict());
            }
            let source = crate::source_retention::load_confirmed_locked(
                &mut tx,
                peek.get("source_operation_id"),
                &r,
            )
            .await?;
            observation
                .as_ref()
                .ok_or_else(unavailable)?
                .verify_locked(&self.state, &mut tx, &r, &source)
                .await?;
            let decision: Uuid = sqlx::query_scalar(
                "SELECT decision_id FROM profiled_settlement_evaluations WHERE operation_id=$1",
            )
            .bind(e.operation_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
            self.reviews
                .combined_completion_in_tx(&mut tx, decision)
                .await?;
        }
        let time = now(&self.state)?;
        if time < row.get::<i64, _>("created_at_ms")
            || time >= row.get::<i64, _>("created_at_ms").saturating_add(60000)
        {
            let s = status(&row)?;
            tx.commit().await.map_err(db)?;
            return Ok(s);
        }
        let lease = Uuid::new_v4();
        sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.1',true)")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("UPDATE profiled_task_completions SET state='possible',dispatch_count=dispatch_count+1,lease_id=$2,lease_expires_at_ms=$3 WHERE operation_id=$1").bind(operation).bind(lease).bind(time.checked_add(15000).ok_or_else(unavailable)?).execute(&mut *tx).await.map_err(db)?;
        let payload: Vec<u8> = row.get("payload_bytes");
        let workspace = row.get("workspace_id");
        if count == 0 {
            observation
                .as_ref()
                .ok_or_else(unavailable)?
                .validate_fresh(&self.state)?;
        }
        tx.commit().await.map_err(db)?;
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            crate::agent_access::settlement_post(rt, workspace, payload),
        )
        .await;
        let mut tx = self.tx().await?;
        let fresh = results::load_locked(&self.state, &mut tx, r.report.report_id).await?;
        let ledger = effects::inspect_aq_effects_locked(&self.state, &mut tx, &fresh).await?;
        if fresh.receipt_artifact != e.report
            || !fresh.is_current_report_head
            || ledger.digest() != &e.ledger_digest
            || ledger.disposition() != effects::LedgerDisposition::Settled
        {
            return Err(conflict());
        }
        let current =
            sqlx::query("SELECT * FROM profiled_task_completions WHERE operation_id=$1 FOR UPDATE")
                .bind(operation)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
        actor.fence(&mut tx).await?;
        if current.get::<Option<Uuid>, _>("lease_id") != Some(lease) {
            let s = status(&current)?;
            tx.commit().await.map_err(db)?;
            return Ok(s);
        }
        sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.1',true)")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let mut next = "possible";
        let mut receipt = None;
        if let Ok(Ok((http, raw))) = outcome {
            if http.is_success() {
                if let Ok(r) = confirmed(&raw, &current) {
                    next = "confirmed";
                    receipt = Some(r)
                }
            } else if http == axum::http::StatusCode::CONFLICT {
                next = "conflict"
            }
        }
        let event=receipt.as_ref().map(|receipt|json!({"event_id":operation,"producer":"aq.task.settlement@1","event":"task.completed","attempt_id":e.attempt_id,"evaluation_operation_id":e.operation_id,"report":e.report,"release_id":e.release_id,"receipt":receipt}));
        sqlx::query("UPDATE profiled_task_completions SET state=$2,receipt=$3,outbox=$4,lease_id=NULL,lease_expires_at_ms=NULL WHERE operation_id=$1").bind(operation).bind(next).bind(receipt.map(serde_json::to_value).transpose().map_err(|_|crate::error::AppError::Internal)?).bind(event).execute(&mut *tx).await.map_err(db)?;
        if next == "confirmed" {
            self.refresh_settled_attempt(&mut tx, e.attempt_id).await?;
        }
        let row = sqlx::query("SELECT * FROM profiled_task_completions WHERE operation_id=$1")
            .bind(operation)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        let s = status(&row)?;
        tx.commit().await.map_err(db)?;
        Ok(s)
    }

    /// Preserve the immutable launch fields and project the owner-verified hot
    /// state only after Graph confirmation. Replay also repairs older snapshots.
    async fn refresh_settled_attempt(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        attempt: Uuid,
    ) -> Result<()> {
        sqlx::query("UPDATE execution_attempts a SET state=a.state || (h.state-ARRAY['owner','preparation_operation_id']),holds_task=false,execution_deadline_ms=(h.state->>'execution_deadline_ms')::bigint FROM profiled_authority_hot h WHERE a.attempt_id=$1 AND h.attempt_id=a.attempt_id AND (a.holds_task OR a.execution_deadline_ms IS DISTINCT FROM (h.state->>'execution_deadline_ms')::bigint OR a.state IS DISTINCT FROM a.state || (h.state-ARRAY['owner','preparation_operation_id']))")
            .bind(attempt)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        Ok(())
    }
}
