//! Inspection and observation-only recovery of an exact retained result.
//! Never publishes, releases source, accepts a result or changes a review gate.
use crate::{
    DeliveryError,
    commands::{Completion, Delivery, DeliveryTarget, FrozenCommand, task_branch},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;
type Result<T> = std::result::Result<T, DeliveryError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub project_id: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub task_id: Uuid,
    pub attempt_id: Uuid,
    pub report_id: Uuid,
    pub revision: String,
    pub head: String,
}
impl Identity {
    pub fn matches(&self, request: &Value, subject: &str) -> bool {
        subject == self.task_id.to_string()
            && request["project"] == self.project_id.to_string()
            && request["task"] == self.task_id.to_string()
            && request["result_ready"]["project_id"] == self.project_id.to_string()
            && request["result_ready"]["task_id"] == self.task_id.to_string()
            && request["result_ready"]["attempt_id"] == self.attempt_id.to_string()
            && request["result_ready"]["result_id"] == self.report_id.to_string()
            && request["result_head"] == self.head
    }
    pub fn matches_target(&self, request: &Value, target: &DeliveryTarget) -> bool {
        target.git.repository_id == self.repository_id
            && target.git.binding_generation == self.binding_generation
            && request.get("publication_target").is_none_or(|frozen| {
                serde_json::to_value(target).is_ok_and(|current| *frozen == current)
            })
    }
}
/// The admitted repository, report digest and centrally retained commit must all agree.
pub async fn identity(
    pool: &PgPool,
    project: Uuid,
    report: Uuid,
    revision: &str,
    repository: Uuid,
) -> Result<Identity> {
    let row = sqlx::query(
        "SELECT a.task_id,r.attempt_id,p.head,g.repository_id,
         (l.execution_spec#>>'{source,repository_binding_generation}')::bigint AS generation
         FROM profiled_result_reports r JOIN execution_attempts a USING(attempt_id)
         JOIN profiled_launches l USING(attempt_id)
         JOIN delivery_git_grants g USING(attempt_id)
         JOIN delivery_pushed_results p ON p.report_id=r.report_id AND p.attempt_id=r.attempt_id
         WHERE r.report_id=$1 AND r.report_digest=$2 AND g.repository_id=$3
         AND l.execution_spec#>>'{source,repository_id}'=$3::uuid::text
         AND l.execution_spec#>>'{lifecycle,project_id}'=$4::uuid::text
         AND p.head IS NOT NULL AND p.task_ref='refs/heads/aq/'||a.task_id::text",
    )
    .bind(report)
    .bind(revision)
    .bind(repository)
    .bind(project)
    .fetch_optional(pool)
    .await?
    .ok_or(DeliveryError::InvalidInput(
        "exact retained result not found",
    ))?;
    Ok(Identity {
        project_id: project,
        repository_id: repository,
        binding_generation: u64::try_from(row.get::<i64, _>("generation"))
            .map_err(|_| DeliveryError::Protocol)?,
        task_id: row.get("task_id"),
        attempt_id: row.get("attempt_id"),
        report_id: report,
        revision: revision.into(),
        head: row.get("head"),
    })
}
/// Only structured, bounded diagnostics leave the publisher. No raw stderr or credentials.
fn summary(id: Uuid, status: &str, request: &Value, output: &Value) -> Value {
    fn oid(value: &Value) -> Option<&str> {
        value
            .as_str()
            .filter(|s| s.len() == 40 && crate::commands::object_id(s))
    }
    let status =
        matches!(status, "running" | "published" | "conflict" | "refused").then_some(status);
    let diagnostic = request["answer_note"].as_str().filter(|s| {
        matches!(
            *s,
            "remote_rejected"
                | "rejected"
                | "git_transport"
                | "git_protocol"
                | "git_error"
                | "reconciliation_required"
                | "publication_not_dispatched"
        )
    });
    let reason = output["reason"]
        .as_str()
        .filter(|s| matches!(*s, "no_branch" | "no_access" | "unreachable"));
    json!({"operation_id":id,"status":status,"expected_old":oid(&request["expected_old"]),
        "head":oid(&request["result_head"]),"diagnostic":diagnostic,
        "reason":reason,"rejection_detail":"Raw remote rejection text is not exposed by this host; do not infer a permission change from this diagnostic."})
}
/// UUID keyset pagination preserves access to all original and subsequent identities.
pub async fn inspect(pool: &PgPool, identity: &Identity, after: Option<Uuid>) -> Result<Value> {
    let rows = sqlx::query(
        "SELECT operation_id,subject_id,status,request,output FROM playbook_host_operations
        WHERE command_name='branch.publish' AND request#>>'{result_ready,result_id}'=$1
        AND subject_id=$3 AND request->>'task'=$3 AND request#>>'{result_ready,task_id}'=$3
        AND request->>'project'=$4 AND request#>>'{result_ready,project_id}'=$4
        AND request#>>'{result_ready,attempt_id}'=$5 AND request->>'result_head'=$6
        AND ($2::uuid IS NULL OR operation_id>$2) ORDER BY operation_id LIMIT 26",
    )
    .bind(identity.report_id.to_string())
    .bind(after)
    .bind(identity.task_id.to_string())
    .bind(identity.project_id.to_string())
    .bind(identity.attempt_id.to_string())
    .bind(&identity.head)
    .fetch_all(pool)
    .await?;
    let more = rows.len() > 25;
    let mut operations = Vec::new();
    let mut cursor = None;
    for row in rows.into_iter().take(25) {
        let request: Value = row.get("request");
        let id: Uuid = row.get("operation_id");
        cursor = Some(id);
        if identity.matches(&request, row.get("subject_id")) {
            operations.push(summary(
                id,
                row.get("status"),
                &request,
                &row.get::<Option<Value>, _>("output").unwrap_or(Value::Null),
            ));
        }
    }
    Ok(
        json!({"identity":identity,"operations":operations,"next_after":more.then_some(cursor).flatten(),
        "recovery":"result.publication.reconcile binds the original aq-git journal and reconciles only pending dispatched or indeterminate records under its frozen generation; it never pushes. If refused or absent, an operator must inspect aq-git's rejection and decide the narrow remote-policy correction. No retry, force, worker replacement or claim release is authorized."}),
    )
}
/// Validate the immutable result/repository before any publication-related read.
pub async fn validate_target(
    pool: &PgPool,
    frozen: FrozenCommand<'_>,
    target: &DeliveryTarget,
) -> Result<Uuid> {
    let report = frozen.request["result_ready"]["result_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or(DeliveryError::InvalidInput(
            "publication requires exact result",
        ))?;
    let project = frozen.request["project"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or(DeliveryError::InvalidInput("project"))?;
    let revision: String =
        sqlx::query_scalar("SELECT report_digest FROM profiled_result_reports WHERE report_id=$1")
            .bind(report)
            .fetch_one(pool)
            .await?;
    let admitted = identity(pool, project, report, &revision, target.git.repository_id).await?;
    if !admitted.matches(frozen.request, &admitted.task_id.to_string())
        || !admitted.matches_target(frozen.request, target)
    {
        return Err(DeliveryError::InvalidInput(
            "publication identity or repository binding differs",
        ));
    }
    Ok(report)
}
/// Historical requests and committed dispatches both forbid another push.
pub async fn dispatched(
    db: impl sqlx::PgExecutor<'_>,
    operation: Uuid,
    report: Uuid,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM retained_publication_dispatches WHERE report_id=$2)
        OR EXISTS(SELECT 1 FROM playbook_host_operations WHERE command_name='branch.publish'
        AND operation_id<>$1 AND request#>>'{result_ready,result_id}'=$2::uuid::text
        AND NOT (status='refused' AND request->>'answer_note' IS NOT DISTINCT FROM 'publication_not_dispatched'))",
    )
    .bind(operation)
    .bind(report)
    .fetch_one(db)
    .await
}
/// Freeze the dispatch immediately before calling Git, after read-only preflight.
/// A new UUID cannot evade an original dispatch or a historical attempt.
pub async fn reserve(
    pool: &PgPool,
    frozen: FrozenCommand<'_>,
    target: &DeliveryTarget,
) -> Result<bool> {
    let report = validate_target(pool, frozen, target).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,179))")
        .bind(report.to_string())
        .execute(&mut *tx)
        .await?;
    let prior = dispatched(&mut *tx, frozen.operation, report).await?;
    let inserted = if prior {
        false
    } else {
        sqlx::query("INSERT INTO retained_publication_dispatches(report_id,operation_id,request,target) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING")
            .bind(report).bind(frozen.operation).bind(frozen.request).bind(serde_json::to_value(target)?)
            .execute(&mut *tx).await?.rows_affected() == 1
    };
    tx.commit().await?;
    Ok(inserted)
}
/// Repeated request IDs replay the immutable observation. A new ID may observe again.
pub async fn reconcile(
    pool: &PgPool,
    delivery: &Delivery,
    target: &DeliveryTarget,
    identity: &Identity,
    operation: Uuid,
    original: Uuid,
    actor: Uuid,
) -> Result<Value> {
    let recorded = self::identity(
        pool,
        identity.project_id,
        identity.report_id,
        &identity.revision,
        identity.repository_id,
    )
    .await?;
    if recorded != *identity {
        return Err(DeliveryError::InvalidInput("retained identity differs"));
    }
    if operation.is_nil() || operation == original {
        return Err(DeliveryError::InvalidInput(
            "recovery needs its own operation ID",
        ));
    }
    let request = json!({"identity":identity,"target":target,"publication_operation_id":original,"actor_id":actor});
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,179))")
        .bind(operation.to_string())
        .execute(&mut *tx)
        .await?;
    if let Some(row) = sqlx::query(
        "SELECT request,evidence FROM retained_publication_recoveries WHERE operation_id=$1",
    )
    .bind(operation)
    .fetch_optional(&mut *tx)
    .await?
    {
        if row.get::<Value, _>("request") != request {
            return Err(DeliveryError::IdempotencyConflict);
        }
        return Ok(row.get("evidence"));
    }
    let eligible: Option<bool> = sqlx::query_scalar(
        "SELECT a.holds_task AND h.report_id=$2
         AND ah.state#>>'{stop,kind}'='stopped' AND ah.state->'revoked'='false'::jsonb
         AND ah.state->>'cancellation_id' IS NULL
         FROM execution_attempts a JOIN profiled_result_heads h USING(attempt_id)
         JOIN profiled_authority_hot ah USING(attempt_id) WHERE a.attempt_id=$1 FOR UPDATE OF a,h",
    )
    .bind(identity.attempt_id)
    .bind(identity.report_id)
    .fetch_optional(&mut *tx)
    .await?;
    if eligible != Some(true) {
        return Err(DeliveryError::InvalidInput(
            "result is no longer held and eligible",
        ));
    }
    let row = sqlx::query("SELECT request,subject_id,status,output FROM playbook_host_operations WHERE operation_id=$1 AND command_name='branch.publish'")
        .bind(original).fetch_optional(&mut *tx).await?.ok_or(DeliveryError::InvalidInput("publication not found"))?;
    let frozen: Value = row.get("request");
    if !identity.matches(&frozen, row.get("subject_id"))
        || !identity.matches_target(&frozen, target)
    {
        return Err(DeliveryError::InvalidInput(
            "publication identity or repository binding differs",
        ));
    }
    // The outer operation lock serializes callers. Persist this request in its
    // own transaction before the journal-changing reconciliation, even if this
    // transaction later loses its receipt or the process stops.
    let journal = if let Some(row) = sqlx::query("SELECT request,publisher FROM retained_publication_recovery_requests WHERE operation_id=$1")
        .bind(operation).fetch_optional(&mut *tx).await? {
        if row.get::<Value,_>("request") != request { return Err(DeliveryError::IdempotencyConflict); }
        row.get("publisher")
    } else {
        let journal = delivery.inspect_publication(target,original,identity.task_id,frozen["expected_old"].as_str(),&identity.head).await;
        sqlx::query("INSERT INTO retained_publication_recovery_requests(operation_id,publication_operation_id,report_id,request,publisher) VALUES($1,$2,$3,$4,$5)")
            .bind(operation).bind(original).bind(identity.report_id).bind(&request).bind(&journal).execute(pool).await?;
        journal
    };
    let observation = delivery
        .reconcile_publication(target, operation, original, &identity.head, &journal)
        .await;
    let evidence = json!({"operation_id":operation,"publication":summary(original,row.get("status"),&frozen,&row.get::<Option<Value>,_>("output").unwrap_or(Value::Null)),
        "identity":identity,"target_ref":task_branch(identity.task_id),
        "publisher":journal,
        "observation":observation});
    sqlx::query("INSERT INTO retained_publication_recoveries(operation_id,publication_operation_id,report_id,request,evidence) VALUES($1,$2,$3,$4,$5)")
        .bind(operation).bind(original).bind(identity.report_id).bind(request).bind(&evidence).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(evidence)
}
/// Duplicate publications may recover an existing receipt, but never send another push.
pub async fn recover_step(db: &mut PgConnection, request: &Value) -> Result<Completion> {
    let report = request["result_ready"]["result_id"]
        .as_str()
        .ok_or(DeliveryError::InvalidInput("result"))?;
    let receipt: Option<Value> = sqlx::query_scalar("SELECT evidence FROM retained_publication_recoveries WHERE report_id::text=$1 AND evidence#>>'{observation,state}'='published' ORDER BY recorded_at LIMIT 1")
        .bind(report).fetch_optional(db).await?;
    // Even a previously recovered receipt must still name this exact commit.
    if let Some(receipt) = receipt
        && receipt["identity"]["head"] == request["result_head"]
    {
        return Ok(Completion::new(
            "published",
            json!({"head":request["result_head"],"recovery_operation_id":receipt["operation_id"]}),
        ));
    }
    Ok(Completion {
        diagnostic: Some("reconciliation_required"),
        ..Completion::refused("unreachable")
    })
}

/// One selected original operation and its publisher journal. An unavailable journal
/// stays explicit; it never proves the original dispatch failed or permits a new one.
pub async fn inspect_operation(
    pool: &PgPool,
    delivery: &Delivery,
    target: &DeliveryTarget,
    identity: &Identity,
    original: Uuid,
) -> Result<Value> {
    let row = sqlx::query("SELECT request,subject_id,status,output FROM playbook_host_operations WHERE operation_id=$1 AND command_name='branch.publish'")
        .bind(original).fetch_optional(pool).await?.ok_or(DeliveryError::InvalidInput("publication not found"))?;
    let request: Value = row.get("request");
    if !identity.matches(&request, row.get("subject_id"))
        || !identity.matches_target(&request, target)
    {
        return Err(DeliveryError::InvalidInput(
            "publication identity or repository binding differs",
        ));
    }
    let receipt: Option<Value> = sqlx::query_scalar("SELECT evidence FROM retained_publication_recoveries WHERE report_id=$1 AND publication_operation_id=$2 ORDER BY recorded_at DESC,operation_id LIMIT 1")
        .bind(identity.report_id).bind(original).fetch_optional(pool).await?;
    Ok(
        json!({"publication":summary(original,row.get("status"),&request,&row.get::<Option<Value>,_>("output").unwrap_or(Value::Null)),
        "frozen_target":request["publication_target"],"recovery_receipt":receipt,
        "publisher":delivery.inspect_publication(target,original,identity.task_id,request["expected_old"].as_str(),&identity.head).await}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_summary_exposes_only_protocol_labels_and_oids() {
        let private = "private hook or remote text";
        let request = json!({"expected_old":private,"result_head":private,"answer_note":private});
        let evidence = summary(
            Uuid::new_v4(),
            private,
            &request,
            &json!({"reason":private}),
        );
        assert!(!evidence.to_string().contains(private));
        for field in ["status", "expected_old", "head", "diagnostic", "reason"] {
            assert!(evidence[field].is_null(), "{field}");
        }
        let request = json!({"expected_old":"b".repeat(40),"result_head":"a".repeat(40),"answer_note":"remote_rejected"});
        let evidence = summary(
            Uuid::new_v4(),
            "refused",
            &request,
            &json!({"reason":"unreachable"}),
        );
        assert_eq!(evidence["status"], "refused");
        assert_eq!(evidence["expected_old"], request["expected_old"]);
        assert_eq!(evidence["head"], request["result_head"]);
        assert_eq!(evidence["diagnostic"], "remote_rejected");
        assert_eq!(evidence["reason"], "unreachable");
    }
}
