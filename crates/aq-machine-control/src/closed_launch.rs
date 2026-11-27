//! `attempt.settle_closed_launch`: the owner's (or its project supervisor's) attested
//! settlement of a launch its worker closed before Started whose evidence shows the launch is
//! over, though not that the harness never started (a pane existed). The server's closure
//! settlement owner runs its ordinary settlement with this attestation in place of
//! `proves_never_started`, only when every condition below holds under its node ->
//! attempt/hot locks, and records it in the same transaction. Nothing here disposes of the
//! task: its next attempt is an ordinary replacement, whose git grant leases the task ref
//! (the retained branch head) at offer time. The automatic sweep never passes an attestation.
use crate::Error;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// Without an attestation only a closure that proves nonstart settles.
pub const NOT_PROVEN: &str = "closure_does_not_prove_nonstart";
/// Condition 1: the launch closure is a host stopped proof whose kernel wait saw no children.
pub const NOT_HOST_STOPPED: &str = "closure_not_host_stopped";

/// One attestation as the settlement owner checks and records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attestation {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub actor_id: Uuid,
    pub actor: String,
    pub reason: String,
    /// The head central aq-git showed for the attempt's granted task ref before the locks
    /// (`Some(None)`: no such branch); `None` when it was not read (no grant, or no answer).
    pub task_ref: Option<Option<String>>,
}

impl Attestation {
    pub fn new(
        operation_id: Uuid,
        reason: String,
        project_id: Uuid,
        (actor_id, actor): (Uuid, String),
        task_ref: Option<Option<String>>,
    ) -> Self {
        Self {
            operation_id,
            project_id,
            actor_id,
            actor,
            reason,
            task_ref,
        }
    }
}

/// Whether the attempt's agent made any AQ-mediated effect: a graph operation, or mail sent
/// from its session (a retry or closure would leave or repeat it).
pub async fn effects_present(conn: &mut PgConnection, attempt: Uuid) -> Result<bool, Error> {
    let operations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_graph_operations WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&mut *conn)
            .await?;
    let agent: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT binding FROM agent_launch_bindings WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_optional(&mut *conn)
            .await?;
    let mailed = match &agent {
        None => false,
        Some(binding) => {
            let key =
                aq_agent_api::digest(&serde_json::json!({"kind":"session","binding":binding}))
                    .map_err(|_| Error::Invalid("agent binding"))?;
            sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mailbox_operations WHERE sender_key=$1)",
            )
            .bind(key)
            .fetch_one(&mut *conn)
            .await?
        }
    };
    Ok(operations != 0 || mailed)
}

/// Why a launch that never started cannot be settled as such: it has execution history (a
/// Started, Stopped, result or source transfer) or made an AQ-mediated effect.
pub async fn used(conn: &mut PgConnection, attempt: Uuid) -> Result<Option<&'static str>, Error> {
    let history: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_started_receipts WHERE attempt_id=$1) \
         OR EXISTS(SELECT 1 FROM profiled_stopped_receipts WHERE attempt_id=$1) \
         OR EXISTS(SELECT 1 FROM profiled_result_reports WHERE attempt_id=$1) \
         OR EXISTS(SELECT 1 FROM source_transfers WHERE attempt_id=$1)",
    )
    .bind(attempt)
    .fetch_one(&mut *conn)
    .await?;
    Ok(if history {
        Some("closure_has_execution_history")
    } else if effects_present(conn, attempt).await? {
        Some("closure_effects_present")
    } else {
        None
    })
}

/// The facts of every condition, read on the caller's locked transaction. The restart is a
/// registration of the closure's node under another boot, registered (server clock) after the
/// closure was recorded: the worker process that ran the launch is gone.
const FACTS: &str = "SELECT a.task_id, c.receipt_digest, r.boot_id AS closure_boot, \
 c.evidence_kind='host_stopped' AND COALESCE((convert_from(c.request_bytes,'UTF8')::jsonb\
 #>>'{evidence,proof,observation,kernel_wait_echild}')::BOOLEAN,FALSE) AS host_stopped, \
 (h.state#>>'{grant,expires_at_ms}')::BIGINT AS expires_at_ms, \
 h.state->>'status'='cancelling' OR COALESCE((h.state->>'revoked')::BOOLEAN,FALSE) AS revoked, \
 (SELECT n.boot_id FROM profiled_native_registration_history n WHERE n.node_id=c.node_id \
  AND n.boot_id<>r.boot_id AND (convert_from(n.metadata_bytes,'UTF8')::jsonb\
  ->>'registered_at_ms')::BIGINT>c.recorded_at_ms ORDER BY n.recorded_at_ms DESC LIMIT 1) AS restart_boot, \
 g.attempt_id IS NOT NULL AS granted, g.lease \
 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
 JOIN profiled_launch_closures c USING(attempt_id) \
 JOIN profiled_native_registration_history r ON r.registration_id=c.registration_id \
 LEFT JOIN delivery_git_grants g ON g.attempt_id=a.attempt_id WHERE a.attempt_id=$1";

/// Checks the conditions in order under the settlement owner's locks and, when all hold,
/// records the attestation; otherwise the code of the first that failed (the caller then
/// changes nothing). Without an attestation it is always [`NOT_PROVEN`].
pub async fn check(
    conn: &mut PgConnection,
    attempt: Uuid,
    attest: Option<&Attestation>,
    now_ms: i64,
) -> Result<Result<(), &'static str>, Error> {
    let Some(a) = attest else {
        return Ok(Err(NOT_PROVEN));
    };
    let Some(f) = sqlx::query(FACTS)
        .bind(attempt)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(Err(NOT_HOST_STOPPED));
    };
    let expires: Option<i64> = f.get("expires_at_ms");
    let restart: Option<Uuid> = f.get("restart_boot");
    let lease: Option<String> = f.get("lease");
    let expired = matches!(expires, Some(at) if at <= now_ms) && f.get::<bool, _>("revoked");
    // The task ref moved under the lease after the launch, or aq-git did not answer.
    let moved = f.get::<bool, _>("granted") && a.task_ref.as_ref() != Some(&lease);
    let failed = match () {
        _ if !f.get::<bool, _>("host_stopped") => Some(NOT_HOST_STOPPED),
        _ if !expired => Some("closure_grant_not_expired"),
        _ if restart.is_none() => Some("closure_node_not_restarted"),
        _ => used(conn, attempt)
            .await?
            .or(moved.then_some("closure_task_ref_moved")),
    };
    if let Some(code) = failed {
        return Ok(Err(code));
    }
    let evidence = serde_json::json!({
        "closure_receipt_digest": f.get::<String, _>("receipt_digest"),
        "closure_boot_id": f.get::<Uuid, _>("closure_boot"), "restart_boot_id": restart,
        "grant_expires_at_ms": expires, "lease": lease, "task_ref_head": a.task_ref,
    });
    sqlx::query("INSERT INTO closed_launch_attestations(attempt_id,operation_id,task_id,project_id,actor_id,actor,reason,evidence,recorded_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(attempt).bind(a.operation_id).bind(f.get::<Uuid, _>("task_id")).bind(a.project_id)
        .bind(a.actor_id).bind(&a.actor).bind(&a.reason).bind(evidence).bind(now_ms)
        .execute(&mut *conn).await?;
    Ok(Ok(()))
}

/// Admission before the settlement: a valid request for an attempt launched in the named
/// project (the immutable launch project), and an operation ID not already used otherwise.
pub async fn admit(pool: &PgPool, attempt: Uuid, a: &Attestation) -> Result<(), Error> {
    let reason = a.reason.trim();
    if a.operation_id.is_nil() || reason.is_empty() || a.reason.len() > 4096 {
        return Err(Error::Invalid("invalid attestation identity or reason"));
    }
    let in_project: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_launches WHERE attempt_id=$1 AND execution_spec#>>'{lifecycle,project_id}'=$2::text)")
        .bind(attempt).bind(a.project_id).fetch_one(pool).await?;
    if !in_project {
        return Err(Error::Refused("attempt_not_in_project"));
    }
    let used: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM closed_launch_attestations WHERE (operation_id=$1 OR attempt_id=$2) AND NOT (operation_id=$1 AND attempt_id=$2 AND project_id=$3 AND actor_id=$4 AND reason=$5))")
        .bind(a.operation_id).bind(attempt).bind(a.project_id).bind(a.actor_id).bind(&a.reason)
        .fetch_one(pool).await?;
    if used {
        return Err(Error::Refused("closed_launch_already_attested"));
    }
    Ok(())
}

/// The recorded attestation of exactly this request, if settled: a replay returns it again.
pub async fn receipt(
    pool: &PgPool,
    attempt: Uuid,
    a: &Attestation,
) -> Result<Option<serde_json::Value>, Error> {
    Ok(sqlx::query_scalar("SELECT to_jsonb(t) FROM closed_launch_attestations t WHERE operation_id=$1 AND attempt_id=$2 AND project_id=$3 AND actor_id=$4 AND reason=$5")
        .bind(a.operation_id).bind(attempt).bind(a.project_id).bind(a.actor_id).bind(&a.reason)
        .fetch_optional(pool).await?)
}
