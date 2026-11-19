//! Observation source queries: the stored source an enrolment or a batch is
//! checked against, supersession, refusals, their release when the attempt ends,
//! the count a node's source limit is checked against, and the ledger facts
//! the session reads decide a state from. Sources and releases are
//! append-only; the events of a released source stay until `collect` removes
//! them.
use aq_stall_ladder::freshness::{ENDED_SQL, Facts};
use sqlx::{PgConnection, PgPool, Row};
use std::collections::HashMap;
use uuid::Uuid;

/// Releases every enrolled source whose attempt ended, on `node` or on every
/// node. From then on the source does not count against the node's source
/// limit, and its batches are accepted until `now_ms + grace_ms` (the final
/// drain). Idempotent: a released source keeps its first release. It takes
/// no lock ingest waits for: a release only ever appears, with a deadline in
/// the future, so a concurrent batch is accepted either way.
pub async fn release_ended(
    conn: &mut PgConnection,
    now_ms: i64,
    grace_ms: i64,
    node: Option<Uuid>,
) -> sqlx::Result<u64> {
    // shortcut: scans every source that has no release, so each ended source is
    // read once; index the unreleased ones if a ledger reaches millions of rows.
    let sql = format!(
        "INSERT INTO observation_source_releases(node_id,source_id,generation,attempt_id,released_at_ms,accept_until_ms) \
         SELECT s.node_id,s.source_id,s.generation,s.attempt_id,$1,$1+$2 FROM observation_sources s \
         JOIN execution_attempts a USING(attempt_id) LEFT JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE ($3::uuid IS NULL OR s.node_id=$3) AND {ENDED_SQL} \
         AND NOT EXISTS(SELECT 1 FROM observation_source_releases r WHERE r.node_id=s.node_id \
          AND r.source_id=s.source_id AND r.generation=s.generation) ON CONFLICT DO NOTHING"
    );
    let done = sqlx::query(&sql)
        .bind(now_ms)
        .bind(grace_ms)
        .bind(node)
        .execute(conn)
        .await?;
    Ok(done.rows_affected())
}

/// Enrollment's count, under the node's ledger lock: releases the node's ended
/// sources ([`release_ended`]), then counts its sources in use, which are
/// neither released nor superseded by a newer generation.
pub async fn in_use_after_release(
    conn: &mut PgConnection,
    node: Uuid,
    now_ms: i64,
    grace_ms: i64,
) -> sqlx::Result<i64> {
    release_ended(&mut *conn, now_ms, grace_ms, Some(node)).await?;
    sqlx::query_scalar(
        "SELECT count(*) FROM observation_sources s WHERE s.node_id=$1 \
         AND NOT EXISTS(SELECT 1 FROM observation_source_releases r WHERE r.node_id=s.node_id \
          AND r.source_id=s.source_id AND r.generation=s.generation) \
         AND NOT EXISTS(SELECT 1 FROM observation_source_revocations r WHERE r.node_id=s.node_id \
          AND r.source_id=s.source_id AND r.generation=s.generation)",
    )
    .bind(node)
    .fetch_one(conn)
    .await
}

/// The attempt's newest unrevoked source, across nodes: (generation, digest).
pub async fn current(pool: &PgPool, attempt: Uuid) -> sqlx::Result<Option<(i64, String)>> {
    sqlx::query_as(
        "SELECT s.generation,s.authority_digest FROM observation_sources s \
         WHERE s.attempt_id=$1 AND NOT EXISTS(SELECT 1 FROM observation_source_revocations r \
          WHERE r.node_id=s.node_id AND r.source_id=s.source_id AND r.generation=s.generation) \
         ORDER BY s.generation DESC LIMIT 1",
    )
    .bind(attempt)
    .fetch_optional(pool)
    .await
}

/// The ledger facts of the attempts of `sessions`, by session, in one query
/// over primary keys: a session list reads no ledger row per item.
pub async fn facts(pool: &PgPool, sessions: &[Uuid]) -> sqlx::Result<HashMap<Uuid, Facts>> {
    let sql = format!(
        "SELECT a.session_id,{ENDED_SQL} AS ended,\
         EXISTS(SELECT 1 FROM observation_sources s WHERE s.attempt_id=a.attempt_id) AS enrolled,\
         COALESCE(p.last_observed_at_ms,0) AS observed,COALESCE(p.last_progress_at_ms,0) AS progress \
         FROM execution_attempts a LEFT JOIN profiled_authority_hot h USING(attempt_id) \
         LEFT JOIN observation_attempt_projections p USING(attempt_id) WHERE a.session_id=ANY($1)"
    );
    let rows = sqlx::query(&sql).bind(sessions).fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|row| {
            // 0 or no projection row: none yet.
            let ms = |column: &str| Some(row.get::<i64, _>(column) as u64).filter(|at| *at > 0);
            let facts = Facts {
                ended: row.get("ended"),
                enrolled: row.get("enrolled"),
                last_observation_ms: ms("observed"),
                last_progress_ms: ms("progress"),
            };
            (row.get("session_id"), facts)
        })
        .collect())
}

/// The stored authority digest of a source and whether it was revoked.
pub async fn stored(
    conn: &mut PgConnection,
    node: Uuid,
    source: Uuid,
    generation: i64,
) -> sqlx::Result<Option<(String, bool)>> {
    sqlx::query_as(
        "SELECT s.authority_digest,r.node_id IS NOT NULL FROM observation_sources s \
         LEFT JOIN observation_source_revocations r USING(node_id,source_id,generation) \
         WHERE s.node_id=$1 AND s.source_id=$2 AND s.generation=$3",
    )
    .bind(node)
    .bind(source)
    .bind(generation)
    .fetch_optional(conn)
    .await
}

/// Every unrevoked source of `attempt` on any node, locked: a new generation supersedes them.
pub async fn unrevoked(
    conn: &mut PgConnection,
    attempt: Uuid,
) -> sqlx::Result<Vec<(Uuid, Uuid, i64)>> {
    sqlx::query_as(
        "SELECT s.node_id,s.source_id,s.generation FROM observation_sources s \
         WHERE s.attempt_id=$1 AND NOT EXISTS(SELECT 1 FROM observation_source_revocations r \
          WHERE r.node_id=s.node_id AND r.source_id=s.source_id AND r.generation=s.generation) \
         ORDER BY s.node_id,s.source_id,s.generation FOR UPDATE OF s",
    )
    .bind(attempt)
    .fetch_all(conn)
    .await
}

/// Revokes `old` (node, source, generation) of `attempt`, superseded by `by`.
pub async fn revoke(
    conn: &mut PgConnection,
    old: (Uuid, Uuid, i64),
    attempt: Uuid,
    by: (Uuid, Uuid, i64),
    at_ms: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO observation_source_revocations(node_id,source_id,generation,attempt_id,\
         superseded_by_node_id,superseded_by_source_id,superseded_by_generation,revoked_at_ms) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(old.0)
    .bind(old.1)
    .bind(old.2)
    .bind(attempt)
    .bind(by.0)
    .bind(by.1)
    .bind(by.2)
    .bind(at_ms)
    .execute(conn)
    .await
    .map(|_| ())
}

/// The source a batch event names, as ingest checks it: `None` when it is unknown or was
/// released with its final drain over before `now_ms`. The row carries
/// `authority`, `authority_digest`, `revoked` and `refused` (a durable source refusal).
pub async fn for_ingest(
    conn: &mut PgConnection,
    node: Uuid,
    source: Uuid,
    generation: i64,
    now_ms: i64,
) -> sqlx::Result<Option<sqlx::postgres::PgRow>> {
    sqlx::query(
        "SELECT s.authority,s.authority_digest,r.node_id IS NOT NULL AS revoked,\
         EXISTS(SELECT 1 FROM observation_source_refusals x WHERE x.node_id=s.node_id \
         AND x.source_id=s.source_id AND x.generation=s.generation) AS refused \
         FROM observation_sources s \
         LEFT JOIN observation_source_revocations r USING(node_id,source_id,generation) \
         WHERE s.node_id=$1 AND s.source_id=$2 AND s.generation=$3 AND NOT EXISTS(\
         SELECT 1 FROM observation_source_releases x WHERE x.node_id=s.node_id AND x.source_id=s.source_id \
         AND x.generation=s.generation AND x.accept_until_ms<$4)",
    )
    .bind(node).bind(source).bind(generation).bind(now_ms).fetch_optional(conn).await
}
