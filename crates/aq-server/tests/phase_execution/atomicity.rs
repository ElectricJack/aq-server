//! Actual owning transaction boundaries, with an explicit SQL fault injector.
use super::*;
async fn rows(c: &Case) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('hot',(SELECT to_jsonb(h) FROM profiled_authority_hot h),'journal',(SELECT to_jsonb(j) FROM profiled_owner_journals j),'accept',(SELECT to_jsonb(a) FROM profiled_v2_accepts a),'grants',(SELECT count(*) FROM profiled_phase_execution_grants),'materials',(SELECT count(*) FROM admission_material_receipts))").fetch_one(&c.env.env.pool).await.unwrap()
}
#[tokio::test]
async fn final_receipt_insert_failure_rolls_back_hot_journal_and_installation() {
    let f = prepared(true).await;
    let before = rows(&f.c).await;
    sqlx::raw_sql("CREATE SEQUENCE fixture_phase_insert_hit; CREATE FUNCTION fixture_phase_insert_fail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('fixture_phase_insert_hit'); RAISE EXCEPTION 'explicit phase final insert fault'; END $$; CREATE TRIGGER zz_fixture_phase_insert_fail AFTER INSERT ON profiled_phase_execution_grants FOR EACH ROW EXECUTE FUNCTION fixture_phase_insert_fail();").execute(&f.c.env.env.pool).await.unwrap();
    let error = phase_execution::prepare(
        &f.c.env.env.state,
        &auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, aq_server::error::AppError::Internal),
        "{error:?}"
    );
    let hit: bool = sqlx::query_scalar("SELECT is_called FROM fixture_phase_insert_hit")
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    assert!(
        hit,
        "failure must reach the final insert after actual authority work"
    );
    assert_eq!(before, rows(&f.c).await);
    assert_unissued(&f.c).await;
    sqlx::query("DROP TRIGGER zz_fixture_phase_insert_fail ON profiled_phase_execution_grants")
        .execute(&f.c.env.env.pool)
        .await
        .unwrap();
    phase_execution::prepare(
        &f.c.env.env.state,
        &auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn original_proposal_expiry_does_not_limit_new_execution_lease() {
    // The live offer window is the minimum of its input facts. A short-lived
    // node-scoped observation bounds it, while the operator observation keeps
    // current health, so only the original proposal expiry passes.
    let f = prepared_with_short_observation(true, 2000).await;
    let now = (f.c.env.env.state.clock)().timestamp_millis() as u64;
    let offer_expiry = f.p.original.offered.clock.expires_at_ms;
    assert!(
        offer_expiry > now,
        "original proposal is still live before the advance"
    );
    let wait = offer_expiry - now + 25;
    assert!(
        wait < 2100,
        "short observation bounds the original proposal"
    );
    assert!(
        now + wait < f.a.lease.expires_at_ms,
        "original proposal must expire inside the live preparation lease"
    );
    // The injected owner clock advances; nothing sleeps.
    f.c.env
        .env
        .time
        .fetch_add(wait as i64, std::sync::atomic::Ordering::SeqCst);
    let grant = phase_execution::prepare(
        &f.c.env.env.state,
        &auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    assert!(grant.receipt.issued_at_ms > f.p.original.offered.clock.expires_at_ms);
    assert!(grant.receipt.grant.expires_at_ms > f.a.lease.expires_at_ms);
    assert_eq!(
        grant.receipt.grant.expires_at_ms - grant.receipt.issued_at_ms,
        f.p.policy.maximum_execution_lease_ms
    );
}
#[tokio::test]
async fn issued_history_is_immutable_and_changed_request_cannot_replay() {
    let f = prepared(true).await;
    let auth = auth(&f.c).await;
    let first = phase_execution::prepare(&f.c.env.env.state, &auth, &f.prepared, Uuid::new_v4())
        .await
        .unwrap();
    let before = rows(&f.c).await;
    for sql in [
        "UPDATE profiled_phase_execution_grants SET expires_at_ms=expires_at_ms+1",
        "DELETE FROM profiled_phase_execution_grants",
    ] {
        assert!(sqlx::query(sql).execute(&f.c.env.env.pool).await.is_err());
    }
    let mut changed = f.prepared.clone();
    changed.original.journal_digest = Digest::of(b"different durable request");
    assert!(
        phase_execution::prepare(&f.c.env.env.state, &auth, &changed, Uuid::new_v4())
            .await
            .is_err()
    );
    changed = f.prepared.clone();
    changed.original.operation_id = Uuid::new_v4();
    assert!(
        phase_execution::prepare(&f.c.env.env.state, &auth, &changed, Uuid::new_v4())
            .await
            .is_err()
    );
    assert_eq!(before, rows(&f.c).await);
    let replay = phase_execution::prepare(&f.c.env.env.state, &auth, &f.prepared, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        first.receipt.digest().unwrap(),
        replay.receipt.digest().unwrap()
    );
}

#[tokio::test]
async fn final_insert_wait_expiring_facts_rolls_back_despite_live_preparation_and_execution_windows()
 {
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };
    let mut f = prepared(true).await;
    let at = (f.c.env.env.state.clock)().timestamp_millis();
    let clock = Arc::new(AtomicI64::new(at));
    let sample = clock.clone();
    f.c.env.env.state.clock = Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(sample.load(Ordering::SeqCst)).unwrap()
    });
    let before = rows(&f.c).await;
    sqlx::raw_sql("CREATE FUNCTION fixture_phase_insert_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(hashtextextended('fixture-phase-final-insert',0)); RETURN NEW; END $$; CREATE TRIGGER zz_fixture_phase_insert_wait AFTER INSERT ON profiled_phase_execution_grants FOR EACH ROW EXECUTE FUNCTION fixture_phase_insert_wait();").execute(&f.c.env.env.pool).await.unwrap();
    let mut blocker = f.c.env.env.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('fixture-phase-final-insert',0))")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let state = f.c.env.env.state.clone();
    let node = auth(&f.c).await;
    let request = f.prepared.clone();
    let task = tokio::spawn(async move {
        phase_execution::prepare(&state, &node, &request, Uuid::new_v4()).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND wait_event='advisory' AND query LIKE '%INSERT INTO profiled_phase_execution_grants%')").fetch_one(&f.c.env.env.pool).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.expect("must observe final actual insert wait");
    let expired_facts = at + f.p.policy.maximum_observation_ms as i64 + 500;
    assert!(
        (expired_facts as u64) < f.a.lease.expires_at_ms,
        "preparation stays live while facts expire"
    );
    clock.store(expired_facts, Ordering::SeqCst);
    blocker.commit().await.unwrap();
    assert!(task.await.unwrap().is_err());
    assert_eq!(before, rows(&f.c).await);
    assert_unissued(&f.c).await;
}
