//! Spec 0017 §5.4: the facts the offer path reads to place an attempt in an
//! environment. A failed read is no fact: the caller then chooses afresh, or
//! does not claim that no node has the environment.
use sqlx::PgPool;
use uuid::Uuid;

/// The environment `attempt` ran in, from its selection decision. A retry or
/// continuation of it keeps that environment (its workspace and build cache).
pub async fn ran_in(pool: &PgPool, attempt: Uuid) -> Option<String> {
    sqlx::query_scalar(
        "SELECT d.decision#>>'{project,environment}' FROM profiled_launches l \
         JOIN intelligence_decisions d USING(decision_id) WHERE l.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .flatten()
}
/// Whether some node's unexpired inventory lists `environment` (`name` or
/// `name@version`) for one of its harnesses. The environment's definition
/// pins its template digest, so a node listing it can run that template.
pub async fn listed(pool: &PgPool, environment: &str, now_ms: i64) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM intelligence_node_snapshots s, \
         jsonb_array_elements(s.snapshot->'harnesses') h \
         WHERE (s.snapshot->>'expires_at_ms')::bigint > $2 \
         AND $1 IN (h#>>'{environment,id}', (h#>>'{environment,id}')||'@'||(h#>>'{environment,version}')))",
    )
    .bind(environment)
    .bind(now_ms)
    .fetch_one(pool)
    .await
    .unwrap_or(true)
}
