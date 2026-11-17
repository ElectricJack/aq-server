//! A granted attempt that never started must not hold anything forever. An attempt that was
//! granted, has no Started, Stopped, result, source transfer or launch closure on record and
//! whose grant has expired is provably not running: the server refuses a Started report and a
//! renewal once the grant has expired, under the same node -> attempt/hot locks its closing
//! takes. The server closes such an attempt itself as failed with the reason
//! [`REASON`] (stop `not_started`, receipt [`RECEIPT`]`<attempt>`), releases its task claim
//! (which frees its node's compute and its snapshot pin) and does not charge it to the
//! lineage budgets. The SQL facts of that rule live here; the server's closure settlement
//! owner writes the typed state.
use crate::Error;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// The failure reason of a lapsed attempt.
pub const REASON: &str = "granted_never_started";
/// The stop receipt id of a lapsed attempt is this prefix and its attempt id.
pub const RECEIPT: &str = "aq.granted-never-started/";

/// The rule over `execution_attempts a` and `profiled_authority_hot h`, without the expiry.
macro_rules! never_started {
    () => {
        "a.kind='profiled_v1' AND a.holds_task AND h.state->>'status' IN ('granted','cancelling') \
         AND jsonb_typeof(h.state->'grant')='object' AND h.state#>>'{stop,kind}'='unknown' \
         AND NOT EXISTS(SELECT 1 FROM profiled_started_receipts x WHERE x.attempt_id=a.attempt_id) \
         AND NOT EXISTS(SELECT 1 FROM profiled_stopped_receipts x WHERE x.attempt_id=a.attempt_id) \
         AND NOT EXISTS(SELECT 1 FROM profiled_result_reports x WHERE x.attempt_id=a.attempt_id) \
         AND NOT EXISTS(SELECT 1 FROM source_transfers x WHERE x.attempt_id=a.attempt_id) \
         AND NOT EXISTS(SELECT 1 FROM profiled_launch_closures x WHERE x.attempt_id=a.attempt_id)"
    };
}
/// A lapsed attempt: [`never_started`] and its grant expired by `$2` (server milliseconds).
macro_rules! lapsed {
    () => {
        concat!(
            never_started!(),
            " AND (h.state#>>'{grant,expires_at_ms}')::BIGINT<=$2"
        )
    };
}

/// A held, granted, never-started attempt's launch binding and the instant its grant expires
/// (None for any other attempt). Unlocked: the settlement owner re-checks under its locks.
pub async fn pending(
    conn: &mut PgConnection,
    attempt: Uuid,
) -> Result<Option<(serde_json::Value, i64)>, Error> {
    let row = sqlx::query(concat!(
        "SELECT l.launch_binding,(h.state#>>'{grant,expires_at_ms}')::BIGINT AS expires \
         FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         JOIN profiled_launches l USING(attempt_id) WHERE a.attempt_id=$1 AND ",
        never_started!()
    ))
    .bind(attempt)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|r| (r.get("launch_binding"), r.get("expires"))))
}

/// Held attempts the closure settlement owner should visit, after `after` (a bounded cursor):
/// launches their worker closed before Started, and lapsed attempts (settled or not).
pub async fn awaiting(
    pool: &PgPool,
    after: Option<Uuid>,
    now_ms: i64,
    limit: i64,
) -> Result<Vec<Uuid>, Error> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT a.attempt_id FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE ($1::UUID IS NULL OR a.attempt_id>$1) AND a.kind='profiled_v1' AND a.holds_task AND (\
         (h.state#>>'{stop,kind}' IN ('unknown','not_started') \
          AND EXISTS(SELECT 1 FROM profiled_launch_closures c WHERE c.attempt_id=a.attempt_id)) \
         OR h.state#>>'{stop,receipt,id}'='",
        "aq.granted-never-started/",
        "'||a.attempt_id::TEXT OR (",
        lapsed!(),
        ")) ORDER BY a.attempt_id LIMIT $3"
    ))
    .bind(after)
    .bind(now_ms)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// The lapsed attempts of one node (a bounded page), for the points a lapsed attempt blocks.
pub async fn on_node(pool: &PgPool, node: Uuid, now_ms: i64) -> Result<Vec<Uuid>, Error> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT a.attempt_id FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE a.node_id=$1 AND ",
        lapsed!(),
        " ORDER BY a.attempt_id LIMIT 64"
    ))
    .bind(node)
    .bind(now_ms)
    .fetch_all(pool)
    .await?)
}

/// Releases the task claim of an attempt the settlement owner closed as lapsed, under the
/// common node -> attempt/hot locks. The release record is the B4 replacement release, so a
/// later offer of the task is assessed by the normal replacement rules (a cancelled or
/// completed task is simply never offered again). Idempotent; false when nothing was held.
pub async fn release(pool: &PgPool, attempt: Uuid, now_ms: i64) -> Result<bool, Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT n.id FROM nodes n JOIN execution_attempts a ON a.node_id=n.id WHERE a.attempt_id=$1 FOR UPDATE OF n")
        .bind(attempt)
        .fetch_optional(&mut *tx)
        .await?;
    let held = sqlx::query(concat!(
        "SELECT 1 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE a.attempt_id=$1 AND a.holds_task AND h.state->>'status'='finished' \
         AND h.state#>>'{stop,kind}'='not_started' AND h.state#>>'{stop,receipt,id}'='",
        "aq.granted-never-started/",
        "'||a.attempt_id::TEXT FOR UPDATE OF a,h"
    ))
    .bind(attempt)
    .fetch_optional(&mut *tx)
    .await?;
    if held.is_none() {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO profiled_replacement_releases(attempt_id,task_id,reason,stop_receipt_digest,\
         retained_source_receipt_digest,effect_reconciliation_receipt_digest,attempts_started,recorded_at_ms) \
         SELECT a.attempt_id,a.task_id,'failed_attempt_replacement',h.state#>>'{stop,receipt,digest}',\
         h.state#>>'{retention,receipt,digest}',h.state#>>'{effects,receipt,digest}',\
         (SELECT count(*) FROM execution_attempts x WHERE x.task_id=a.task_id \
          AND NOT (NOT x.holds_task AND profiled_never_granted_valid(x.attempt_id))),$2 \
         FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1 \
         ON CONFLICT(attempt_id) DO NOTHING",
    )
    .bind(attempt)
    .bind(now_ms)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1 AND holds_task",
    )
    .bind(attempt)
    .execute(&mut *tx)
    .await?;
    // Launch-scoped material is closed with the launch, as a worker's launch closure does.
    sqlx::query("UPDATE admission_material_leases SET state='revoked' WHERE prospective_attempt_id=$1 AND state IN ('pending','admitted')")
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}
