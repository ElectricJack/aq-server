//! Actual SQL/nominal refresh owners; synthetic installation, no physical stop claim.
mod common;
#[path = "common/phase_refresh_terminal.rs"]
mod terminal;
use aq_server::profiled_execution::no_grant;
use sqlx::Row;
use std::sync::Arc;
fn expire(f: &mut terminal::Prepared) {
    let at = chrono::DateTime::from_timestamp_millis(f.a.lease.expires_at_ms as i64).unwrap();
    f.c.env.env.state.clock = Arc::new(move || at);
}
#[tokio::test]
async fn actual_refresh_accepted_without_grant_expires_and_replays_fixed_absence_proof() {
    let mut f = Box::pin(terminal::prepared_terminal()).await;
    let attempt = f.prepared.original.binding.attempt_id;
    let node = f.q.original.identity.node_id;
    assert!(
        no_grant::close_expired(&f.c.env.env.state, node, attempt)
            .await
            .unwrap()
            .is_none()
    );
    expire(&mut f);
    let first = no_grant::close_expired(&f.c.env.env.state, node, attempt)
        .await
        .unwrap()
        .unwrap();
    assert!(first.changed);
    assert_eq!(first.proof.reason, "preparation_expired");
    assert_eq!(first.proof.applicable_deadline_ms, f.a.lease.expires_at_ms);
    assert_eq!(first.proof.binding, f.prepared.original.binding);
    let replay = no_grant::close_expired(&f.c.env.env.state, node, attempt)
        .await
        .unwrap()
        .unwrap();
    assert!(!replay.changed);
    assert_eq!(first.bytes, replay.bytes);
    let row=sqlx::query("SELECT a.holds_task,h.state->>'status' AS status,profiled_never_granted_valid(a.attempt_id) AS valid,(SELECT count(*) FROM profiled_phase_execution_grants WHERE attempt_id=a.attempt_id) AS grants FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1").bind(attempt).fetch_one(&f.c.env.env.pool).await.unwrap();
    assert!(!row.get::<bool, _>("holds_task"));
    assert_eq!(row.get::<String, _>("status"), "never_granted");
    assert!(row.get::<bool, _>("valid"));
    assert_eq!(row.get::<i64, _>("grants"), 0);
    let counts = no_grant::attempt_counts(&f.c.env.env.pool, f.c.env.lineage.task_id)
        .await
        .unwrap();
    assert_eq!(counts.started, 0);
    assert_eq!(counts.never_granted, 1);
}
#[tokio::test]
async fn actual_refresh_issued_phase_grant_never_uses_absence_release() {
    let mut f = Box::pin(terminal::prepared_terminal()).await;
    let issued = terminal::issue(&f).await;
    expire(&mut f);
    let attempt = issued.grant.binding.attempt_id;
    assert!(
        no_grant::close_expired(&f.c.env.env.state, f.q.original.identity.node_id, attempt)
            .await
            .is_err()
    );
    let row=sqlx::query("SELECT holds_task,(SELECT count(*) FROM profiled_never_granted_closures WHERE attempt_id=a.attempt_id) AS closures FROM execution_attempts a WHERE attempt_id=$1").bind(attempt).fetch_one(&f.c.env.env.pool).await.unwrap();
    assert!(row.get::<bool, _>("holds_task"));
    assert_eq!(row.get::<i64, _>("closures"), 0);
}
