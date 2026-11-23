//! The retained asynchronous host operations and the open-decision projection.
use crate::{
    Error, Namespace, Result,
    host_values::{DECISION_OUTCOMES, HostResult, responder, revision_of, text},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgExecutor, Row};
use std::collections::BTreeSet;
use uuid::Uuid;

/// Records one host decision under the caller's namespace lock.
pub async fn record_effect(
    db: &mut PgConnection,
    namespace: &Namespace,
    (grant, revision): (Uuid, i64),
    intent: &aq_playbook_host::runner::Intent,
    identity: &str,
    (operation, status, receipt, at): (Uuid, &str, &Value, i64),
) -> Result<()> {
    sqlx::query("INSERT INTO playbook_effects(namespace_id,generation,intent_id,run_id,command_name,request_digest,command_fingerprint,input_digest,grant_id,grant_revision,operation_id,status,receipt,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
        .bind(namespace.namespace_id)
        .bind(namespace.generation)
        .bind(&intent.id)
        .bind(&intent.run)
        .bind(&intent.command.name)
        .bind(identity)
        .bind(intent.command.fingerprint().map_err(|_| Error::Invalid)?)
        .bind(&intent.input_digest)
        .bind(grant)
        .bind(revision)
        .bind(operation)
        .bind(status)
        .bind(receipt)
        .bind(at)
        .execute(db)
        .await?;
    Ok(())
}

/// The caller proved absence of the effect row and validated the original intent
/// under the same namespace lock that admission requires. The expiry marker
/// keeps that intent closed even if its engine reply is lost or the clock moves.
pub async fn close_unadmitted_delivery(
    db: &mut PgConnection,
    namespace: &Namespace,
    grant: (Uuid, i64),
    intent: &aq_playbook_host::runner::Intent,
    identity: &str,
    operation: Uuid,
    now: i64,
) -> Result<Option<HostResult>> {
    if intent.deadline_at_ms > now
        || !aq_playbook_host::DELIVERY_RECOVERY_COMMANDS.contains(&intent.command.name.as_str())
    {
        return Ok(None);
    }
    let admitted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playbook_host_operations WHERE namespace_id=$1
         AND generation=$2 AND intent_id=$3)",
    )
    .bind(namespace.namespace_id)
    .bind(namespace.generation)
    .bind(&intent.id)
    .fetch_one(&mut *db)
    .await?;
    if admitted {
        return Ok(None);
    }
    let output = json!({"reason":aq_playbook_host::DELIVERY_ADMISSION_EXPIRED});
    record_effect(
        db,
        namespace,
        grant,
        intent,
        identity,
        (operation, "expired", &output, now),
    )
    .await?;
    Ok(Some(HostResult {
        outcome: "expired".into(),
        output,
    }))
}

pub struct Operation {
    pub command: String,
    pub intent_id: String,
    pub request: Value,
    pub status: String,
    pub output: Option<Value>,
}

impl Operation {
    pub fn retained(self) -> Result<HostResult> {
        Ok(HostResult {
            outcome: self.status,
            output: self.output.ok_or(Error::Unknown)?,
        })
    }
}

#[derive(Deserialize, Serialize)]
pub struct DecisionRequest {
    pub subject: Value,
    pub question: String,
    pub choices: Vec<String>,
    pub responders: Vec<String>,
    pub expires_at_ms: i64,
}

impl DecisionRequest {
    pub fn valid(&self, now: i64) -> bool {
        let distinct = |values: &[String]| {
            !values.is_empty()
                && values
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == values.len()
        };
        distinct(&self.choices)
            && distinct(&self.responders)
            && self
                .choices
                .iter()
                .all(|c| DECISION_OUTCOMES.contains(&c.as_str()))
            && self.responders.iter().all(|r| responder(r).is_some())
            && !self.question.trim().is_empty()
            && self.expires_at_ms > now
    }

    pub fn permits(&self, actor: Uuid) -> bool {
        self.responders.iter().any(|r| responder(r) == Some(actor))
    }

    pub fn subject_id(&self) -> Option<Uuid> {
        self.subject["id"].as_str()?.parse().ok()
    }

    pub fn output(&self, operation: Uuid) -> Value {
        json!({"reference": format!("decision:{operation}"), "subject": self.subject})
    }
}

#[derive(Serialize)]
pub struct DecisionView {
    pub namespace: Namespace,
    pub handle: Uuid,
    #[serde(flatten)]
    pub request: DecisionRequest,
}

pub struct Operations<'a>(pub &'a Namespace);

impl Operations<'_> {
    pub async fn require_project_workspace(&self, db: impl PgExecutor<'_>) -> Result<()> {
        let matches: Option<bool> = sqlx::query_scalar(
            "SELECT binding @> $2 FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
        )
        .bind(self.0.project_id)
        .bind(json!({"enabled": true, "graph_workspace_id": self.0.workspace_id}))
        .fetch_optional(db)
        .await?;
        if matches != Some(true) {
            return Err(Error::Forbidden);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record(
        &self,
        db: &mut PgConnection,
        operation: Uuid,
        intent_id: &str,
        command: &str,
        subject: &Value,
        check: Option<&str>,
        request: &Value,
        now: i64,
    ) -> Result<()> {
        sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,check_id,request,status,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,'running',$10)")
            .bind(operation)
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(intent_id)
            .bind(command)
            .bind(text(subject, "id")?)
            .bind(revision_of(subject)?)
            .bind(check)
            .bind(request)
            .bind(now)
            .execute(db)
            .await?;
        Ok(())
    }

    pub async fn load(&self, db: impl PgExecutor<'_>, id: Uuid) -> Result<Option<Operation>> {
        let row = sqlx::query("SELECT command_name,intent_id,request,status,output FROM playbook_host_operations WHERE operation_id=$1 AND namespace_id=$2 AND generation=$3")
            .bind(id)
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .fetch_optional(db)
            .await?;
        Ok(row.map(|row| Operation {
            command: row.get("command_name"),
            intent_id: row.get("intent_id"),
            request: row.get("request"),
            status: row.get("status"),
            output: row.get("output"),
        }))
    }

    pub async fn complete(
        &self,
        db: &mut PgConnection,
        operation: Uuid,
        result: &HostResult,
        now: i64,
        answer_note: Option<&str>,
    ) -> Result<bool> {
        let updated = sqlx::query("UPDATE playbook_host_operations SET status=$4,output=$5,completed_at_ms=$6,request=CASE WHEN $7::text IS NULL THEN request ELSE request || jsonb_build_object('answer_note',$7::text) END WHERE operation_id=$1 AND namespace_id=$2 AND generation=$3 AND status='running'")
            .bind(operation)
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(&result.outcome)
            .bind(&result.output)
            .bind(now)
            .bind(answer_note)
            .execute(db)
            .await?
            .rows_affected();
        Ok(updated == 1)
    }

    pub async fn observe_work(
        &self,
        db: &mut PgConnection,
        limit: u32,
        now: i64,
    ) -> Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar("UPDATE playbook_host_operations SET observed_at_ms=$4 WHERE operation_id IN (SELECT operation_id FROM playbook_host_operations WHERE namespace_id=$1 AND generation=$2 AND command_name='work.ensure' AND status='running' ORDER BY COALESCE(observed_at_ms,created_at_ms),operation_id LIMIT $3) RETURNING operation_id")
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(i64::from(limit))
            .bind(now)
            .fetch_all(db)
            .await?)
    }

    pub async fn decisions(
        &self,
        db: impl PgExecutor<'_>,
        now: i64,
        handle: Option<Uuid>,
    ) -> Result<Vec<DecisionView>> {
        let rows: Vec<(Uuid, sqlx::types::Json<DecisionRequest>)> = sqlx::query_as(
            "SELECT operation_id,request FROM playbook_host_operations
             WHERE namespace_id=$1 AND generation=$2 AND command_name='decision.request'
               AND status='running' AND (request->>'expires_at_ms')::bigint>$3
               AND ($4::uuid IS NULL OR operation_id=$4)
             ORDER BY (request->>'expires_at_ms')::bigint,operation_id",
        )
        .bind(self.0.namespace_id)
        .bind(self.0.generation)
        .bind(now)
        .bind(handle)
        .fetch_all(db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(handle, request)| DecisionView {
                namespace: self.0.clone(),
                handle,
                request: request.0,
            })
            .collect())
    }
}

/// Project the produced knowledge nodes shown by a validated depth-one context
/// response. A truncated empty result cannot establish that no document exists.
pub fn produced_knowledge(context: &Value, task: Uuid) -> Option<Vec<Uuid>> {
    let task = json!(task);
    let linked: BTreeSet<_> = context["edges"]
        .as_array()?
        .iter()
        .filter(|edge| {
            edge["edge_type"] == "produces" && edge["source"] == task && edge["deleted"] == false
        })
        .filter_map(|edge| edge["target"].as_str())
        .collect();
    let ids: BTreeSet<Uuid> = context["nodes"]
        .as_array()?
        .iter()
        .filter(|node| node["kind"] == "knowledge" && node["deleted"] == false)
        .filter_map(|node| node["id"].as_str())
        .filter(|id| linked.contains(id))
        .filter_map(|id| id.parse().ok())
        .collect();
    (!ids.is_empty() || context["truncated"] == false).then(|| ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produced_knowledge_needs_live_outgoing_edges_and_knowledge_nodes() {
        let task = Uuid::from_u128(1);
        let produced = Uuid::from_u128(2);
        let old = Uuid::from_u128(3);
        let incoming = Uuid::from_u128(4);
        let child = Uuid::from_u128(5);
        let edge = |source, target| json!({"source": source, "target": target, "deleted": false, "edge_type": "produces"});
        let mut context = json!({
            "truncated": false,
            "edges": [edge(task, produced), edge(task, old), edge(incoming, task), edge(task, child)],
            "nodes": [
                {"id": produced, "kind": "knowledge", "deleted": false},
                {"id": old, "kind": "knowledge", "deleted": true},
                {"id": incoming, "kind": "knowledge", "deleted": false},
                {"id": child, "kind": "task", "deleted": false}
            ]
        });
        assert_eq!(produced_knowledge(&context, task), Some(vec![produced]));
        context["edges"][0]["deleted"] = json!(true);
        assert_eq!(produced_knowledge(&context, task), Some(vec![]));
        context["truncated"] = json!(true);
        assert_eq!(produced_knowledge(&context, task), None);
    }
}
