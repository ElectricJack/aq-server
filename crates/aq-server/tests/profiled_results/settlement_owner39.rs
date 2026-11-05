//! Actual SQL decision/settlement/transport owners with SYNTHETIC launch, AQ
//! binding, Git inspection and Graph peers. Not provider/physical/real-Git or
//! GATE04 qualification. Every positive owner fact is obtained through its port.
use super::*;
#[path = "settlement_owner39/completion_outbox.rs"]
mod completion_outbox;
#[path = "settlement_owner39/source_retention_gc.rs"]
mod source_retention_gc;
use aq_server::{
    profiled_execution::settlement::{self, EvaluationDisposition},
    reviews::combined::*,
};
use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicUsize};
const POLICY: &str =
    r#"{"schema_version":1,"kind":"source_change_reviewed_human_owner_no_checks_v1"}"#;
const USER_AUTHORIZATION_LOCK: &str = "SELECT role,email,issuer,subject,xmin::text AS authorization_revision FROM users WHERE id=$1 FOR SHARE";
const CANCELLATION_NODE_LOCK: &str = "SELECT id FROM nodes WHERE id=$1 FOR UPDATE";
pub(super) fn policy() -> aq_server::profiled_execution::definitions::Definition {
    aq_server::profiled_execution::definitions::Definition::new(
        artifact("pilot-settlement-policy", POLICY.as_bytes()),
        ArtifactKind::Definition,
        POLICY.into(),
    )
    .unwrap()
}
pub(super) fn graph_context(f: &Fixture) -> Value {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
    ))
    .unwrap();
    let mut node = fixture["nodes"]
        .as_object()
        .unwrap()
        .values()
        .find(|n| n["kind"] == "task" && n["task_state"] == "active")
        .unwrap()
        .clone();
    node["id"] = json!(f.selection.lineage.task_id);
    node["workspace_id"] = json!(f.selection.lineage.workspace_id);
    node["version"] = json!(f.profiled.offer.manifest.bindings.task_revision);
    node["content_revision"] = json!(1);
    json!({"node":node,"nodes":[],"edges":[],"pinned_revisions":[],"truncated":false})
}
pub(super) fn context_bytes(f: &Fixture) -> Vec<u8> {
    let raw = graph_context(f);
    let mut node = raw["node"].clone();
    let fields = [
        "id",
        "kind",
        "title",
        "body",
        "acceptance_criteria",
        "labels",
        "task_state",
        "verification",
        "content_type",
        "retired",
        "deleted",
        "version",
        "content_revision",
    ];
    node.as_object_mut()
        .unwrap()
        .retain(|k, _| fields.contains(&k.as_str()));
    node["version"] = json!(f.profiled.offer.manifest.bindings.task_revision.to_string());
    node["content_revision"] = json!("1");
    let mut v = json!({"observed_at":chrono::DateTime::from_timestamp_millis(NOW as i64).unwrap().to_rfc3339(),"projection":"authorized_scope","root_ids":[f.selection.lineage.task_id],"nodes":[node],"edges":[],"pinned_revisions":[],"coverage":{"status":"complete","complete_for":"authorized_projection","reasons":[]}});
    v.sort_all_objects();
    serde_json::to_vec(&v).unwrap()
}
pub(super) async fn initial_project(f: &Fixture) {
    let p = &f.profiled;
    let a = policy();
    let binding = json!({"schema_version":1,"project_id":f.selection.lineage.project_id,"name":"Synthetic settlement fixture","enabled":true,"repository_id":p.source.repository_id,"repository_binding_generation":1,"repository_adapter_reference":"aq.git.configured","graph_adapter_reference":"aq.graph.configured","graph_workspace_id":f.selection.lineage.workspace_id,"catalog_digest":f.selection.catalog_digest,"allowed_agent_profiles":["implementer@2"],"allowed_intelligence_profiles":["engineering@2"],"allowed_execution_profiles":["confined-development@2"],"allowed_node_ids":[f.node.node_id],"allowed_worker_groups":[],"allowed_environments":["confined-worktree"],"policy":{"identity":a.artifact().id,"revision":a.artifact().version,"digest":a.artifact().digest},"context":[],"ceilings":{"permissions":[{"operation":"task.read","scope_reference":format!("project:{}",f.selection.lineage.project_id)}],"context_bytes":1048576,"wall_time_ms":60000,"memory_bytes":268435456,"cpu_millis":60000,"process_limit":8,"cost_limit_microunits":null,"currency":null,"require_hard_cost_enforcement":false}});
    let typed: aq_server::project_bindings::domain::ProjectBinding =
        serde_json::from_value(binding.clone()).unwrap();
    typed.validate(&Default::default()).unwrap();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,$3,'{}')").bind(f.selection.lineage.project_id).bind(binding).bind(NOW as i64).execute(&f.db.pool().await).await.unwrap();
}
fn external(attempt: Uuid) -> ExternalDeclaration {
    ExternalDeclaration {
        scope: ExternalScope::TrustedNodeSourceAndAqReadsOnly,
        worktree_attempt_id: attempt,
        known_effects: vec![],
        reason: "Fixture human explicitly reconciles declared trusted-node source scope".into(),
        residual_uncertainty:
            "Synthetic peers do not establish absence of arbitrary shell or network effects".into(),
    }
}
#[derive(Default)]
struct ResponseGate {
    enabled: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
struct Case {
    env: ReviewEnv,
    actor: aq_server::store::User,
    service: settlement::Service,
    request: settlement::EvaluateRequest,
    _graph: tokio::task::JoinHandle<()>,
    writes: Arc<AtomicUsize>,
    loss: Arc<AtomicBool>,
    gate: Arc<ResponseGate>,
}
async fn setup() -> Case {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../protocol/agent-access/v1/fixtures/mutation-delivery.json"
    ))
    .unwrap();
    let mut profile = fixture["proposal_profiles"][0]["profile"].clone();
    profile["capabilities"] = json!(["read.get", "read.context"]);
    let mut env = Box::pin(review_env_options(16, Some(profile.clone()), true)).await;
    // Activate the real immutable fixture catalog through its authenticated owner,
    // rather than assigning a digest-shaped pointer in SQL.
    use tower::ServiceExt;
    let selection: Value =
        serde_json::from_str(include_str!("../fixtures/selection-v2.json")).unwrap();
    let response=aq_server::build_admin_router(env.f.state.clone()).oneshot(axum::http::Request::builder().method("POST").uri("/api/profiles/catalogs/activate").header("content-type","application/json").header("X-AQ-Request","1").body(axum::body::Body::from(json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"artifact_json":selection["catalog_artifact_json"]}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status(), 200);
    let r = {
        let mut tx = env.f.db.pool().await.begin().await.unwrap();
        let r = results::load_locked(&env.f.state, &mut tx, env.report.report_id)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        r
    };
    let SessionIdentity::Remote {
        node_id,
        boot_id,
        session_id,
        ..
    } = r.report.binding.session
    else {
        unreachable!()
    };
    let binding = json!({"task_id":r.spec.task.task_id,"attempt_id":r.report.binding.attempt_id,"session_id":session_id,"generation":r.report.binding.assignment_generation,"node_id":node_id,"boot_id":boot_id,"project_id":r.spec.lifecycle.project_id,"workspace_id":r.spec.task.workspace_id,"resolved_spec_digest":r.report.binding.execution_spec_digest,"profile_digest":profile_digest(&profile),"context_digest":r.spec.task_observation.digest});
    assert!(aq_agent_api::valid("Binding", &binding));
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,$4,'{}')").bind(r.report.binding.attempt_id).bind(node_id).bind(&binding).bind(&profile).execute(&env.f.db.pool().await).await.unwrap();
    let actual: String = sqlx::query_scalar("SELECT settlement_sender_key($1)")
        .bind(&binding)
        .fetch_one(&env.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(
        actual,
        aq_agent_api::digest(&json!({"kind":"session","binding":binding})).unwrap()
    );
    let graph = graph_context(&env.f);
    let write_graph = graph.clone();
    let writes = Arc::new(AtomicUsize::new(0));
    let loss = Arc::new(AtomicBool::new(false));
    let wc = writes.clone();
    let lc = loss.clone();
    let expected_key = Arc::new(std::sync::Mutex::new(None::<String>));
    let key = expected_key.clone();
    let gate = Arc::new(ResponseGate::default());
    let response_gate = gate.clone();
    let app=Router::new().route("/v1/workspaces/{workspace}/nodes/{task}/context",get(move||{let g=graph.clone();async move{([("X-AQ-Graph-API-Version","1")],Json(g))}})).route("/v1/workspaces/{workspace}/mutations",post(move|Json(body):Json<Value>|{
        let g=write_graph.clone();let wc=wc.clone();let lc=lc.clone();let key=key.clone();let gate=response_gate.clone();
        async move{
            assert_eq!(body["operations"][0]["op"],"update_node");assert_eq!(body["operations"][0]["id"],g["node"]["id"]);assert_eq!(body["operations"][0]["expected_version"],g["node"]["version"]);
            {let id=body["idempotency_key"].as_str().unwrap().to_owned();let mut guard=key.lock().unwrap();if let Some(old)=guard.as_ref(){assert_eq!(old,&id)}else{*guard=Some(id);wc.fetch_add(1,Ordering::SeqCst);}}
            if gate.enabled.swap(false,Ordering::SeqCst){gate.entered.notify_one();gate.release.notified().await;}
            let mut node=g["node"].clone();node["task_state"]=json!("completed");node["version"]=json!(node["version"].as_u64().unwrap()+1);node["content_revision"]=json!(node["content_revision"].as_u64().unwrap()+1);
            let reply=json!({"change_set_id":"00000000-0000-0000-0000-000000000039","results":[node]});let status=if lc.swap(false,Ordering::SeqCst){axum::http::StatusCode::SERVICE_UNAVAILABLE}else{axum::http::StatusCode::OK};(status,[("X-AQ-Graph-API-Version","1")],Json(reply))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    use std::os::unix::fs::PermissionsExt;
    let path = env._directory.path().join("graph-key");
    std::fs::write(&path, "x".repeat(32)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    env.f.state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: format!("http://{address}/"),
            graph_token_file: path,
            mutation_actor_id: Some("synthetic-settlement-service".into()),
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let actor = env
        .f
        .state
        .store
        .find_user_by_binding("dev", "owner@localhost")
        .await
        .unwrap()
        .unwrap();
    let prepared = env
        .service
        .prepare_combined_as(
            &actor,
            env.report.report_id,
            env.operation,
            external(r.report.binding.attempt_id),
        )
        .await
        .unwrap();
    let decision = aq_review_protocol::DecisionRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: Uuid::new_v4(),
        subject: prepared.subject().artifact().unwrap(),
        eligible_owners: vec![actor.id],
        eligibility_project_revision: 1,
        review_policy: prepared.subject().code.validation_policy.clone(),
        expires_at_ms: NOW + 30000,
    };
    env.service
        .admit_combined(actor.id, &prepared, &decision)
        .await
        .unwrap();
    let answer = aq_review_protocol::ResolveRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: decision.decision_id,
        expected_decision_revision: 1,
        choice: aq_review_protocol::Choice::Approve,
        comment: None,
    };
    assert!(
        env.service.resolve(actor.id, &answer).await.is_err(),
        "old code-only API must refuse combined profile"
    );
    env.service
        .resolve_combined_as(&actor, &answer, &decision.subject)
        .await
        .unwrap();
    let request = settlement::EvaluateRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        report_id: env.report.report_id,
        source_operation_id: env.operation,
        decision_id: decision.decision_id,
        subject: decision.subject,
    };
    // Different actual bound requester/approver identities let the lock-barrier
    // tests isolate the FINAL actor wait from the earlier review-owner wait.
    let at = chrono::DateTime::from_timestamp_millis(NOW as i64).unwrap();
    env.f
        .state
        .store
        .invite_user(
            "settlement-requester@localhost",
            aq_server::store::Role::Owner,
            at,
        )
        .await
        .unwrap();
    let actor = env
        .f
        .state
        .store
        .bind_invited_user(
            "settlement-requester@localhost",
            "dev",
            "settlement-requester@localhost",
            at,
        )
        .await
        .unwrap()
        .unwrap();
    let service = settlement::Service::new(env.f.state.clone(), env.service.clone());
    Case {
        env,
        actor,
        service,
        request,
        _graph: server,
        writes,
        loss,
        gate,
    }
}
fn profile_digest(v: &Value) -> String {
    aq_agent_api::digest(v).unwrap()
}
#[tokio::test]
async fn native_cancellation_before_evaluation_refuses_success() {
    let c = Box::pin(setup()).await;
    let (status, receipt) = common::native_cancellation::cancel(
        &c.env.f.state,
        c.env.report.binding.attempt_id,
        Uuid::new_v4(),
        "cancel before acceptance",
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    let evaluation = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    assert_eq!(evaluation.disposition, EvaluationDisposition::Rejected);
    assert_eq!(
        evaluation.reason,
        Some(settlement::eligibility::Reason::Cancelled)
    );
    assert!(evaluation.completion_operation_id.is_none());
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    c.env.peer.abort();
    c._graph.abort();
}

#[tokio::test]
async fn native_cancellation_after_accepted_evaluation_preserves_completion() {
    let c = Box::pin(setup()).await;
    let accepted = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    assert_eq!(accepted.disposition, EvaluationDisposition::Accepted);
    let op = Uuid::new_v4();
    let (status, receipt) = common::native_cancellation::cancel(
        &c.env.f.state,
        c.env.report.binding.attempt_id,
        op,
        "too late",
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["disposition"], "already_accepted");
    assert!(receipt["cancellation_id"].is_null());
    let done = Box::pin(
        c.service
            .complete(&c.actor, accepted.completion_operation_id.unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(done.state, "confirmed");
    assert_eq!(c.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        json!(
            Box::pin(c.service.evaluate(&c.actor, &c.request))
                .await
                .unwrap()
        ),
        json!(accepted)
    );
    c.env.peer.abort();
    c._graph.abort();
}
#[tokio::test]
async fn native_cancellation_waits_for_acceptance_owner_and_preserves_accepted_result() {
    let c = Box::pin(setup()).await;
    let pool = c.env.f.db.pool().await;
    let mut user_lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(c.actor.id)
        .execute(&mut *user_lock)
        .await
        .unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *user_lock)
        .await
        .unwrap();
    let service = c.service.clone();
    let actor = c.actor.clone();
    let request = c.request.clone();
    let mut evaluating =
        tokio::spawn(async move { Box::pin(service.evaluate(&actor, &request)).await });
    let acceptance_owner =
        wait_for_lock(&pool, blocker, USER_AUTHORIZATION_LOCK, &mut evaluating).await;
    let state = c.env.f.state.clone();
    let attempt = c.env.report.binding.attempt_id;
    let mut cancelling = tokio::spawn(async move {
        aq_server::profiled_execution::cancellation::cancel(
            &state,
            attempt,
            Uuid::new_v4(),
            "concurrent cancellation",
        )
        .await
    });
    wait_for_lock(
        &pool,
        acceptance_owner,
        CANCELLATION_NODE_LOCK,
        &mut cancelling,
    )
    .await;
    user_lock.commit().await.unwrap();
    let accepted = evaluating.await.unwrap().unwrap();
    assert_eq!(accepted.disposition, EvaluationDisposition::Accepted);
    let cancelled = cancelling.await.unwrap().unwrap();
    assert_eq!(json!(cancelled.disposition), "already_accepted");
    assert!(cancelled.cancellation_id.is_none());
    let completion = Box::pin(
        c.service
            .complete(&c.actor, accepted.completion_operation_id.unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(completion.state, "confirmed");
    assert_eq!(c.writes.load(Ordering::SeqCst), 1);
    c.env.peer.abort();
    c._graph.abort();
}

#[tokio::test]
async fn settlement39_actual_owners_accept_one_combined_decision_hold_until_exact_graph_receipt() {
    let c = Box::pin(setup()).await;
    let receipt = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    assert_eq!(receipt.disposition, EvaluationDisposition::Accepted);
    let op = receipt.completion_operation_id.unwrap();
    let pool = c.env.f.db.pool().await;
    let original: (Value, i64) =
        sqlx::query_as("SELECT state,record_revision FROM execution_attempts WHERE attempt_id=$1")
            .bind(receipt.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(original.0["status"], "offered");
    assert_eq!(original.0["retention"]["kind"], "pending");
    // An accepted evaluation alone cannot refresh or release the snapshot.
    assert!(sqlx::query("UPDATE execution_attempts a SET state=a.state || (h.state-ARRAY['owner','preparation_operation_id']),holds_task=false,execution_deadline_ms=(h.state->>'execution_deadline_ms')::bigint FROM profiled_authority_hot h WHERE a.attempt_id=$1 AND h.attempt_id=a.attempt_id")
        .bind(receipt.attempt_id).execute(&pool).await.is_err());
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(receipt.attempt_id)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    let replay = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    assert_eq!(json!(receipt), json!(replay));
    let mut correction = c.env.report.clone();
    correction.supersedes = Some(correction.report_id);
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    assert!(
        results::record(&c.env.f.state, &context(&c.env.f).await, &correction)
            .await
            .is_err()
    );
    c.loss.store(true, Ordering::SeqCst);
    let unknown = Box::pin(c.service.complete(&c.actor, op)).await.unwrap();
    assert_eq!(unknown.state, "possible");
    assert!(unknown.receipt.is_none());
    let possible: (Value, i64) =
        sqlx::query_as("SELECT state,record_revision FROM execution_attempts WHERE attempt_id=$1")
            .bind(receipt.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        original, possible,
        "an uncertain Graph result preserves the snapshot"
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(receipt.attempt_id)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    let done = Box::pin(c.service.complete(&c.actor, op)).await.unwrap();
    assert_eq!(done.state, "confirmed");
    let settled: (Value, i64) =
        sqlx::query_as("SELECT state,record_revision FROM execution_attempts WHERE attempt_id=$1")
            .bind(receipt.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(settled.0["status"], "finished");
    assert_eq!(settled.0["retention"]["kind"], "retained");
    assert_eq!(settled.0["effects"]["kind"], "quiescent");
    assert_eq!(settled.0["stop"]["kind"], "stopped");
    for field in ["offer", "source", "graph"] {
        assert_eq!(settled.0[field], original.0[field]);
    }
    assert_eq!(settled.1, original.1 + 1);
    let compatibility: aq_server::execution::profiled::ProfiledState =
        serde_json::from_value(settled.0.clone()).unwrap();
    compatibility.validate().unwrap();
    // Restart with an empty immutable-facts cache: the compatibility snapshot
    // must still validate, including the persisted deadline column.
    let mut restarted = c.env.f.state.clone();
    restarted.store = c.env.f.db.store().await;
    let mut cold_tx = pool.begin().await.unwrap();
    let cold = aq_server::execution::authority::load_locked(
        &restarted,
        &mut cold_tx,
        &compatibility.offer.binding.session,
        aq_server::execution::authority::Policy::IdentityOnly,
    )
    .await
    .unwrap();
    assert!(cold.release_ready);
    assert!(!cold.holds_task);
    cold_tx.commit().await.unwrap();
    assert!(sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{retention}', '{\"kind\":\"pending\"}') WHERE attempt_id=$1")
        .bind(receipt.attempt_id).execute(&pool).await.is_err(),
        "confirmation only permits the exact verified terminal projection");
    assert_eq!(c.writes.load(Ordering::SeqCst), 1);
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(receipt.attempt_id)
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_task_completions WHERE outbox IS NOT NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        json!(done),
        json!(Box::pin(c.service.complete(&c.actor, op)).await.unwrap())
    );
    let replayed: (Value, i64) =
        sqlx::query_as("SELECT state,record_revision FROM execution_attempts WHERE attempt_id=$1")
            .bind(receipt.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        settled, replayed,
        "confirmed replay does not advance the revision"
    );
    c.env.peer.abort();
    c._graph.abort();
}

async fn wait_for_lock<T: std::fmt::Debug>(
    pool: &sqlx::PgPool,
    blocker: i32,
    query: &str,
    pending: &mut tokio::task::JoinHandle<T>,
) -> i32 {
    // Preliminary owner/Graph/Git reads do not have a 400 ms startup contract.
    // Observe the actual blocking edge, while the operation's own SQL/I/O
    // deadlines bound the wait. Early completion (including a lock timeout)
    // must fail the barrier, never masquerade as an observed authorization wait.
    tokio::select! {
        biased;
        result = pending => panic!(
            "operation completed before its lock wait was observed: blocker={blocker}, query={query}, result={result:?}"
        ),
        pid = async {
            loop {
                let pid: Option<i32> = sqlx::query_scalar(
                    "SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query=$2 AND $1=ANY(pg_blocking_pids(pid))",
                )
                .bind(blocker)
                .bind(query)
                .fetch_optional(pool)
                .await
                .unwrap();
                if let Some(pid) = pid {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        } => pid,
    }
}
#[tokio::test]
async fn settlement39_expiry_after_final_requester_wait_rolls_back_acceptance() {
    for after in [(NOW + 30000) as i64, (NOW - 1) as i64] {
        let c = Box::pin(setup()).await;
        let pool = c.env.f.db.pool().await;
        let mut lock = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
            .bind(c.actor.id)
            .execute(&mut *lock)
            .await
            .unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        let service = c.service.clone();
        let actor = c.actor.clone();
        let request = c.request.clone();
        let mut pending =
            tokio::spawn(async move { Box::pin(service.evaluate(&actor, &request)).await });
        wait_for_lock(&pool, blocker, USER_AUTHORIZATION_LOCK, &mut pending).await;
        c.env.f.time.store(after, Ordering::SeqCst);
        lock.commit().await.unwrap();
        assert!(pending.await.unwrap().is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_settlement_evaluations")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(c.writes.load(Ordering::SeqCst), 0);
        c.env.peer.abort();
        c._graph.abort();
    }
}
#[tokio::test]
async fn settlement39_expiry_after_final_approver_wait_rolls_back_first_dispatch() {
    let c = Box::pin(setup()).await;
    let e = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    let op = e.completion_operation_id.unwrap();
    let pool = c.env.f.db.pool().await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(c.env.f.actor)
        .execute(&mut *lock)
        .await
        .unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let service = c.service.clone();
    let actor = c.actor.clone();
    let mut pending = tokio::spawn(async move { Box::pin(service.complete(&actor, op)).await });
    wait_for_lock(&pool, blocker, USER_AUTHORIZATION_LOCK, &mut pending).await;
    c.env.f.time.store((NOW + 30000) as i64, Ordering::SeqCst);
    lock.commit().await.unwrap();
    assert!(pending.await.unwrap().is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i32>(
            "SELECT dispatch_count FROM profiled_task_completions WHERE operation_id=$1"
        )
        .bind(op)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    c.env.peer.abort();
    c._graph.abort();
}
#[tokio::test]
async fn settlement39_lost_lease_after_held_response_fences_revoked_then_regranted_actor() {
    let c = Box::pin(setup()).await;
    let e = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    let op = e.completion_operation_id.unwrap();
    let pool = c.env.f.db.pool().await;
    c.gate.enabled.store(true, Ordering::SeqCst);
    let service = c.service.clone();
    let actor = c.actor.clone();
    let first = tokio::spawn(async move { Box::pin(service.complete(&actor, op)).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), c.gate.entered.notified())
        .await
        .unwrap();
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(c.actor.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(c.actor.id)
        .execute(&pool)
        .await
        .unwrap();
    c.env.f.time.store((NOW + 16000) as i64, Ordering::SeqCst);
    let second = Box::pin(c.service.complete(&c.actor, op)).await.unwrap();
    assert_eq!(second.state, "confirmed");
    assert_eq!(second.dispatch_count, 2);
    assert!(
        !first.is_finished(),
        "old request must still be held AFTER newer lease confirms"
    );
    c.gate.release.notify_one();
    let old = first.await.unwrap();
    assert!(
        matches!(old, Err(aq_server::error::AppError::Forbidden(_))),
        "old held request must not receive new lease's private receipt: {old:?}"
    );
    assert_eq!(c.writes.load(Ordering::SeqCst), 1);
    c.env.peer.abort();
    c._graph.abort();
}

#[tokio::test]
async fn settlement39_startup_refuses_corrupted_accepted_hot_basis_without_repair() {
    let c = Box::pin(setup()).await;
    let e = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    let pool = c.env.f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE profiled_authority_hot DISABLE TRIGGER profiled_hot_guard")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{effects}','{\"kind\":\"unknown\"}'::JSONB) WHERE attempt_id=$1").bind(e.attempt_id).execute(&mut *tx).await.unwrap();
    sqlx::query("ALTER TABLE profiled_authority_hot ENABLE TRIGGER profiled_hot_guard")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(e.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let reopened = aq_server::store::Store::connect_with(c.env.f.db.options()).await;
    assert!(
        matches!(reopened, Err(aq_server::store::StoreError::Corrupt(_))),
        "startup must reject the changed accepted materialization"
    );
    let after: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(e.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert_eq!(after["effects"]["kind"], "unknown");
    c.env.peer.abort();
    c._graph.abort();
}

#[tokio::test]
async fn settlement39_prepare_held_git_reply_fences_revoked_preparer_before_private_subject_return()
{
    let mut c = Box::pin(setup()).await;
    let pool = c.env.f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    let report = results::load_locked(&c.env.f.state, &mut tx, c.env.report.report_id)
        .await
        .unwrap();
    let source =
        aq_server::source_retention::load_confirmed_locked(&mut tx, c.env.operation, &report)
            .await
            .unwrap();
    tx.rollback().await.unwrap();
    let path = c.env._directory.path().join("held-prepare.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let enter = entered.clone();
    let resume = release.clone();
    let inspection = json!({"version":1,"id":source.manifest.artifact_id,"repository_id":source.receipt.repository_id,"binding_generation":source.receipt.binding_generation,"generation":1,"state":"active","resource":"transfer","release_operation":null,"release_reason":null,"refs":[{"name":format!("refs/aq-git/transfers/{}",source.manifest.artifact_id),"oid":source.receipt.source.hex}]});
    let peer = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio::io::BufReader::new(socket);
        let mut line = String::new();
        socket.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["operation"], "retention.inspect");
        enter.notify_one();
        resume.notified().await;
        let mut reply=serde_json::to_vec(&json!({"protocol_version":1,"request_id":request["request_id"],"ok":true,"result":inspection})).unwrap();
        reply.push(b'\n');
        socket.get_mut().write_all(&reply).await.unwrap();
    });
    c.env.f.state.source_retention = Some(Arc::new(
        aq_server::source_retention::Runtime::new(
            aq_server::source_retention::SourceRetentionConfig {
                keep_per_task: 2,
                max_age: 30 * 86400,
                staging_root: c.env._directory.path().join("held-stage"),
                git_root: c.env._directory.path().join("held-git"),
                git_socket: path,
                max_staging_bytes: 1048576,
                max_bundle_bytes: 1048576,
                bulk_concurrency: 1,
                incomplete_ttl_secs: 60,
            },
        )
        .unwrap(),
    ));
    let service = aq_server::reviews::Service::new(c.env.f.state.clone(), c.env.keys.clone());
    let actor = c.actor.clone();
    let id = c.env.report.report_id;
    let operation = c.env.operation;
    let declaration = external(c.env.report.binding.attempt_id);
    let pending = tokio::spawn(async move {
        Box::pin(service.prepare_combined_as(&actor, id, operation, declaration)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(c.actor.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !pending.is_finished(),
        "valid Git response is still held after role revocation"
    );
    release.notify_one();
    assert!(
        matches!(
            pending.await.unwrap(),
            Err(aq_server::error::AppError::Forbidden(_))
        ),
        "private subject must not escape after the actual I/O wait"
    );
    peer.await.unwrap();
    c.env.peer.abort();
    c._graph.abort();
}

#[tokio::test]
async fn settlement39_valid_pending_combined_review_is_unavailable_without_settlement_writes() {
    let c = Box::pin(setup()).await;
    let pool = c.env.f.db.pool().await;
    let prepared = Box::pin(c.env.service.prepare_combined_as(
        &c.actor,
        c.env.report.report_id,
        c.env.operation,
        external(c.env.report.binding.attempt_id),
    ))
    .await
    .unwrap();
    let question = aq_review_protocol::DecisionRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: Uuid::new_v4(),
        subject: prepared.subject().artifact().unwrap(),
        eligible_owners: vec![c.actor.id],
        eligibility_project_revision: 1,
        review_policy: prepared.subject().code.validation_policy.clone(),
        expires_at_ms: NOW + 30000,
    };
    Box::pin(
        c.env
            .service
            .admit_combined(c.actor.id, &prepared, &question),
    )
    .await
    .unwrap();
    let snapshot = || async {
        sqlx::query_scalar::<_,Value>("SELECT jsonb_build_object('evaluations',(SELECT count(*) FROM profiled_settlement_evaluations),'releases',(SELECT count(*) FROM profiled_verified_releases),'completions',(SELECT count(*) FROM profiled_task_completions),'hot',h.state,'held',a.holds_task) FROM profiled_authority_hot h JOIN execution_attempts a USING(attempt_id) WHERE h.attempt_id=$1").bind(c.env.report.binding.attempt_id).fetch_one(&pool).await.unwrap()
    };
    let before = snapshot().await;
    let mut request = c.request.clone();
    request.operation_id = Uuid::new_v4();
    request.decision_id = question.decision_id;
    request.subject = question.subject;
    let result = Box::pin(c.service.evaluate(&c.actor, &request)).await;
    assert!(
        matches!(
            result,
            Err(aq_server::error::AppError::UpdatesUnavailable(_))
        ),
        "a real pending review must hold with503"
    );
    assert_eq!(snapshot().await, before);
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    c.env.peer.abort();
    c._graph.abort();
}

#[path = "settlement_http.rs"]
mod settlement_http;

#[tokio::test]
async fn overview_settlement_acceptance_does_not_claim_graph_completion() {
    let c = Box::pin(setup()).await;
    let receipt = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    assert_eq!(receipt.disposition, EvaluationDisposition::Accepted);
    let app = aq_server::build_admin_router_with_policy_owners(
        c.env.f.state.clone(),
        None,
        Some(Arc::new(c.env.service.clone())),
    );
    let other = super::review_inbox::overview_as(&app, "/api/v1/overview", "owner@localhost").await;
    assert!(
        !other["data"]["recent_outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["outcome"] == "settlement_accepted")
    );
    let body = super::review_inbox::overview_as(
        &app,
        "/api/v1/overview",
        "settlement-requester@localhost",
    )
    .await;
    let outcomes = body["data"]["recent_outcomes"].as_array().unwrap();
    let outcome = outcomes
        .iter()
        .find(|v| v["outcome"] == "settlement_accepted")
        .unwrap_or_else(|| panic!("accepted evaluation is observed: {body}"));
    assert_eq!(outcome["subject"]["type"], "settlement_evaluation");
    assert_eq!(outcome["subject"]["id"], json!(receipt.operation_id));
    let digest: String = sqlx::query_scalar(
        "SELECT receipt_digest FROM profiled_settlement_evaluations WHERE operation_id=$1",
    )
    .bind(receipt.operation_id)
    .fetch_one(&c.env.f.db.pool().await)
    .await
    .unwrap();
    assert_eq!(outcome["subject"]["revision"], format!("sha256:{digest}"));
    assert_eq!(outcome["at"], "1970-01-01T00:16:40.000Z");
    assert!(!outcomes.iter().any(|v| v["outcome"] == "completed"));
    assert_eq!(
        c.writes.load(Ordering::SeqCst),
        0,
        "overview must not dispatch Graph completion"
    );
    c.env.peer.abort();
    c._graph.abort();
}
