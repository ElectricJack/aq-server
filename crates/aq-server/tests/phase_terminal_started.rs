//! Actual phase SQL owners; explicit synthetic runtime/Running observation.
//! No physical producer, provider, result settlement or task release qualification.
mod common;
#[allow(dead_code)]
#[path = "common/phase_terminal.rs"]
mod terminal;
use aq_admission::preparation_v2::ExecutionReceipt;
use aq_phase_terminal_protocol as wire;
use aq_server::{
    AppState,
    error::AppError,
    profiled_execution::{phase_terminal::started, policies},
};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use uuid::Uuid;
struct Case {
    f: terminal::Prepared,
    execution: ExecutionReceipt,
    request: wire::StartedRequest,
}
async fn setup() -> Case {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let request = terminal::started_request(&f, &execution);
    Case {
        f,
        execution,
        request,
    }
}
async fn audit(c: &Case) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('hot',(SELECT to_jsonb(h) FROM profiled_authority_hot h),'journal',(SELECT to_jsonb(j) FROM profiled_owner_journals j),'started',(SELECT jsonb_agg(to_jsonb(s)) FROM profiled_started_receipts s),'stopped',(SELECT count(*) FROM profiled_stopped_receipts),'closure',(SELECT count(*) FROM profiled_launch_closures),'holds',(SELECT holds_task FROM execution_attempts LIMIT 1))").fetch_one(&c.f.c.env.env.pool).await.unwrap()
}
async fn call(
    c: &Case,
    state: &AppState,
    q: &wire::StartedRequest,
) -> Result<wire::StartedReceipt, AppError> {
    started::record(state, &terminal::auth(&c.f).await, q).await
}
fn canonical(v: &impl serde::Serialize) -> Vec<u8> {
    let mut v = serde_json::to_value(v).unwrap();
    v.sort_all_objects();
    serde_json::to_vec(&v).unwrap()
}
fn clock(c: &Case, now: u64) -> (AppState, Arc<AtomicI64>) {
    let at = Arc::new(AtomicI64::new(now as i64));
    let read = at.clone();
    let mut state = c.f.c.env.env.state.clone();
    state.clock = Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(read.load(Ordering::SeqCst)).unwrap()
    });
    (state, at)
}
#[tokio::test]
async fn actual_phase_started_records_original_ack_without_extending_grant_and_replays_after_expiry()
 {
    let c = Box::pin(setup()).await;
    let before = audit(&c).await;
    let receipt = call(&c, &c.f.c.env.env.state, &c.request).await.unwrap();
    receipt.validate_for(&c.request).unwrap();
    let after = audit(&c).await;
    assert_eq!(
        after["hot"]["state"]["grant"],
        before["hot"]["state"]["grant"]
    );
    assert_eq!(
        after["hot"]["state"]["execution_deadline_ms"],
        before["hot"]["state"]["execution_deadline_ms"]
    );
    assert_eq!(after["hot"]["state"]["stop"]["kind"], "running");
    assert_eq!(after["journal"]["phase"], "started");
    assert_eq!(after["holds"], true);
    assert_eq!(after["stopped"], 0);
    let bytes: Vec<u8> =
        sqlx::query_scalar("SELECT phase_ack_bytes FROM profiled_started_receipts")
            .fetch_one(&c.f.c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(bytes, canonical(&receipt));
    let (late, _) = clock(&c, c.execution.grant.expires_at_ms + 1);
    let replay = call(&c, &late, &c.request).await.unwrap();
    assert_eq!(canonical(&receipt), canonical(&replay));
    assert_eq!(audit(&c).await, after);
}
#[tokio::test]
async fn exact_ack_survives_retired_launch_eligibility_but_new_start_does_not() {
    let c = Box::pin(setup()).await;
    let receipt = call(&c, &c.f.c.env.env.state, &c.request).await.unwrap();
    let mut retired = c.f.c.env.env.state.clone();
    let mut config = (*retired.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
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
    p.revision += 1;
    p.validate().unwrap();
    config.profiled_execution = Some(Arc::new(p));
    retired.config = Arc::new(config);
    policies::install(&retired).await.unwrap();
    let before = audit(&c).await;
    let replay = call(&c, &retired, &c.request).await.unwrap();
    assert_eq!(canonical(&receipt), canonical(&replay));
    let mut changed = c.request.clone();
    changed.original.operation_id = Uuid::new_v4();
    assert!(call(&c, &retired, &changed).await.is_err());
    assert_eq!(audit(&c).await, before);
}
#[tokio::test]
async fn wrong_outer_origin_or_inner_scope_refuses_without_partial_state() {
    let c = Box::pin(setup()).await;
    let before = audit(&c).await;
    let mut variants = vec![c.request.clone(), c.request.clone(), c.request.clone()];
    variants[0].origin.execution_receipt_digest =
        aq_admission::Digest::of(b"another phase execution");
    variants[1].origin.accepted_receipt_digest =
        aq_admission::Digest::of(b"another accepted preparation");
    variants[2].original.runtime.digest = aq_admission::Digest::of(b"another runtime");
    for q in variants {
        assert!(call(&c, &c.f.c.env.env.state, &q).await.is_err());
        assert_eq!(audit(&c).await, before);
    }
}
#[tokio::test]
async fn expired_or_cancelled_current_authority_cannot_record_first_started() {
    let c = Box::pin(setup()).await;
    let before = audit(&c).await;
    let (expired, _) = clock(&c, c.execution.grant.expires_at_ms);
    assert!(matches!(
        call(&c, &expired, &c.request).await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(audit(&c).await, before);
    sqlx::query("UPDATE profiled_authority_hot SET state=state||jsonb_build_object('cancellation_id',$1::text,'revoked',true,'status','cancelling')").bind(Uuid::new_v4().to_string()).execute(&c.f.c.env.env.pool).await.unwrap();
    let cancelled = audit(&c).await;
    assert!(call(&c, &c.f.c.env.env.state, &c.request).await.is_err());
    assert_eq!(audit(&c).await, cancelled);
}
#[tokio::test]
async fn changed_project_and_current_principal_cannot_gain_started_authority() {
    let c = Box::pin(setup()).await;
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1,binding=jsonb_set(binding,'{enabled}','false'::jsonb)").execute(&c.f.c.env.env.pool).await.unwrap();
    let before = audit(&c).await;
    assert!(call(&c, &c.f.c.env.env.state, &c.request).await.is_err());
    assert_eq!(audit(&c).await, before);
}
#[tokio::test]
async fn retired_installed_policy_refuses_first_started_even_with_current_configuration() {
    let c = Box::pin(setup()).await;
    let mut state = c.f.c.env.env.state.clone();
    let mut config = (*state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    p.revision += 1;
    for project in &mut p.projects {
        project.enabled = false;
    }
    config.profiled_execution = Some(Arc::new(p));
    state.config = Arc::new(config);
    policies::install(&state).await.unwrap();
    let before = audit(&c).await;
    assert!(call(&c, &state, &c.request).await.is_err());
    assert_eq!(audit(&c).await, before);
}
#[tokio::test]
async fn stale_authenticated_context_is_rechecked_before_any_started_mutation() {
    let c = Box::pin(setup()).await;
    let auth = terminal::auth(&c.f).await;
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(auth.id())
        .bind("rotated synthetic verifier")
        .execute(&c.f.c.env.env.pool)
        .await
        .unwrap();
    let before = audit(&c).await;
    assert!(matches!(
        started::record(&c.f.c.env.env.state, &auth, &c.request).await,
        Err(AppError::CredentialRevoked)
    ));
    assert_eq!(audit(&c).await, before);
}
#[tokio::test]
async fn receipt_failure_rolls_back_journal_hot_and_allows_same_original_retry() {
    let c = Box::pin(setup()).await;
    let before = audit(&c).await;
    sqlx::raw_sql("CREATE FUNCTION fixture_refuse_phase_started() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic Started receipt fault'; END $$; CREATE TRIGGER zz_fixture_refuse_phase_started AFTER INSERT ON profiled_started_receipts FOR EACH ROW EXECUTE FUNCTION fixture_refuse_phase_started();").execute(&c.f.c.env.env.pool).await.unwrap();
    assert!(matches!(
        call(&c, &c.f.c.env.env.state, &c.request).await,
        Err(AppError::Internal)
    ));
    assert_eq!(audit(&c).await, before);
    sqlx::query("DROP TRIGGER zz_fixture_refuse_phase_started ON profiled_started_receipts")
        .execute(&c.f.c.env.env.pool)
        .await
        .unwrap();
    call(&c, &c.f.c.env.env.state, &c.request).await.unwrap();
}
#[path = "phase_terminal_started/clock.rs"]
mod clock_tests;
