//! Actual SQL history and current node auth, with synthetic Graph/Git/runtime.
mod common;
#[allow(dead_code)]
#[path = "common/phase_execution.rs"]
mod fixture;
use aq_admission::{Digest, SessionIdentity};
use aq_phase_terminal_protocol::{HistoryRequest, Negotiation};
use aq_server::{
    error::AppError,
    profiled_execution::{phase_execution, phase_terminal::history},
};
use fixture::{Prepared, auth, prepared};
use serde_json::Value;
use uuid::Uuid;

fn request(f: &Prepared) -> HistoryRequest {
    HistoryRequest {
        schema_version: 1,
        negotiation: Negotiation::current().unwrap(),
        operation_id: Uuid::new_v4(),
        binding: f.prepared.original.binding.clone(),
        accepted_receipt_digest: f.a.digest().unwrap(),
        prepare_operation_id: f.prepared.original.operation_id,
    }
}
async fn snapshot(f: &Prepared) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('hot',(SELECT to_jsonb(h) FROM profiled_authority_hot h LIMIT 1),'attempt',(SELECT to_jsonb(a) FROM execution_attempts a LIMIT 1),'journal',(SELECT to_jsonb(j) FROM profiled_owner_journals j LIMIT 1),'grant',(SELECT to_jsonb(g) FROM profiled_phase_execution_grants g LIMIT 1),'started',(SELECT count(*) FROM profiled_started_receipts),'stopped',(SELECT count(*) FROM profiled_stopped_receipts),'closures',(SELECT count(*) FROM profiled_launch_closures))")
        .fetch_one(&f.c.env.env.pool).await.unwrap()
}
async fn issue(f: &Prepared) -> aq_admission::preparation_v2::ExecutionReceipt {
    phase_execution::prepare(
        &f.c.env.env.state,
        &auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await
    .unwrap()
    .receipt
}

#[tokio::test]
async fn lost_first_reply_lookup_advances_from_absence_to_history_without_effects() {
    let mut f = prepared(true).await;
    let q = request(&f);
    let node = auth(&f.c).await;
    let before = snapshot(&f).await;
    let absent = history::read(&f.c.env.env.state, &node, &q).await.unwrap();
    absent.validate_for(&q).unwrap();
    assert!(absent.identity.is_none());
    assert_eq!(snapshot(&f).await, before);
    let execution = issue(&f).await;
    let before = snapshot(&f).await;
    let known = history::read(&f.c.env.env.state, &node, &q).await.unwrap();
    known.validate_for(&q).unwrap();
    let identity = known.identity.as_ref().unwrap();
    assert_eq!(identity.grant_id, execution.grant.grant_id);
    assert_eq!(
        identity.origin.execution_receipt_digest,
        execution.digest().unwrap()
    );
    assert_eq!(
        identity.prepared.revision,
        f.prepared.original.journal_revision
    );
    assert_eq!(identity.prepared.digest, f.prepared.original.journal_digest);
    assert_eq!(snapshot(&f).await, before);
    // Expiry and eligibility retirement do not erase immutable history. The
    // result still contains no ExecutionDelivery or local invocation token.
    let future = execution.execution_deadline_ms + 1;
    f.c.env.env.state.clock = std::sync::Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(future as i64).unwrap()
    });
    let mut config = (*f.c.env.env.state.config).clone();
    config.profiled_execution = None;
    f.c.env.env.state.config = std::sync::Arc::new(config);
    let historical = history::read(&f.c.env.env.state, &node, &q).await.unwrap();
    historical.validate_for(&q).unwrap();
    assert_eq!(historical.observed_at_ms, future);
    assert_eq!(
        serde_json::to_value(&historical.identity).unwrap(),
        serde_json::to_value(&known.identity).unwrap()
    );
    let encoded = serde_json::to_value(historical).unwrap();
    assert!(encoded.get("delivery").is_none() && encoded.get("clock").is_none());
    assert_eq!(snapshot(&f).await, before);
    let before_issuance = execution.issued_at_ms - 1;
    f.c.env.env.state.clock = std::sync::Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(before_issuance as i64).unwrap()
    });
    assert!(history::read(&f.c.env.env.state, &node, &q).await.is_err());
}

#[tokio::test]
async fn wrong_origin_operation_binding_and_current_credentials_are_refused() {
    let f = prepared(true).await;
    issue(&f).await;
    let node = auth(&f.c).await;
    let q = request(&f);
    let before = snapshot(&f).await;
    let mut wrong = q.clone();
    wrong.accepted_receipt_digest = Digest::of(b"wrong accepted origin");
    assert!(
        history::read(&f.c.env.env.state, &node, &wrong)
            .await
            .is_err()
    );
    wrong = q.clone();
    wrong.prepare_operation_id = Uuid::new_v4();
    assert!(
        history::read(&f.c.env.env.state, &node, &wrong)
            .await
            .is_err()
    );
    wrong = q.clone();
    wrong.binding.execution_spec_digest = Digest::of(b"wrong spec");
    assert!(
        history::read(&f.c.env.env.state, &node, &wrong)
            .await
            .is_err()
    );
    wrong = q.clone();
    if let SessionIdentity::Remote { node_id, .. } = &mut wrong.binding.session {
        *node_id = Uuid::new_v4();
    }
    assert!(matches!(
        history::read(&f.c.env.env.state, &node, &wrong).await,
        Err(AppError::Forbidden(_))
    ));
    assert_eq!(snapshot(&f).await, before);
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(node.id())
        .bind("0".repeat(64))
        .execute(&f.c.env.env.pool)
        .await
        .unwrap();
    assert!(matches!(
        history::read(&f.c.env.env.state, &node, &q).await,
        Err(AppError::CredentialRevoked)
    ));
    assert_eq!(snapshot(&f).await, before);
}

#[tokio::test]
async fn corrupt_retained_receipt_or_missing_owner_table_never_becomes_absence() {
    let f = prepared(true).await;
    issue(&f).await;
    let q = request(&f);
    let node = auth(&f.c).await;
    let mut tx = f.c.env.env.pool.begin().await.unwrap();
    // Deliberate corruption in a disposable test database, never a runtime path.
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_phase_execution_grants SET node_id=$1")
        .bind(Uuid::new_v4())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        history::read(&f.c.env.env.state, &node, &q).await.is_err(),
        "corrupt ownership cannot be hidden as missing history"
    );
    let mut tx = f.c.env.env.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_phase_execution_grants SET node_id=$1")
        .bind(node.id())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_phase_execution_grants SET receipt_bytes='{}'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(history::read(&f.c.env.env.state, &node, &q).await.is_err());
    sqlx::query("ALTER TABLE profiled_phase_execution_grants RENAME TO hidden_phase_grants")
        .execute(&f.c.env.env.pool)
        .await
        .unwrap();
    assert!(history::read(&f.c.env.env.state, &node, &q).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_history_has_finite_admission_and_does_not_take_renewal_connections() {
    let f = prepared(true).await;
    let q = request(&f);
    let node = auth(&f.c).await;
    let before = snapshot(&f).await;
    let mut blocker = f.c.env.env.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node.id())
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let mut readers = Vec::new();
    for _ in 0..2 {
        let state = f.c.env.env.state.clone();
        let q = q.clone();
        let node = node.clone();
        readers.push(tokio::spawn(async move {
            history::read(&state, &node, &q).await
        }));
    }
    let mut blocked = false;
    for _ in 0..30 {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT id FROM nodes WHERE id=%'")
            .fetch_one(&f.c.env.env.pool).await.unwrap();
        if count == 2 {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        blocked,
        "both actual history requests must own admission while waiting on SQL"
    );
    assert!(matches!(
        history::read(&f.c.env.env.state, &node, &q).await,
        Err(AppError::Maintenance(_))
    ));
    let renewal = f.c.env.env.state.store.renewal_pool_for_testing();
    let connection = renewal.acquire().await.unwrap();
    drop(connection);
    blocker.rollback().await.unwrap();
    for reader in readers {
        assert!(reader.await.unwrap().unwrap().identity.is_none());
    }
    assert_eq!(snapshot(&f).await, before);
}
