use aq_admission::{
    Label, SessionIdentity,
    transport::{ProcessOutcome, ResultReport, WorkerIdentity},
};
use aq_server::profiled_execution::results;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;
/// Builds a large owner future in this helper's frame and moves it to the heap,
/// so unoptimized test poll frames keep a pointer instead of the whole future.
fn on_heap<F, Fut>(build: F) -> std::pin::Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    Box::pin(build())
}
async fn ready() -> (terminal::Prepared, ResultReport) {
    Box::pin(ready_with_stop(true)).await
}
async fn ready_with_stop(stopped: bool) -> (terminal::Prepared, ResultReport) {
    Box::pin(ready_with_stop_reason(stopped, None)).await
}
async fn ready_with_stop_reason(
    stopped: bool,
    reason: Option<aq_admission::transport::StopReason>,
) -> (terminal::Prepared, ResultReport) {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let issued = on_heap(|| terminal::issue(&f)).await;
    let auth = terminal::auth(&f).await;
    let start = terminal::started_request(&f, &issued);
    on_heap(|| terminal_owner::started::record(&f.c.env.env.state, &auth, &start))
        .await
        .unwrap();
    let mut stop = terminal::stop_request(&start);
    if let Some(reason) = reason {
        stop.original.evidence.reason = reason;
    }
    if stopped {
        on_heap(|| terminal_owner::stopped::record(&f.c.env.env.state, &auth, &stop))
            .await
            .unwrap();
    }
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = start.original.binding.session
    else {
        panic!()
    };
    let report = ResultReport {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity { node_id, boot_id },
        binding: start.original.binding,
        report_id: Uuid::new_v4(),
        supersedes: None,
        outcome: ProcessOutcome::Success,
        stop: stop.original.evidence,
        source_receipt_ids: vec![],
        pending_effect_ids: vec![],
        conclusion: Label::new("Synthetic B1 claim only; not accepted or settled").unwrap(),
    };
    (f, report)
}

#[cfg(feature = "delivery_playbook")]
#[tokio::test]
async fn native_outcome_blocked_is_bound_to_the_current_assignment() {
    use aq_admission::transport::StopReason;
    use aq_playbook_domain::delivery_store::{DeliveryPort, PgDelivery, observed};

    let (f, mut report) = Box::pin(ready_with_stop_reason(
        true,
        Some(StopReason::Reconciliation),
    ))
    .await;
    let auth = terminal::auth(&f).await;
    let pool = &f.c.env.env.pool;
    let attempt = report.binding.attempt_id;
    let (task, version): (Uuid, i64) = sqlx::query_as(
        "SELECT a.task_id,(l.execution_spec#>>'{task,version}')::bigint
         FROM execution_attempts a JOIN profiled_launches l USING(attempt_id) WHERE a.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO delivery_git_grants(attempt_id,repository_id,base,task_ref) VALUES($1,$2,$3,$4)")
        .bind(attempt)
        .bind(Uuid::new_v4())
        .bind("1".repeat(40))
        .bind(format!("refs/heads/aq/{task}"))
        .execute(pool)
        .await
        .unwrap();
    report.outcome = ProcessOutcome::Failed;
    report.conclusion =
        Label::new("Outcome: blocked; Result: verification service unavailable").unwrap();
    on_heap(|| results::record(&f.c.env.env.state, &auth, &report))
        .await
        .unwrap();
    let port = PgDelivery(pool.clone());
    let latest = port.latest(task).await.unwrap().unwrap();
    assert_eq!(latest.outcome, "failed");
    let scope = (latest.workspace_id, latest.project_id);
    let mut conn = pool.acquire().await.unwrap();
    for (at, labels, expected) in [
        (version, Vec::<&str>::new(), "agent_blocked"),
        (version, vec!["review-only"], "agent_blocked"),
        (version + 1, vec![], "failed"),
    ] {
        let node = json!({"version":at,"labels":labels});
        let output = observed(&mut conn, task, scope, Some(&latest), &node, (false, 0, 1))
            .await
            .unwrap();
        assert_eq!(output["facts"]["outcome"], expected);
    }

    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    correction.conclusion = Label::new("Outcome: incomplete; Result: Outcome: blocked").unwrap();
    on_heap(|| results::record(&f.c.env.env.state, &auth, &correction))
        .await
        .unwrap();
    let corrected = port.latest(task).await.unwrap().unwrap();
    assert_eq!(corrected.result_id, correction.report_id);
    for candidate in [&latest, &corrected] {
        let output = observed(
            &mut conn,
            task,
            scope,
            Some(candidate),
            &json!({"version":version}),
            (false, 0, 1),
        )
        .await
        .unwrap();
        assert_eq!(output["facts"]["outcome"], "failed");
    }
}

async fn invariant(f: &terminal::Prepared) -> Value {
    let pool = &f.c.env.env.pool;
    let attempt = f.prepared.original.binding.attempt_id;
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(pool)
            .await
            .unwrap();
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(pool)
            .await
            .unwrap();
    let effects: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_graph_operations WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(pool)
            .await
            .unwrap();
    let release: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_verified_releases WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(pool)
            .await
            .unwrap();
    json!({"hot":hot,"held":held,"effects":effects,"release":release})
}
#[tokio::test]
async fn actual_origin_records_replays_reads_and_never_accepts_or_releases() {
    let (f, report) = Box::pin(ready()).await;
    let before = invariant(&f).await;
    let auth = terminal::auth(&f).await;
    let receipt = results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    assert_eq!(
        receipt.disposition,
        aq_admission::result_record::Disposition::Recorded
    );
    let repeated = results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&repeated).unwrap(),
        serde_json::to_value(&receipt).unwrap()
    );
    let value = results::test_load_retained(&f.c.env.env.state, report.report_id)
        .await
        .unwrap();
    assert_eq!(value["receipt"], serde_json::to_value(&receipt).unwrap());
    let row=sqlx::query("SELECT result_origin_kind,origin_execution_receipt_digest,origin_stopped_ack_bytes_digest FROM profiled_result_reports WHERE report_id=$1").bind(report.report_id).fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("result_origin_kind"), ORIGIN);
    assert!(
        row.get::<Option<String>, _>("origin_execution_receipt_digest")
            .is_some()
    );
    assert!(
        row.get::<Option<String>, _>("origin_stopped_ack_bytes_digest")
            .is_some()
    );
    assert_eq!(before, invariant(&f).await);
    assert!(before["held"].as_bool().unwrap());
}
/// Format 149: the SQL origin owner keys on the admission contract id. A
/// proposal recorded under an earlier digest of a known id stays valid; an
/// unknown id, or the other profile's id, does not.
#[tokio::test]
async fn origin_owner_keys_on_contract_id_not_digest() {
    let (f, report) = Box::pin(ready()).await;
    let auth = terminal::auth(&f).await;
    results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    let pool = &f.c.env.env.pool;
    for name in ["profiled_result_origin_valid", "profiled_close_ungranted"] {
        let pinned: bool = sqlx::query_scalar("SELECT bool_or(prosrc ~ '[0-9a-f]{64}\",\"id\":\"aq\\.admission\\.') FROM pg_proc WHERE proname=$1")
            .bind(name).fetch_one(pool).await.unwrap();
        assert!(!pinned, "{name} pins an admission contract digest");
    }
    let stored: String = sqlx::query_scalar("SELECT proposal#>>'{negotiation,contract,id}' FROM profiled_v2_proposals WHERE attempt_id=$1")
        .bind(report.binding.attempt_id).fetch_one(pool).await.unwrap();
    // The digests the long contracts had before the lease ceiling left the schema.
    let (earlier, other) = if ORIGIN == "phase_execution_v2" {
        ("8e60c64136ecc6653bf9d1488b98832f02eb841bd3ceef462662a098dd13e599", "aq.admission.preparation-registry-refresh-v1-contract")
    } else {
        ("bd9abe1d9ef02970beb7946c007418de90a6066a146698ac0c075da1c1f2a28f", "aq.admission.preparation-v2-contract")
    };
    let long = format!("{stored}.interactive-long-v1");
    for (id, digest, valid) in [
        (stored.as_str(), earlier, true),
        (long.as_str(), earlier, true),
        (long.as_str(), "0000000000000000000000000000000000000000000000000000000000000000", true),
        (other, earlier, false),
        ("aq.admission.unknown-contract", earlier, false),
    ] {
        let mut tx = pool.begin().await.unwrap();
        // History is immutable; only this rolled-back test transaction rewrites it.
        sqlx::query("SET LOCAL session_replication_role=replica").execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE profiled_v2_proposals SET proposal=jsonb_set(proposal,'{negotiation,contract}',jsonb_build_object('id',$2::text,'version','1','digest',$3::text)) WHERE attempt_id=$1")
            .bind(report.binding.attempt_id).bind(id).bind(digest).execute(&mut *tx).await.unwrap();
        let read: bool = sqlx::query_scalar("SELECT profiled_result_origin_valid(attempt_id,node_id,result_origin_kind,origin_execution_receipt_digest,origin_stopped_ack_bytes_digest) FROM profiled_result_reports WHERE report_id=$1")
            .bind(report.report_id).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(read, valid, "{id} {digest}");
        tx.rollback().await.unwrap();
    }
}
#[tokio::test]
async fn historical_phase_read_and_replay_survive_expiry_cancellation_and_runtime_retirement() {
    let (f, report) = Box::pin(ready()).await;
    let auth = terminal::auth(&f).await;
    let pool = &f.c.env.env.pool;
    let mut state = f.c.env.env.state.clone();
    let mut config = (*state.config).clone();
    config.profiled_execution = None;
    state.config = std::sync::Arc::new(config);
    state.clock =
        std::sync::Arc::new(|| chrono::DateTime::from_timestamp_millis(8_000_000_000_000).unwrap());
    let mut hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(report.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    hot["cancellation_id"] = json!(Uuid::new_v4());
    hot["revoked"] = json!(true);
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(report.binding.attempt_id)
        .bind(hot)
        .execute(pool)
        .await
        .unwrap();
    let receipt = results::record(&state, &auth, &report).await.unwrap();
    assert_eq!(receipt.recorded_at_ms, 8_000_000_000_000);
    let replay = results::record(&state, &auth, &report).await.unwrap();
    assert_eq!(receipt.recorded_at_ms, replay.recorded_at_ms);
    results::test_load_retained(&state, report.report_id)
        .await
        .unwrap();
}
#[tokio::test]
async fn correction_race_and_changed_operation_bytes_keep_one_immutable_head() {
    let (f, report) = Box::pin(ready()).await;
    let auth = terminal::auth(&f).await;
    on_heap(|| results::record(&f.c.env.env.state, &auth, &report))
        .await
        .unwrap();
    let mut changed = report.clone();
    changed.conclusion = Label::new("Changed under same identity").unwrap();
    assert!(
        on_heap(|| results::record(&f.c.env.env.state, &auth, &changed))
            .await
            .is_err()
    );
    let mut left = report.clone();
    left.report_id = Uuid::new_v4();
    left.operation_id = Uuid::new_v4();
    left.supersedes = Some(report.report_id);
    let mut right = left.clone();
    right.report_id = Uuid::new_v4();
    right.operation_id = Uuid::new_v4();
    let a = terminal::auth(&f).await;
    let b = terminal::auth(&f).await;
    let (x, y) = tokio::join!(
        on_heap(|| results::record(&f.c.env.env.state, &a, &left)),
        on_heap(|| results::record(&f.c.env.env.state, &b, &right))
    );
    assert_ne!(x.is_ok(), y.is_ok());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_result_reports WHERE attempt_id=$1"
        )
        .bind(report.binding.attempt_id)
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap(),
        2
    );
    on_heap(|| results::test_load_retained(&f.c.env.env.state, report.report_id))
        .await
        .unwrap();
}
#[tokio::test]
async fn origin_pin_forgery_and_legacy_ui_coverage_refuse() {
    let (f, report) = Box::pin(ready()).await;
    let auth = terminal::auth(&f).await;
    let receipt = results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    let pool = &f.c.env.env.pool;
    let actor: Uuid =
        sqlx::query_scalar("SELECT id FROM users WHERE role='owner' ORDER BY id LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
    let page = aq_server::profiled_execution::human_results::read(
        &f.c.env.env.state,
        actor,
        &Default::default(),
        None,
    )
    .await
    .unwrap();
    assert!(
        page.items
            .iter()
            .all(|item| item.report_id != report.report_id)
    );
    assert!(
        aq_server::profiled_execution::human_results::read(
            &f.c.env.env.state,
            actor,
            &Default::default(),
            Some((report.report_id, receipt.report_digest.as_str()))
        )
        .await
        .is_err()
    );
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE profiled_result_reports DISABLE TRIGGER profiled_result_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(sqlx::query("UPDATE profiled_result_reports SET origin_execution_receipt_digest=repeat('a',64) WHERE report_id=$1").bind(report.report_id).execute(&mut *tx).await.is_err(),"actual issuance FK refuses changed origin even without user triggers");
    tx.rollback().await.unwrap();
    results::test_load_retained(&f.c.env.env.state, report.report_id)
        .await
        .unwrap();
}
#[tokio::test]
async fn retained_origin_rechecks_qualification_and_proof_instead_of_trusting_report_row() {
    let (f, report) = Box::pin(ready()).await;
    let auth = terminal::auth(&f).await;
    results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    let pool = &f.c.env.env.pool;
    let original=sqlx::query("SELECT phase_terminal_qualification,proof_bytes,proof_digest FROM profiled_stopped_receipts WHERE attempt_id=$1").bind(report.binding.attempt_id).fetch_one(pool).await.unwrap();
    // Deliberate privileged corruption, with all normal relational/check guards
    // intact. A stored report is not permission to skip its historical owner.
    for field in ["qualification", "proof"] {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "ALTER TABLE profiled_stopped_receipts DISABLE TRIGGER profiled_stopped_immutable",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        if field == "qualification" {
            sqlx::query("UPDATE profiled_stopped_receipts SET phase_terminal_qualification=jsonb_set(phase_terminal_qualification,'{id}','\"aq.changed-qualification/test\"'::jsonb) WHERE attempt_id=$1").bind(report.binding.attempt_id).execute(&mut *tx).await.unwrap();
        } else {
            sqlx::query("UPDATE profiled_stopped_receipts SET proof_bytes=convert_to('{}','UTF8'),proof_digest=encode(sha256(convert_to('{}','UTF8')),'hex') WHERE attempt_id=$1").bind(report.binding.attempt_id).execute(&mut *tx).await.unwrap();
        }
        sqlx::query(
            "ALTER TABLE profiled_stopped_receipts ENABLE TRIGGER profiled_stopped_immutable",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(
            results::record(&f.c.env.env.state, &auth, &report)
                .await
                .is_err(),
            "record accepted corrupted {field}"
        );
        assert!(
            results::test_load_retained(&f.c.env.env.state, report.report_id)
                .await
                .is_err(),
            "historical read accepted corrupted {field}"
        );
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "ALTER TABLE profiled_stopped_receipts DISABLE TRIGGER profiled_stopped_immutable",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("UPDATE profiled_stopped_receipts SET phase_terminal_qualification=$2,proof_bytes=$3,proof_digest=$4 WHERE attempt_id=$1").bind(report.binding.attempt_id).bind(original.get::<Option<Value>,_>("phase_terminal_qualification")).bind(original.get::<Vec<u8>,_>("proof_bytes")).bind(original.get::<String,_>("proof_digest")).execute(&mut *tx).await.unwrap();
        sqlx::query(
            "ALTER TABLE profiled_stopped_receipts ENABLE TRIGGER profiled_stopped_immutable",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        results::test_load_retained(&f.c.env.env.state, report.report_id)
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn sql_no_marker_and_even_forged_release_inputs_cannot_cross_settlement_gate() {
    let (f, report) = Box::pin(ready()).await;
    let auth = terminal::auth(&f).await;
    results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    let pool = &f.c.env.env.pool;
    // BEFORE origin guard must refuse before the duplicate primary key check.
    let no_marker=sqlx::query("INSERT INTO profiled_result_reports(report_id,attempt_id,node_id,operation_id,supersedes,report_bytes,report_digest,receipt_bytes,outcome_artifact_id,result_origin_kind,origin_execution_receipt_digest,origin_stopped_ack_bytes_digest) SELECT report_id,attempt_id,node_id,operation_id,supersedes,report_bytes,report_digest,receipt_bytes,outcome_artifact_id,result_origin_kind,origin_execution_receipt_digest,origin_stopped_ack_bytes_digest FROM profiled_result_reports WHERE report_id=$1").bind(report.report_id).execute(pool).await.unwrap_err();
    assert!(
        no_marker
            .to_string()
            .contains("qualified exact phase result origin required"),
        "{no_marker}"
    );
    let refused=sqlx::query("INSERT INTO profiled_verified_releases(attempt_id,release_id,stop_operation_id,stop_evidence_digest,host_owner_qualification,source_receipt_digest,effect_receipt_digest,verified_at_ms) VALUES($1,$2,$3,repeat('a',64),'{}',repeat('b',64),repeat('c',64),1)").bind(report.binding.attempt_id).bind(Uuid::new_v4()).bind(Uuid::new_v4()).execute(pool).await.unwrap_err();
    assert!(
        refused
            .to_string()
            .contains(if aq_server::STATE_FORMAT_VERSION >= 39 {
                "qualified settlement release owner required"
            } else {
                "phase settlement protocol not qualified"
            }),
        "{refused}"
    );
    assert!(invariant(&f).await["held"].as_bool().unwrap());
    assert_eq!(invariant(&f).await["release"], 0);
}
#[tokio::test]
async fn held_node_lock_rechecks_revoked_credential_before_any_report_commit() {
    let (f, report) = Box::pin(ready()).await;
    let auth = terminal::auth(&f).await;
    let state = f.c.env.env.state.clone();
    let pool = &f.c.env.env.pool;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(auth.id())
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let pending_report = report.clone();
    let task = tokio::spawn(async move { results::record(&state, &auth, &pending_report).await });
    tokio::time::timeout(std::time::Duration::from_secs(1),async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT id FROM nodes WHERE id=%FOR KEY SHARE')").fetch_one(pool).await.unwrap();
            if waiting {break;}
            tokio::task::yield_now().await;
        }
    }).await.expect("report must be demonstrably blocked on current node lock");
    sqlx::query("UPDATE nodes SET revoked_at=clock_timestamp() WHERE id=$1")
        .bind(f.c.request.identity.node_id)
        .execute(&mut *held)
        .await
        .unwrap();
    held.commit().await.unwrap();
    assert!(matches!(
        task.await.unwrap(),
        Err(aq_server::error::AppError::CredentialRevoked)
    ));
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports WHERE attempt_id=$1")
            .bind(report.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(n, 0);
    assert!(invariant(&f).await["held"].as_bool().unwrap());
}
#[tokio::test]
async fn running_claim_cannot_become_recorded_without_actual_stopped_owner() {
    let (f, report) = Box::pin(ready_with_stop(false)).await;
    let auth = terminal::auth(&f).await;
    let before = invariant(&f).await;
    assert!(
        results::record(&f.c.env.env.state, &auth, &report)
            .await
            .is_err()
    );
    assert_eq!(before, invariant(&f).await);
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports WHERE attempt_id=$1")
            .bind(report.binding.attempt_id)
            .fetch_one(&f.c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(n, 0);
}
#[tokio::test]
async fn head_failure_rolls_back_report_and_exact_retry_records_once() {
    let (f, report) = Box::pin(ready()).await;
    let auth = terminal::auth(&f).await;
    let pool = &f.c.env.env.pool;
    sqlx::raw_sql("CREATE FUNCTION fixture_refuse_result_head() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic crash boundary between report and head'; END $$; CREATE TRIGGER fixture_refuse_result_head BEFORE INSERT ON profiled_result_heads FOR EACH ROW EXECUTE FUNCTION fixture_refuse_result_head()")
        .execute(pool).await.unwrap();
    assert!(
        results::record(&f.c.env.env.state, &auth, &report)
            .await
            .is_err()
    );
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports WHERE attempt_id=$1")
            .bind(report.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(n, 0, "report cannot survive aborted head transaction");
    sqlx::raw_sql("DROP TRIGGER fixture_refuse_result_head ON profiled_result_heads; DROP FUNCTION fixture_refuse_result_head()")
        .execute(pool).await.unwrap();
    results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    results::test_load_retained(&f.c.env.env.state, report.report_id)
        .await
        .unwrap();
    assert!(invariant(&f).await["held"].as_bool().unwrap());
}
