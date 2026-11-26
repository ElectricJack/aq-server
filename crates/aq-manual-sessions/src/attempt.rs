//! The live terminal of a queued attempt: the same disposable byte stream as a
//! hand-started agent's, attached by the worker to the attempt's session-host
//! tmux. Opening or closing it never starts, stops or settles the attempt.
use super::*;
use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};

pub(super) fn human_routes() -> Router<Service> {
    Router::new()
        .route(
            "/api/attempt-terminals/{id}/terminal/connect",
            post(terminal::connect),
        )
        .route(
            "/api/attempt-terminals/{id}/terminal/live",
            get(stream::browser),
        )
        .layer(DefaultBodyLimit::max(4096))
}
pub(super) fn worker_routes() -> Router<Service> {
    Router::new().route(
        "/v1/worker/attempt-terminals/{id}/terminal-stream",
        get(stream::worker),
    )
}
/// The attempt's node, and whether the attempt still runs there.
pub(super) async fn subject(s: &Service, id: Uuid) -> Result<(Uuid, bool)> {
    let row: Option<(Uuid, bool)> = sqlx::query_as("SELECT a.node_id,a.holds_task AND NOT EXISTS(SELECT 1 FROM execution_attempt_compute c WHERE c.attempt_id=a.attempt_id AND c.released) AND n.revoked_at IS NULL FROM execution_attempts a JOIN nodes n ON n.id=a.node_id WHERE a.attempt_id=$1")
        .bind(id)
        .fetch_optional(&s.pool)
        .await?;
    row.ok_or_else(missing)
}
/// Records that a person holds the attempt's pane. Every viewer of this stream can
/// type, so the stall ladder reads it as human control (`HUMAN_CONTROL_SQL`). Best
/// effort: a missed write only shortens the hold by one renewal.
pub(super) async fn controlled(s: &Service, id: Uuid, actor: Uuid) {
    if s.subject != Subject::Attempt {
        return;
    }
    let _ = sqlx::query("INSERT INTO attempt_terminal_viewers(attempt_id,principal,seen_at_ms) VALUES($1,$2,$3) ON CONFLICT(attempt_id) DO UPDATE SET principal=excluded.principal,seen_at_ms=excluded.seen_at_ms")
        .bind(id)
        .bind(actor)
        .bind(now())
        .execute(&s.pool)
        .await;
}
pub(super) async fn released(s: &Service, id: Uuid) {
    if s.subject != Subject::Attempt {
        return;
    }
    let _ = sqlx::query("DELETE FROM attempt_terminal_viewers WHERE attempt_id=$1")
        .bind(id)
        .execute(&s.pool)
        .await;
}
