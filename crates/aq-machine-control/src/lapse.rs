//! A granted attempt that never started must not hold anything forever. An attempt that was
//! granted, has no Started, Stopped, result, source transfer or launch closure on record and
//! whose launch has lapsed is provably not running and can never start: its launch lapses
//! when its grant expires or, earlier, when its launch deadline (Settings › Selection
//! defaults `launch_deadline_ms`, measured from the grant) passes. The server refuses a
//! Started report once the launch has lapsed, and a renewal once the grant has expired, under
//! the same node -> attempt/hot locks its closing takes; the closing itself revokes the
//! attempt and finishes it, so no later report can start it either. The server closes such an
//! attempt itself as failed with the reason [`REASON`] (stop `not_started`, receipt
//! [`RECEIPT`]`<attempt>`), releases its task claim (which frees its node's compute and its
//! snapshot pin, and lets another node take the task) and does not charge it to the lineage
//! budgets. The SQL facts of that rule live here; the server's closure settlement owner
//! writes the typed state.
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
/// The instant the launch deadline `$n` (milliseconds, a parameter) of a granted attempt ends:
/// its grant instant plus the deadline. Both grant owners set the execution deadline to the
/// grant instant plus the offer's wall time, so the grant instant is their difference. NULL
/// (no launch deadline) for an attempt without both.
macro_rules! launch_ends {
    ($deadline:literal) => {
        concat!(
            "((h.state->>'execution_deadline_ms')::BIGINT\
             -(a.state#>>'{offer,manifest,resources,wall_time_ms}')::BIGINT+",
            $deadline,
            "::BIGINT)"
        )
    };
}
/// The instant a granted attempt can no longer start: its grant expiry or, when earlier, the
/// end of its launch deadline `$n`.
macro_rules! launch_lapses {
    ($deadline:literal) => {
        concat!(
            "LEAST((h.state#>>'{grant,expires_at_ms}')::BIGINT,",
            launch_ends!($deadline),
            ")"
        )
    };
}
/// A lapsed attempt: [`never_started`] and its launch lapsed by `$2` (server milliseconds),
/// with the launch deadline `$n`.
macro_rules! lapsed {
    ($deadline:literal) => {
        concat!(never_started!(), " AND ", launch_lapses!($deadline), "<=$2")
    };
}

/// A held, granted, never-started attempt's launch binding and the instant its launch lapses
/// under the launch deadline `launch_deadline_ms` (None for any other attempt). Unlocked: the
/// settlement owner re-checks under its locks.
pub async fn pending(
    conn: &mut PgConnection,
    attempt: Uuid,
    launch_deadline_ms: u64,
) -> Result<Option<(serde_json::Value, i64)>, Error> {
    let row = sqlx::query(concat!(
        "SELECT l.launch_binding,",
        launch_lapses!("$2"),
        " AS lapses FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         JOIN profiled_launches l USING(attempt_id) WHERE a.attempt_id=$1 AND ",
        never_started!()
    ))
    .bind(attempt)
    .bind(millis(launch_deadline_ms))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|r| (r.get("launch_binding"), r.get("lapses"))))
}

/// Whether the launch deadline `launch_deadline_ms` of `attempt` ended by `now_ms`. False for
/// an attempt that has no grant. Read on the caller's transaction, after its locks.
pub async fn launch_overdue(
    conn: &mut PgConnection,
    attempt: Uuid,
    now_ms: i64,
    launch_deadline_ms: u64,
) -> Result<bool, Error> {
    let overdue: Option<Option<bool>> = sqlx::query_scalar(concat!(
        "SELECT ",
        launch_ends!("$3"),
        "<=$2 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE a.attempt_id=$1 AND jsonb_typeof(h.state->'grant')='object'"
    ))
    .bind(attempt)
    .bind(now_ms)
    .bind(millis(launch_deadline_ms))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(overdue.flatten().unwrap_or(false))
}

/// A granted attempt whose agent has not started and whose launch has lapsed: shown as
/// "launch overdue on <node>" until the settlement owner ends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overdue {
    pub attempt_id: Uuid,
    pub task_id: Uuid,
    pub node_id: Uuid,
    /// The node's display name (its enrolled name, else its id).
    pub node_name: String,
    /// When the launch lapsed (grant expiry or end of the launch deadline).
    pub lapsed_at_ms: i64,
}
impl Overdue {
    /// `Launch overdue on <node>`.
    pub fn headline(&self) -> String {
        format!("Launch overdue on {}", self.node_name)
    }
}
const OVERDUE: &str =
    "SELECT a.attempt_id,a.task_id,a.node_id,COALESCE(NULLIF(n.name,''),n.id::TEXT) AS node_name,";
fn overdue_row(r: &sqlx::postgres::PgRow) -> Overdue {
    Overdue {
        attempt_id: r.get("attempt_id"),
        task_id: r.get("task_id"),
        node_id: r.get("node_id"),
        node_name: r.get("node_name"),
        lapsed_at_ms: r.get("lapses"),
    }
}

/// The overdue launch of `attempt`, if its launch lapsed by `now_ms` and it has not started.
pub async fn overdue_attempt(
    conn: &mut PgConnection,
    attempt: Uuid,
    now_ms: i64,
    launch_deadline_ms: u64,
) -> Result<Option<Overdue>, Error> {
    let row = sqlx::query(&format!(
        "{OVERDUE}{} AS lapses FROM execution_attempts a JOIN profiled_authority_hot h \
         USING(attempt_id) JOIN nodes n ON n.id=a.node_id WHERE a.attempt_id=$1 AND {}",
        launch_lapses!("$3"),
        lapsed!("$3")
    ))
    .bind(attempt)
    .bind(now_ms)
    .bind(millis(launch_deadline_ms))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(overdue_row))
}

/// The overdue launches on `nodes` (a bounded page, oldest lapse first).
pub async fn overdue_on(
    conn: &mut PgConnection,
    nodes: &[Uuid],
    now_ms: i64,
    launch_deadline_ms: u64,
) -> Result<Vec<Overdue>, Error> {
    let rows = sqlx::query(&format!(
        "{OVERDUE}{} AS lapses FROM execution_attempts a JOIN profiled_authority_hot h \
         USING(attempt_id) JOIN nodes n ON n.id=a.node_id WHERE a.node_id=ANY($1) AND {} \
         ORDER BY lapses,a.attempt_id LIMIT 64",
        launch_lapses!("$3"),
        lapsed!("$3")
    ))
    .bind(nodes)
    .bind(now_ms)
    .bind(millis(launch_deadline_ms))
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(overdue_row).collect())
}

fn millis(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Held attempts the closure settlement owner should visit, after `after` (a bounded cursor):
/// launches their worker closed before Started, and lapsed attempts (settled or not) under the
/// launch deadline `launch_deadline_ms`.
pub async fn awaiting(
    pool: &PgPool,
    after: Option<Uuid>,
    now_ms: i64,
    limit: i64,
    launch_deadline_ms: u64,
) -> Result<Vec<Uuid>, Error> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT a.attempt_id FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE ($1::UUID IS NULL OR a.attempt_id>$1) AND a.kind='profiled_v1' AND a.holds_task AND (\
         (h.state#>>'{stop,kind}' IN ('unknown','not_started') \
          AND EXISTS(SELECT 1 FROM profiled_launch_closures c WHERE c.attempt_id=a.attempt_id)) \
         OR h.state#>>'{stop,receipt,id}'='",
        "aq.granted-never-started/",
        "'||a.attempt_id::TEXT OR (",
        lapsed!("$4"),
        ")) ORDER BY a.attempt_id LIMIT $3"
    ))
    .bind(after)
    .bind(now_ms)
    .bind(limit)
    .bind(millis(launch_deadline_ms))
    .fetch_all(pool)
    .await?)
}

/// The lapsed attempts of one node (a bounded page), for the points a lapsed attempt blocks.
pub async fn on_node(
    pool: &PgPool,
    node: Uuid,
    now_ms: i64,
    launch_deadline_ms: u64,
) -> Result<Vec<Uuid>, Error> {
    Ok(sqlx::query_scalar(concat!(
        "SELECT a.attempt_id FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE a.node_id=$1 AND ",
        lapsed!("$3"),
        " ORDER BY a.attempt_id LIMIT 64"
    ))
    .bind(node)
    .bind(now_ms)
    .bind(millis(launch_deadline_ms))
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
    sqlx::query("SELECT n.id FROM nodes n JOIN execution_attempts a ON a.node_id=n.id WHERE a.attempt_id=$1 FOR KEY SHARE OF n")
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
