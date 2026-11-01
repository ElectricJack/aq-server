use super::*;
async fn waited(c: &Case, at: &Arc<AtomicI64>, new_time: u64, state: AppState) {
    let key = 0x0330f11_i64;
    let pool = &c.f.c.env.env.pool;
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *blocker)
        .await
        .unwrap();
    sqlx::raw_sql(&format!("CREATE FUNCTION fixture_phase_started_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({key}); RETURN NEW; END $$; CREATE TRIGGER zz_fixture_phase_started_wait AFTER INSERT ON profiled_started_receipts FOR EACH ROW EXECUTE FUNCTION fixture_phase_started_wait();")).execute(pool).await.unwrap();
    let auth = terminal::auth(&c.f).await;
    let q = c.request.clone();
    let task = tokio::spawn(async move { started::record(&state, &auth, &q).await });
    let until = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND NOT granted AND classid=0 AND objid::bigint=$1 AND database=(SELECT oid FROM pg_database WHERE datname=current_database()))").bind(key).fetch_one(pool).await.unwrap();
        if pending {
            break;
        }
        assert!(
            !task.is_finished(),
            "Started must reach final INSERT before the clock fault"
        );
        assert!(
            tokio::time::Instant::now() < until,
            "expected actual advisory wait"
        );
        tokio::task::yield_now().await;
    }
    at.store(new_time as i64, Ordering::SeqCst);
    blocker.commit().await.unwrap();
    let result = task.await.unwrap();
    assert!(
        matches!(result, Err(AppError::Conflict(_))),
        "must reach actual final time fence: {result:?}"
    );
}
#[tokio::test]
async fn final_insert_expiry_and_backward_clock_each_roll_back_actual_started() {
    for backwards in [false, true] {
        let c = Box::pin(setup()).await;
        let before = audit(&c).await;
        let (state, at) = clock(&c, c.execution.issued_at_ms + 1);
        let next = if backwards {
            c.execution.issued_at_ms
        } else {
            c.execution.grant.expires_at_ms
        };
        waited(&c, &at, next, state).await;
        assert_eq!(audit(&c).await, before);
    }
}
#[tokio::test]
async fn server_clock_regression_before_receipt_stamp_cannot_precede_issuance() {
    let c = Box::pin(setup()).await;
    let before = audit(&c).await;
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = reads.clone();
    let issued = c.execution.issued_at_ms;
    let mut state = c.f.c.env.env.state.clone();
    state.clock = Arc::new(move || {
        let now = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            issued + 1
        } else {
            issued - 1
        };
        chrono::DateTime::from_timestamp_millis(now as i64).unwrap()
    });
    assert!(matches!(
        call(&c, &state, &c.request).await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(audit(&c).await, before);
}
