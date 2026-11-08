//! Swarm phase 1, D2 ruling C: a restarted node releases an attempt whose host was lost. The
//! SQL facts behind the release: ownership, the uncertain hold, the outside proof checked
//! against the attempt's own Started record, and the release record.
use crate::Error;
use aq_protocol::{LostAttemptRelease, LostReason, StopProof};
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// First `state_format_version` with `lost_attempt_releases`.
pub const LOST_FORMAT: u32 = 102;
pub async fn has_lost_format(conn: &mut PgConnection) -> Result<bool, Error> {
    let v: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
        .fetch_one(&mut *conn)
        .await?;
    Ok(v.parse::<u32>().map_err(|_| Error::Invalid("format"))? >= LOST_FORMAT)
}

/// A held attempt the node may release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lost {
    pub task_id: Uuid,
    pub reason: LostReason,
    pub charged: bool,
}
/// The recorded release of an attempt, if any: (proof, reason, charged).
pub async fn release_of(
    conn: &mut PgConnection,
    attempt: Uuid,
) -> Result<Option<(StopProof, LostReason, bool)>, Error> {
    let Some(row) =
        sqlx::query("SELECT proof,charged FROM lost_attempt_releases WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_optional(&mut *conn)
            .await?
    else {
        return Ok(None);
    };
    let proof: StopProof = serde_json::from_value(row.get("proof"))
        .map_err(|_| Error::Invalid("stored lost proof"))?;
    let reason = LostReason::of(&proof).ok_or(Error::Invalid("stored lost proof"))?;
    Ok(Some((proof, reason, row.get("charged"))))
}

/// The checks that come first, before anything about the attempt is revealed (a replay
/// included), under the caller's transaction (`FOR UPDATE` on the attempt): the request's
/// shape, a non-guardian proof, and that the attempt is this node's.
pub async fn owned_locked(
    conn: &mut PgConnection,
    node: Uuid,
    request: &LostAttemptRelease,
) -> Result<LostReason, Error> {
    if request.schema_version != 1 {
        return Err(Error::Invalid("schema_version"));
    }
    let reason = LostReason::of(&request.proof).ok_or(Error::Invalid(
        "lost_attempt_guardian_proof: a guardian's own stop is reported as stopped",
    ))?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
            .bind(request.attempt_id)
            .fetch_optional(&mut *conn)
            .await?;
    match owner == Some(node) {
        true => Ok(reason),
        false => Err(Error::Invalid("attempt_not_owned")),
    }
}
/// Checks a new release of an owned attempt (`owned_locked` first):
/// - it still holds its task, was granted (status granted or cancelling, with a grant) and has
///   no known stop (`replacement_prior_execution_uncertain`); an attempt never granted is closed
///   through the launch closure instead;
/// - no result report or stopped receipt is on record;
/// - a reboot proof names the boot the guardian recorded in the attempt's Started report.
pub async fn check_locked(
    conn: &mut PgConnection,
    node: Uuid,
    request: &LostAttemptRelease,
) -> Result<Lost, Error> {
    let reason = owned_locked(conn, node, request).await?;
    let row = sqlx::query(
        "SELECT a.node_id, a.task_id, a.holds_task, h.state->'stop'->>'kind' AS stop, h.state->>'status' AS status, \
         COALESCE(jsonb_typeof(h.state->'grant')='object', FALSE) AS granted, \
         s.request_bytes FROM execution_attempts a \
         LEFT JOIN profiled_authority_hot h ON h.attempt_id = a.attempt_id \
         LEFT JOIN profiled_started_receipts s ON s.attempt_id = a.attempt_id \
         WHERE a.attempt_id=$1 FOR UPDATE OF a",
    )
    .bind(request.attempt_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row.filter(|r| r.get::<Uuid, _>("node_id") == node) else {
        return Err(Error::Invalid("attempt_not_owned"));
    };
    let status = row.get::<Option<String>, _>("status");
    if matches!(status.as_deref(), Some("offered" | "preparing")) {
        return Err(Error::Refused(
            "lost_attempt_not_granted: an attempt never granted is closed through the launch closure",
        ));
    }
    let held = row.get::<bool, _>("holds_task")
        && row.get::<bool, _>("granted")
        && matches!(
            row.get::<Option<String>, _>("stop").as_deref(),
            Some("running" | "unknown")
        )
        && matches!(status.as_deref(), Some("granted" | "cancelling"));
    if !held {
        return Err(Error::Refused(
            "lost_attempt_not_held: the attempt is not held as prior execution uncertain",
        ));
    }
    let reported: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_result_reports WHERE attempt_id=$1) \
         OR EXISTS(SELECT 1 FROM profiled_stopped_receipts WHERE attempt_id=$1)",
    )
    .bind(request.attempt_id)
    .fetch_one(&mut *conn)
    .await?;
    if reported {
        return Err(Error::Refused(
            "lost_attempt_reported: its result or stop is on record",
        ));
    }
    let guardian_boot = row
        .get::<Option<Vec<u8>>, _>("request_bytes")
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            v["observation"]["guardian"]["machine_boot_id"]
                .as_str()
                .map(str::to_owned)
        });
    match (&request.proof, &guardian_boot) {
        (StopProof::BootChanged { .. }, None) => {
            return Err(Error::Refused(
                "lost_attempt_boot_unknown: no Started report records the guardian's boot",
            ));
        }
        (proof, boot) => proof
            .validate(boot.as_deref().unwrap_or_default())
            .map_err(|_| Error::Invalid("lost_attempt_proof: the proof's facts do not hold"))?,
    }
    Ok(Lost {
        task_id: row.get("task_id"),
        reason,
        charged: reason == LostReason::LostAfterCrash,
    })
}

/// Records why B4 still holds the released attempt's task (none once it is released), so
/// the owner can see it.
pub async fn record_held(
    conn: &mut PgConnection,
    attempt: Uuid,
    held: Option<&str>,
) -> Result<(), Error> {
    sqlx::query("UPDATE lost_attempt_releases SET held_reason=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(held)
        .execute(&mut *conn)
        .await?;
    Ok(())
}
/// Records the release once; a replay with the same proof is accepted, another refused.
pub async fn record(
    conn: &mut PgConnection,
    node: Uuid,
    request: &LostAttemptRelease,
    lost: &Lost,
    receipt_digest: &str,
    now: DateTime<Utc>,
) -> Result<(), Error> {
    let proof = serde_json::to_value(&request.proof).map_err(|_| Error::Invalid("proof"))?;
    sqlx::query(
        "INSERT INTO lost_attempt_releases(attempt_id,node_id,proof_kind,proof,reason,charged,receipt_digest,recorded_at) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(request.attempt_id)
    .bind(node)
    .bind(request.proof.kind())
    .bind(proof)
    .bind(match lost.reason {
        LostReason::LostAfterReboot => "lost_after_reboot",
        LostReason::LostAfterCrash => "lost_after_crash",
    })
    .bind(lost.charged)
    .bind(receipt_digest)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The system comment on a task that B4 holds after its attempt was lost (a fixed template,
/// no worker text): the hold is where the owner acts. Posted once per attempt.
pub async fn post_hold_comment(
    pool: &sqlx::PgPool,
    attempt: Uuid,
    thread: aq_task_comments::Thread,
    reason: LostReason,
    held: &str,
    now: DateTime<Utc>,
) {
    let lost = match reason {
        LostReason::LostAfterCrash => "a crash",
        LostReason::LostAfterReboot => "a reboot",
    };
    let body = format!(
        "AQ: this task is held. Its last attempt ({attempt}) was lost after {lost} and its retry budget is exhausted after lost attempts (hold reason: {held}). An owner can retire the task or release the hold."
    );
    let author = aq_task_comments::Author::System {
        component: "aq.swarm".into(),
    };
    let id = Uuid::new_v5(&attempt, b"aq.lost-attempt-hold");
    if let Err(e) = aq_task_comments::add(pool, id, thread, &author, &body, now).await {
        tracing::warn!(%attempt, error = ?e, "lost-attempt hold comment not posted");
    }
}
