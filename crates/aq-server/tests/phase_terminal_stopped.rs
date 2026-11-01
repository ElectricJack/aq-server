//! Actual phase SQL owners; proof DTOs are explicitly synthetic, not kernel evidence.
mod common;
#[allow(dead_code)]
#[path = "common/phase_terminal.rs"]
mod terminal;
use aq_server::profiled_execution::phase_terminal::{started, stopped};
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::Arc;

async fn snapshot(f: &terminal::Prepared) -> Value {
    let id = f.prepared.original.binding.attempt_id;
    let pool = &f.c.env.env.pool;
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    let versions:(i64,i64,i64)=sqlx::query_as("SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1").bind(id).fetch_one(pool).await.unwrap();
    let head: Value =
        sqlx::query_scalar("SELECT to_jsonb(j) FROM profiled_owner_journals j WHERE attempt_id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM profiled_started_receipts),(SELECT count(*) FROM profiled_stopped_receipts),(SELECT count(*) FROM profiled_launch_closures)").fetch_one(pool).await.unwrap();
    json!({"hot":hot,"versions":versions,"head":head,"counts":counts})
}
#[tokio::test]
async fn expired_cancelled_paired_stop_retains_claim_and_exact_ack_after_runtime_retirement() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let auth = terminal::auth(&f).await;
    let start = terminal::started_request(&f, &execution);
    started::record(&f.c.env.env.state, &auth, &start)
        .await
        .unwrap();
    let stop = terminal::stop_request(&start);
    let before = snapshot(&f).await;
    let pool = &f.c.env.env.pool;
    let mut hot = before["hot"].clone();
    hot["status"] = json!("cancelling");
    hot["revoked"] = json!(true);
    hot["cancellation_id"] = json!(uuid::Uuid::new_v4());
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(start.original.binding.attempt_id)
        .bind(&hot)
        .execute(pool)
        .await
        .unwrap();
    // Original publication/Accept/Grant used real clocks. This fixed clock models
    // cleanup after expiry only; it does not qualify new authority under slow I/O.
    let mut state = f.c.env.env.state.clone();
    let at = execution.grant.expires_at_ms + 100;
    state.clock = Arc::new(move || chrono::DateTime::from_timestamp_millis(at as i64).unwrap());
    let mut config = (*state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    p.revision += 1;
    p.qualified_runtimes.clear();
    p.context_qualifications.clear();
    p.definition_bodies.retain(|d| {
        !d.artifact()
            .id
            .as_str()
            .starts_with("aq.accepted-preparation-policy/")
            && !d
                .artifact()
                .id
                .as_str()
                .starts_with("aq.phase-execution-qualification/")
    });
    config.profiled_execution = Some(Arc::new(p));
    state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&state)
        .await
        .unwrap();
    let ack = stopped::record(&state, &auth, &stop).await.unwrap();
    ack.validate_for(&stop).unwrap();
    assert_eq!(ack.original.recorded_at_ms, at);
    let after = snapshot(&f).await;
    assert_eq!(after["hot"]["status"], "finished");
    assert_eq!(after["hot"]["stop"]["kind"], "stopped");
    for k in ["grant", "execution_deadline_ms", "retention", "effects"] {
        assert_eq!(after["hot"][k], before["hot"][k], "{k}")
    }
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(start.original.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(held);
    assert_eq!(
        serde_json::to_value(stopped::record(&state, &auth, &stop).await.unwrap()).unwrap(),
        json!(ack)
    );
    assert_eq!(snapshot(&f).await, after);
    let mut changed = stop.clone();
    changed.original.evidence.reason = aq_admission::transport::StopReason::Cancellation;
    assert!(stopped::record(&state, &auth, &changed).await.is_err());
    assert_eq!(snapshot(&f).await, after);
    // Legacy routes must not reinterpret the actual phase inner evidence.
    assert!(
        aq_server::profiled_execution::stopped::record(&state, &auth, &stop.original)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn missing_started_and_foreign_proof_refuse_without_authority_mutation() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let auth = terminal::auth(&f).await;
    let start = terminal::started_request(&f, &execution);
    let stop = terminal::stop_request(&start);
    let before = snapshot(&f).await;
    assert!(
        stopped::record(&f.c.env.env.state, &auth, &stop)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f).await, before);
    started::record(&f.c.env.env.state, &auth, &start)
        .await
        .unwrap();
    let before = snapshot(&f).await;
    let mut foreign = stop.clone();
    foreign.original.proof.observation.guardian.nonce = uuid::Uuid::new_v4();
    assert!(
        stopped::record(&f.c.env.env.state, &auth, &foreign)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f).await, before);
    let mut wrong = stop;
    wrong.origin.execution_receipt_digest = aq_admission::Digest::of(b"foreign phase receipt");
    assert!(
        stopped::record(&f.c.env.env.state, &auth, &wrong)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f).await, before);
}
#[tokio::test]
async fn stopped_insert_failure_rolls_back_journal_and_hot_then_same_intent_retries() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let auth = terminal::auth(&f).await;
    let start = terminal::started_request(&f, &execution);
    started::record(&f.c.env.env.state, &auth, &start)
        .await
        .unwrap();
    let stop = terminal::stop_request(&start);
    let before = snapshot(&f).await;
    let pool = &f.c.env.env.pool;
    sqlx::raw_sql("CREATE FUNCTION fail_phase_stop() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected phase stop persistence failure'; END $$; CREATE CONSTRAINT TRIGGER zz_fail_phase_stop AFTER INSERT ON profiled_stopped_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_phase_stop();").execute(pool).await.unwrap();
    assert!(
        stopped::record(&f.c.env.env.state, &auth, &stop)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f).await, before);
    sqlx::query("DROP TRIGGER zz_fail_phase_stop ON profiled_stopped_receipts")
        .execute(pool)
        .await
        .unwrap();
    stopped::record(&f.c.env.env.state, &auth, &stop)
        .await
        .unwrap();
    let retained=sqlx::query("SELECT phase_ack_bytes,phase_envelope_bytes FROM profiled_stopped_receipts WHERE attempt_id=$1").bind(start.original.binding.attempt_id).fetch_one(pool).await.unwrap();
    assert!(!retained.get::<Vec<u8>, _>("phase_ack_bytes").is_empty());
    assert!(
        !retained
            .get::<Vec<u8>, _>("phase_envelope_bytes")
            .is_empty()
    );
}
#[tokio::test]
async fn paired_stop_does_not_qualify_old_sql_result_or_release_owners() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let auth = terminal::auth(&f).await;
    let start = terminal::started_request(&f, &execution);
    started::record(&f.c.env.env.state, &auth, &start)
        .await
        .unwrap();
    let stop = terminal::stop_request(&start);
    stopped::record(&f.c.env.env.state, &auth, &stop)
        .await
        .unwrap();
    let before = snapshot(&f).await;
    let pool = &f.c.env.env.pool;
    // Relational-only denied requests, not valid report/release authority. The
    // exact new guard context proves refusal precedes unrelated payload checks.
    let bytes = b"{}".to_vec();
    let digest = aq_admission::Digest::of(&bytes);
    let error=sqlx::query("INSERT INTO profiled_result_reports(report_id,attempt_id,node_id,operation_id,report_bytes,report_digest,receipt_bytes,outcome_artifact_id) VALUES($1,$2,$3,$4,$5,$6,$5,$1)").bind(uuid::Uuid::new_v4()).bind(start.original.binding.attempt_id).bind(auth.id()).bind(uuid::Uuid::new_v4()).bind(&bytes).bind(digest.as_str()).execute(pool).await.unwrap_err();
    assert!(
        error
            .as_database_error()
            .unwrap()
            .message()
            .contains("phase result and settlement protocol not qualified")
    );
    let error=sqlx::query("INSERT INTO profiled_verified_releases(attempt_id,release_id,stop_operation_id,stop_evidence_digest,host_owner_qualification,source_receipt_digest,effect_receipt_digest,verified_at_ms) VALUES($1,$2,$3,$4,'{}',$4,$4,1)").bind(start.original.binding.attempt_id).bind(uuid::Uuid::new_v4()).bind(stop.original.evidence.stop_operation_id).bind(digest.as_str()).execute(pool).await.unwrap_err();
    assert!(
        error
            .as_database_error()
            .unwrap()
            .message()
            .contains("phase result and settlement protocol not qualified")
    );
    assert_eq!(snapshot(&f).await, before);
}
