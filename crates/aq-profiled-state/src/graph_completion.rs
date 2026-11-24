//! Durable state for an admitted automatic Graph completion.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, Row, postgres::PgRow};
use uuid::Uuid;

pub const VERSION_CONFLICT: &str = "graph_version_conflict";
pub const RECEIPT_CONFLICT: &str = "graph_completion_receipt_conflict";
pub const GRAPH_PERMISSION: &str = "graph_completion_permission_required";
pub const GRAPH_IDENTITY: &str = "graph_completion_identity_required";
pub const GRAPH_PENDING: &str = "graph_completion_receipt_pending";

pub fn terminal_conflict(reason: &str) -> bool {
    matches!(reason, VERSION_CONFLICT | RECEIPT_CONFLICT)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoDisposition {
    /// Not completed; waiting on evidence or a re-evaluation.
    Held,
    /// Refused with evidence; terminal for this report.
    ReworkRequired,
    Prepared,
    /// Dispatched; the Graph outcome is not yet known.
    Possible,
    /// A validated Graph receipt completed the task.
    Confirmed,
}
impl AutoDisposition {
    fn parse(s: &str) -> Result<Self, sqlx::Error> {
        Ok(match s {
            "held" => Self::Held,
            "rework_required" => Self::ReworkRequired,
            "prepared" => Self::Prepared,
            "possible" => Self::Possible,
            "confirmed" => Self::Confirmed,
            _ => {
                return Err(sqlx::Error::Protocol(
                    "invalid automatic settlement status".into(),
                ));
            }
        })
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct AutoSettlement {
    pub report_id: Uuid,
    pub disposition: AutoDisposition,
    pub reason: Option<String>,
    pub operation_id: Option<Uuid>,
    pub dispatch_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_graph_check_at_ms: Option<i64>,
    pub evidence: Value,
}
pub fn status(row: &PgRow) -> Result<AutoSettlement, sqlx::Error> {
    Ok(AutoSettlement {
        report_id: row.get("report_id"),
        disposition: AutoDisposition::parse(row.get("disposition"))?,
        reason: row.get("reason"),
        operation_id: row.get("operation_id"),
        dispatch_count: u32::try_from(row.get::<i32, _>("dispatch_count"))
            .map_err(|_| sqlx::Error::Protocol("invalid Graph dispatch count".into()))?,
        next_graph_check_at_ms: row.get("next_graph_check_at_ms"),
        evidence: row.get("evidence"),
    })
}
pub async fn fence_transaction(db: &mut PgConnection, owner: &str) -> sqlx::Result<()> {
    sqlx::query("SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2000ms',true),set_config('aq.settlement_owner',$1,true)")
        .bind(owner)
        .execute(db)
        .await?;
    Ok(())
}

/// The locked automatic settlement, if any, of the current report.
pub async fn lock_row(db: &mut PgConnection, report: Uuid) -> sqlx::Result<Option<PgRow>> {
    sqlx::query("SELECT * FROM profiled_auto_settlements WHERE report_id=$1 FOR UPDATE")
        .bind(report)
        .fetch_optional(db)
        .await
}

/// Update a settlement that has not prepared an external completion.
pub async fn record_noncompletion(
    db: &mut PgConnection,
    report: Uuid,
    attempt: Uuid,
    (actor, disposition, reason): (&str, &str, &str),
    evidence: &serde_json::Value,
    at: i64,
) -> sqlx::Result<Option<AutoSettlement>> {
    sqlx::query(
        "INSERT INTO profiled_auto_settlements(report_id,attempt_id,policy_actor,disposition,reason,evidence,created_at_ms,updated_at_ms) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$7)
         ON CONFLICT(report_id) DO UPDATE SET disposition=EXCLUDED.disposition,reason=EXCLUDED.reason,evidence=EXCLUDED.evidence,updated_at_ms=EXCLUDED.updated_at_ms
         WHERE profiled_auto_settlements.disposition='held' RETURNING *",
    )
    .bind(report)
    .bind(attempt)
    .bind(actor).bind(disposition)
    .bind(reason)
    .bind(evidence)
    .bind(at)
    .fetch_optional(db)
    .await?
    .as_ref()
    .map(status)
    .transpose()
}

/// Find a due default-policy completion without scanning finished history.
pub async fn next_due(
    pool: &sqlx::PgPool,
    after: Option<uuid::Uuid>,
    now: i64,
) -> Result<Option<uuid::Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT report_id FROM profiled_auto_settlements WHERE disposition IN ('prepared','possible') AND evidence->>'decided_by' IS DISTINCT FROM 'playbook' AND report_id > COALESCE($1,'00000000-0000-0000-0000-000000000000'::uuid) AND (next_graph_check_at_ms IS NULL OR next_graph_check_at_ms <= $2) ORDER BY report_id LIMIT 1")
        .bind(after).bind(now).fetch_optional(pool).await
}

/// Fence each round before sending a read or write to Graph.
pub async fn schedule(
    connection: &mut PgConnection,
    row: &PgRow,
    time: i64,
    actor_current: bool,
) -> Result<(AutoSettlement, Option<i64>), sqlx::Error> {
    if row
        .get::<Option<i64>, _>("next_graph_check_at_ms")
        .is_some_and(|next| next > time)
    {
        return Ok((status(row)?, None));
    }
    let report_id: Uuid = row.get("report_id");
    let sent: i32 = row.get("dispatch_count");
    let delay = (5_000_i64 << sent.clamp(0, 6)).min(300_000);
    let next = time.saturating_add(delay);
    let first = sent == 0 && actor_current;
    let recorded: Option<String> = row.get("reason");
    let reason = match (actor_current, recorded.as_deref()) {
        (false, _) => GRAPH_IDENTITY,
        (true, Some(GRAPH_PERMISSION)) => GRAPH_PERMISSION,
        (true, _) => GRAPH_PENDING,
    };
    let row = sqlx::query("UPDATE profiled_auto_settlements SET disposition=CASE WHEN $4 THEN 'possible' ELSE disposition END,dispatch_count=dispatch_count+$4::int,reason=$5,next_graph_check_at_ms=$3,updated_at_ms=$2 WHERE report_id=$1 RETURNING *")
        .bind(report_id).bind(time).bind(next).bind(first)
        .bind(reason)
        .fetch_one(connection).await?;
    Ok((status(&row)?, actor_current.then_some(next)))
}

/// Record another send before it can be observed by Graph.
pub async fn authorize_resend(
    connection: &mut PgConnection,
    report: Uuid,
    check: i64,
) -> Result<bool, sqlx::Error> {
    let outcome = sqlx::query("UPDATE profiled_auto_settlements SET dispatch_count=dispatch_count+1,reason=$3 WHERE report_id=$1 AND disposition='possible' AND next_graph_check_at_ms=$2")
        .bind(report).bind(check).bind(GRAPH_PENDING).execute(connection).await?;
    Ok(outcome.rows_affected() == 1)
}

pub async fn hold(
    connection: &mut PgConnection,
    report: Uuid,
    time: i64,
    reason: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE profiled_auto_settlements SET disposition='held',reason=$2,updated_at_ms=$3,next_graph_check_at_ms=NULL WHERE report_id=$1")
        .bind(report).bind(reason).bind(time).execute(connection).await?;
    Ok(())
}

/// Confirm the owner-validated receipt and release its original task claim in
/// the same transaction. The database still guards against a proofless release.
pub async fn confirm(
    connection: &mut PgConnection,
    report: Uuid,
    attempt: Uuid,
    time: i64,
    receipt: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE profiled_auto_settlements SET disposition='confirmed',reason=NULL,receipt=$2,updated_at_ms=$3,next_graph_check_at_ms=NULL WHERE report_id=$1")
        .bind(report).bind(receipt).bind(time).execute(&mut *connection).await?;
    sqlx::query(
        "UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1 AND holds_task",
    )
    .bind(attempt)
    .execute(connection)
    .await?;
    Ok(())
}

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

/// Verify that a schema-validated response completed this exact versioned task.
pub fn verified_receipt(raw: &Value, row: &PgRow) -> Option<CompletionReceipt> {
    let nodes = raw["results"].as_array().filter(|v| v.len() == 1)?;
    let node = &nodes[0];
    let version = u64::try_from(row.get::<i64, _>("expected_version"))
        .ok()?
        .checked_add(1)?;
    let logical = u64::try_from(row.get::<i64, _>("expected_content_revision"))
        .ok()?
        .checked_add(1)?;
    let workspace: Uuid = row.get("workspace_id");
    let task: Uuid = row.get("task_id");
    if node["id"] != serde_json::json!(task)
        || node["workspace_id"] != serde_json::json!(workspace)
        || node["kind"] != "task"
        || node["task_state"] != "completed"
        || node["deleted"] != false
        || node["retired"] != false
        || node["version"] != serde_json::json!(version)
        || node["content_revision"] != serde_json::json!(logical)
    {
        return None;
    }
    let change_set_id = raw["change_set_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
        .filter(|id| !id.is_nil())?;
    let mut canonical = raw.clone();
    canonical.sort_all_objects();
    let response = serde_json::to_vec(&canonical).ok()?;
    Some(CompletionReceipt {
        workspace_id: workspace,
        task_id: task,
        change_set_id,
        version,
        content_revision: logical,
        response_digest: aq_admission::Digest::of(&response).as_str().into(),
    })
}
