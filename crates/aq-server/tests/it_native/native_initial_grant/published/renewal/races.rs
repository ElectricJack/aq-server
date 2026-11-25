//! Adversarial owner-ordering checks on actual native publication and grants.
use super::*;
fn physical_started(
    c: &Prepared,
    initial: &InitialGrantDelivery,
) -> aq_admission::started::Request {
    let mut physical: aq_admission::started::Request = serde_json::from_str(include_str!(
        "../../../../../../aq-admission/artifacts/started.fixture.json"
    ))
    .unwrap();
    physical.operation_id = Uuid::new_v4();
    physical.binding = c.request.binding.clone();
    physical.grant_id = initial.original.grant.grant_id;
    physical.guardian_id = c.request.guardian_id;
    physical.launch_id = c.request.launch_id;
    physical.reservation_id = c.request.reservation.reservation_id;
    let runtime = &c
        .case
        .env
        .env
        .state
        .config
        .profiled_execution
        .as_ref()
        .unwrap()
        .qualified_runtimes[0];
    physical.runtime = runtime.runtime.clone();
    physical.qualification = runtime.qualification.clone();
    physical.journal_revision = c.request.journal_revision + 1;
    physical.journal_digest = Digest::of(b"synthetic original physical Started checkpoint");
    physical.observation.guardian.intent.launch_id = physical.launch_id;
    physical.observation.guardian.intent.session = physical.binding.session.clone();
    physical.observation.guardian.intent.manifest_digest =
        physical.binding.launch_manifest_digest.clone();
    physical.observation_digest = physical.observation.digest().unwrap();
    physical.validate().unwrap();
    physical
}
async fn journal_head(c: &Prepared, attempt_id: Uuid) -> (i64, String) {
    sqlx::query_as("SELECT revision,digest FROM profiled_owner_journals WHERE attempt_id=$1")
        .bind(attempt_id)
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap()
}
async fn renewal_receipt_count(c: &Prepared) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM profiled_execution_receipts WHERE kind='renew'")
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap()
}
async fn wait_for_quota_lock(pool: &sqlx::PgPool) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity \
                 WHERE datname=current_database() AND wait_event_type='Lock' \
                 AND query LIKE '%aq-native-renewal-history-v1%')",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("observe actual final quota-lock wait");
}

#[tokio::test]
async fn receipt_insert_failure_rolls_back_hot_extension_and_exact_retry_can_succeed() {
    let c = setup().await;
    let initial = grant(&c).await.unwrap();
    let r = request(&c, &initial);
    sqlx::raw_sql("CREATE FUNCTION fixture_refuse_renew() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='renew' THEN RAISE EXCEPTION 'fixture receipt failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fixture_refuse_renew BEFORE INSERT ON profiled_execution_receipts FOR EACH ROW EXECUTE FUNCTION fixture_refuse_renew();").execute(&c.case.env.env.pool).await.unwrap();
    let (status, _) = send(&c, &r).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(r.binding.attempt_id)
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(
        hot["grant"]["expires_at_ms"],
        initial.original.clock.expires_at_ms
    );
    assert_eq!(renewal_receipt_count(&c).await, 0);
    sqlx::raw_sql("DROP TRIGGER fixture_refuse_renew ON profiled_execution_receipts; DROP FUNCTION fixture_refuse_renew();").execute(&c.case.env.env.pool).await.unwrap();
    let (status, reply) = send(&c, &r).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
}
#[tokio::test]
async fn changed_task_cas_and_content_refuse_without_reusing_admission_observation() {
    let c = setup().await;
    let initial = grant(&c).await.unwrap();
    for mode in [1, 2] {
        c.case.mode.store(mode, Ordering::SeqCst);
        let (status, reply) = send(&c, &request(&c, &initial)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    }
    assert_eq!(renewal_receipt_count(&c).await, 0);
}
#[tokio::test]
async fn delayed_actual_started_keeps_physical_checkpoint_separate_from_latest_renew_counter() {
    let c = setup().await;
    let initial = grant(&c).await.unwrap();
    let physical = physical_started(&c, &initial);
    let mut r = request(&c, &initial);
    r.journal_revision = physical.journal_revision + 1;
    let (status, reply) = send(&c, &r).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    let ControlReply::Renewed(first) = reply else {
        panic!("expected renewal")
    };
    let prepared = journal_head(&c, r.binding.attempt_id).await;
    assert_eq!(
        prepared,
        (
            c.request.journal_revision as i64,
            c.request.journal_digest.as_str().into()
        )
    );
    aq_server::profiled_execution::started::record(
        &c.case.env.env.state,
        &auth(&c).await,
        &physical,
    )
    .await
    .unwrap();
    let head = journal_head(&c, r.binding.attempt_id).await;
    assert_eq!(
        head,
        (
            physical.journal_revision as i64,
            physical.journal_digest.as_str().into()
        )
    );
    let (status, replay) = send(&c, &r).await;
    assert_eq!(status, StatusCode::OK, "{replay:?}");
    let ControlReply::Renewed(replayed) = replay else {
        panic!("expected unchanged permit")
    };
    assert_eq!(first.versions, replayed.versions);
    assert_eq!(
        first.delivery.clock.expires_at_ms,
        replayed.delivery.clock.expires_at_ms
    );
    let mut stale = r.clone();
    stale.operation_id = Uuid::new_v4();
    let (status, reply) = send(&c, &stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    c.clock.fetch_add(1000, Ordering::SeqCst);
    let mut next = stale;
    next.journal_revision += 1;
    let (status, reply) = send(&c, &next).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    let still_physical = journal_head(&c, r.binding.attempt_id).await;
    assert_eq!(head, still_physical);
    let latest:i64=sqlx::query_scalar("SELECT max((response#>>'{body,journal_revision}')::bigint) FROM profiled_execution_receipts WHERE kind='renew'").fetch_one(&c.case.env.env.pool).await.unwrap();
    assert_eq!(latest, next.journal_revision as i64);
}
#[tokio::test]
async fn observed_final_quota_wait_crossing_old_expiry_rolls_back_extension_and_receipt() {
    let c = Arc::new(setup().await);
    let initial = grant(&c).await.unwrap();
    let r = request(&c, &initial);
    let mut blocker = c.case.env.env.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('aq-native-renewal-history-v1',0))")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let owner = c.clone();
    let pending = tokio::spawn(async move { send(&owner, &r).await });
    wait_for_quota_lock(&c.case.env.env.pool).await;
    c.clock.store(
        initial.original.clock.expires_at_ms as i64,
        Ordering::SeqCst,
    );
    blocker.commit().await.unwrap();
    let (status, reply) = pending.await.unwrap();
    assert!(
        matches!(
            status,
            StatusCode::CONFLICT | StatusCode::SERVICE_UNAVAILABLE
        ),
        "{status} {reply:?}"
    );
    let expiry:i64=sqlx::query_scalar("SELECT (state#>>'{grant,expires_at_ms}')::bigint FROM profiled_authority_hot WHERE attempt_id=$1").bind(c.request.binding.attempt_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    assert_eq!(expiry as u64, initial.original.clock.expires_at_ms);
    assert_eq!(renewal_receipt_count(&c).await, 0);
}
