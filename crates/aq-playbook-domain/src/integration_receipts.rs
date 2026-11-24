//! Namespace-scoped inspection of candidate preparation and publication receipts.
use crate::{
    Namespace, Result,
    host_values::{HostResult, handle, merged},
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub const CANDIDATE_REF: &str = "aq.candidate/";
pub const PUBLICATION_REF: &str = "aq.publication/";

pub struct IntegrationReceipts<'a> {
    pub namespace: &'a Namespace,
    pub pool: &'a PgPool,
}
impl IntegrationReceipts<'_> {
    async fn receipt_row(&self, reference: &str) -> Result<Option<sqlx::postgres::PgRow>> {
        let (command, operation) = if let Some(op) = handle(reference, CANDIDATE_REF) {
            ("candidate.prepare", op)
        } else if let Some(op) = handle(reference, PUBLICATION_REF) {
            ("candidate.publish", op)
        } else {
            return Ok(None);
        };
        Ok(sqlx::query("SELECT operation_id,command_name,state,reason,candidate_operation_id,repository_id,binding_generation,request,detail,reconcile_attempts FROM playbook_integration_receipts WHERE operation_id=$1 AND namespace_id=$2 AND generation=$3 AND command_name=$4")
            .bind(operation)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(command)
            .fetch_optional(self.pool)
            .await?)
    }

    /// operation.get for candidate and publication references; `None` for
    /// any other handle.
    pub async fn operation(&self, reference: &str) -> Result<Option<HostResult>> {
        let Some(row) = self.receipt_row(reference).await? else {
            return Ok(None);
        };
        let request: Value = row.get("request");
        let pointer = json!({"reference": request["reference"], "subject": request["subject"]});
        let state: String = row.get("state");
        let completed = |outcome: &str, disposition: &str| HostResult {
            outcome: "completed".into(),
            output: json!({"outcome": outcome, "output": merged(&pointer, json!({"disposition": disposition}))}),
        };
        Ok(Some(match state.as_str() {
            "pending" => HostResult {
                outcome: "running".into(),
                output: pointer,
            },
            "ambiguous" => HostResult {
                outcome: "indeterminate".into(),
                output: pointer,
            },
            "prepared" | "published" => completed("completed", "accepted"),
            "conflict" => completed("completed", "failed"),
            _ => completed("refused", "failed"),
        }))
    }

    /// Inspection: the typed receipt behind a candidate or publication
    /// reference in this namespace generation.
    pub async fn inspect(&self, reference: &str) -> Result<Option<Value>> {
        let Some(row) = self.receipt_row(reference).await? else {
            return Ok(None);
        };
        Ok(Some(json!({
            "operation": row.get::<Uuid, _>("operation_id"),
            "command": row.get::<String, _>("command_name"),
            "state": row.get::<String, _>("state"),
            "reason": row.get::<Option<String>, _>("reason"),
            "candidate_operation": row.get::<Uuid, _>("candidate_operation_id"),
            "repository_id": row.get::<Option<Uuid>, _>("repository_id"),
            "binding_generation": row.get::<Option<i64>, _>("binding_generation"),
            "request": row.get::<Value, _>("request"),
            "detail": row.get::<Option<Value>, _>("detail"),
            "reconcile_attempts": row.get::<i32, _>("reconcile_attempts"),
        })))
    }
}
