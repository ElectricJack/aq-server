//! `attempt.settle_closed_launch` (`aq_machine_control::closed_launch`): an owner's attested
//! settlement of a launch closed with a pane (the gate failed before the agent was handed
//! over), the case the automatic owner holds forever. Every refusal changes nothing.
use super::*;
use aq_admission::granted_closure::{DriverIntent, HostStoppedProof, StoppedObservation};
use aq_server::profiled_execution::{
    closure::settlement::{self, ClosureSettlement},
    expiry::ClosureSweep,
    registration, replacement,
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

/// A host stopped closure whose pane ran and was reaped: it does not prove nonstart.
async fn paned(c: &Prepared) -> Request {
    let mut r = request(c).await;
    let running = started(&r, c.request.journal_revision).observation;
    let intent = DriverIntent {
        binding: r.binding.clone(),
        grant_id: r.grant_id,
        guardian_id: r.guardian_id,
        reservation_id: r.reservation_id,
        host: running.guardian.intent.clone(),
        journal: JournalCheckpoint {
            revision: c.request.journal_revision + 1,
            digest: Digest::of(b"synthetic persisted prehook launch intent"),
        },
    };
    let proof = HostStoppedProof {
        observation: StoppedObservation {
            guardian: running.guardian,
            phase: aq_admission::stopped::StoppedPhase::Stopped,
            pane: Some(running.pane),
            permit_sequence: 1,
            permit_deadline_boot_ms: 100,
            observed_boot_ms: 101,
            reaped_count: 3,
            kernel_wait_echild: true,
            root_status: Some(0),
            stop_reason: None,
        },
    };
    r.journal.revision = c.request.journal_revision + 2;
    r.evidence = Evidence::HostStopped {
        driver_intent: Box::new(intent),
        proof_digest: proof.digest().unwrap(),
        proof,
    };
    r.validate().unwrap();
    assert!(!r.evidence.proves_never_started());
    r
}
async fn close(c: &Prepared, r: &Request) {
    let p = policy(c, r, 1).await;
    let (status, body) = post(c, p, r).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
}
/// Moves the server clock to the instant the grant expires.
async fn expire(c: &Prepared) {
    let at: String = scalar(
        c,
        "SELECT state#>>'{grant,expires_at_ms}' FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .await;
    c.clock.store(at.parse().unwrap(), Ordering::SeqCst);
}
/// The node registers again under a new boot after the closure (as the live nodes did).
async fn restart(c: &Prepared) {
    c.clock.fetch_add(1, Ordering::SeqCst);
    let row: (Value,) = sqlx::query_as("SELECT convert_from(metadata_bytes,'UTF8')::JSONB FROM profiled_native_registration_history h JOIN profiled_launch_registrations l USING(registration_id) WHERE l.attempt_id=$1")
        .bind(c.request.binding.attempt_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    let metadata: registration::NativeRegistration = serde_json::from_value(row.0).unwrap();
    let mut changed = metadata.request;
    changed.operation_id = Uuid::new_v4();
    changed.identity.boot_id = Uuid::new_v4();
    let auth = closure::auth(c).await;
    registration::register_slots(&c.case.env.env.state, &auth, &changed, Some(1))
        .await
        .unwrap();
}
async fn scalar<T>(c: &Prepared, sql: &str) -> T
where
    T: Send + Unpin + for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    sqlx::query_scalar(sql)
        .bind(c.request.binding.attempt_id)
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap()
}
/// The attempt's facts a refusal must leave unchanged.
async fn facts(c: &Prepared) -> Value {
    scalar(c, "SELECT jsonb_build_object('held',a.holds_task,'hot',h.state,\
        'attested',(SELECT count(*) FROM closed_launch_attestations t WHERE t.attempt_id=a.attempt_id),\
        'released',(SELECT count(*) FROM profiled_replacement_releases r WHERE r.attempt_id=a.attempt_id),\
        'disposed',(SELECT count(*) FROM profiled_stopped_dispositions d WHERE d.task_id=a.task_id)) \
        FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1").await
}
/// `attempt.settle_closed_launch` through the owner's command surface.
async fn command(c: &Prepared, project: Uuid, operation: Uuid) -> (u16, Value) {
    let params = json!({"project_id": project, "attempt_id": c.request.binding.attempt_id,
        "operation_id": operation, "reason": "the gate failed before handover; host processes reaped"});
    let response = aq_server::build_admin_router(c.case.env.env.state.clone())
        .oneshot(
            axum::http::Request::post("/api/v1/commands/attempt.settle_closed_launch")
                .header("content-type", "application/json")
                .header("x-aq-dev-email", "owner@localhost")
                .header("X-AQ-Request", "1")
                .body(axum::body::Body::from(
                    json!({"params": params}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
fn project(c: &Prepared) -> Uuid {
    c.case.env.lineage.project_id
}
/// Refused with `code` and nothing changed.
async fn refused(c: &Prepared, code: &str) {
    let before = facts(c).await;
    let (status, body) = command(c, project(c), Uuid::new_v4()).await;
    assert_eq!(status, 409, "{body}");
    assert!(body.to_string().contains(code), "{code}: {body}");
    assert_eq!(facts(c).await, before);
    assert_eq!(before["attested"], 0);
    assert_eq!(before["hot"]["stop"]["kind"], "unknown");
}
/// A paned closure whose grant expired and whose node restarted.
async fn ready() -> (Prepared, Request) {
    let c = closure::setup().await;
    let r = paned(&c).await;
    close(&c, &r).await;
    expire(&c).await;
    restart(&c).await;
    (c, r)
}

#[tokio::test]
async fn the_automatic_owner_still_holds_a_paned_closure() {
    let (c, _) = ready().await;
    let state = &c.case.env.env.state;
    let attempt = c.request.binding.attempt_id;
    let before = facts(&c).await;
    let progress = ClosureSweep::default().tick(state).await.unwrap();
    assert_eq!((progress.inspected, progress.closed), (1, 0));
    assert_eq!(
        settlement::settle_and_release(state, attempt, None)
            .await
            .unwrap(),
        ClosureSettlement::Held("closure_does_not_prove_nonstart")
    );
    assert_eq!(facts(&c).await, before);
    assert_eq!(before["held"], true);
}

#[tokio::test]
async fn an_attested_closed_launch_releases_its_claim_for_one_ordinary_successor() {
    let (c, _) = ready().await;
    let (state, pool) = (&c.case.env.env.state, &c.case.env.env.pool);
    let attempt = c.request.binding.attempt_id;
    let task: Uuid = scalar(
        &c,
        "SELECT task_id FROM execution_attempts WHERE attempt_id=$1",
    )
    .await;
    // The retained branch head: the task ref at the attempt's lease (delivery builds only).
    #[cfg(feature = "delivery_playbook")]
    let head = retained_head(&c, task).await;
    let operation = Uuid::new_v4();
    let (status, body) = command(&c, project(&c), operation).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["settlement"], "released", "{body}");
    let attestation = &body["attestation"];
    assert_eq!(attestation["operation_id"], operation.to_string());
    assert_eq!(attestation["actor"], "owner@localhost");
    assert_eq!(attestation["task_id"], task.to_string());
    assert!(
        attestation["evidence"]["restart_boot_id"].is_string(),
        "{body}"
    );
    let after = facts(&c).await;
    assert_eq!(after["held"], false, "the claim is released");
    assert_eq!(after["released"], 1, "through the ordinary B4 release");
    assert_eq!(after["disposed"], 0, "never a terminal disposition");
    assert_eq!(after["hot"]["stop"]["kind"], "not_started");
    let closure: String = scalar(
        &c,
        "SELECT receipt_digest FROM profiled_launch_closures WHERE attempt_id=$1",
    )
    .await;
    assert_eq!(after["hot"]["stop"]["receipt"]["digest"], closure);
    assert_eq!(after["hot"]["effects"]["kind"], "quiescent");
    // The task is replaced by the ordinary rules: one successor, its predecessor released.
    let now = (state.clock)().timestamp_millis() as u64;
    let assessed = replacement::assess_task(state, task, now).await.unwrap();
    let replace = assessed.replacement().expect("{assessed:?}");
    assert_eq!(replace.predecessor, attempt);
    assert!(!replace.needs_release);
    // The successor's git grant leases the task ref: it starts from the retained head.
    #[cfg(feature = "delivery_playbook")]
    assert_eq!(successor_lease(&c, task).await, Some(head));
    // Idempotent: the same operation returns the same attestation and changes nothing.
    let (status, replay) = command(&c, project(&c), operation).await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(&replay["attestation"], attestation);
    assert_eq!(facts(&c).await, after);
    let other = command(&c, project(&c), Uuid::new_v4()).await;
    assert_eq!(other.0, 409, "{}", other.1);
    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_replacement_releases")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(releases, 1);
}

#[tokio::test]
async fn another_projects_attempt_is_refused() {
    let (c, _) = ready().await;
    let before = facts(&c).await;
    let (status, body) = command(&c, Uuid::new_v4(), Uuid::new_v4()).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.to_string().contains("attempt_not_in_project"),
        "{body}"
    );
    assert_eq!(facts(&c).await, before);
}

#[tokio::test]
async fn condition_1_a_closure_without_a_host_stopped_proof_is_refused() {
    let c = closure::setup().await;
    let r = request(&c).await; // a durable suppression, never a host's stopped proof
    close(&c, &r).await;
    expire(&c).await;
    restart(&c).await;
    refused(&c, "closure_not_host_stopped").await;
}

#[tokio::test]
async fn condition_2_an_unexpired_grant_is_refused() {
    let c = closure::setup().await;
    let r = paned(&c).await;
    close(&c, &r).await;
    restart(&c).await;
    refused(&c, "closure_grant_not_expired").await;
}

#[tokio::test]
async fn condition_3_a_node_that_did_not_restart_is_refused() {
    let c = closure::setup().await;
    let r = paned(&c).await;
    close(&c, &r).await;
    expire(&c).await;
    refused(&c, "closure_node_not_restarted").await;
}

#[tokio::test]
async fn condition_4_execution_history_is_refused() {
    let (c, _) = ready().await;
    let node: Uuid = scalar(
        &c,
        "SELECT node_id FROM execution_attempts WHERE attempt_id=$1",
    )
    .await;
    sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,artifact_id,import_operation_id,manifest) VALUES($1,$2,$3,$1,$4,$5,'{}')")
        .bind(Uuid::new_v4()).bind(c.request.binding.attempt_id).bind(node)
        .bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&c.case.env.env.pool).await.unwrap();
    refused(&c, "closure_has_execution_history").await;
}

#[tokio::test]
async fn condition_5_a_task_ref_not_at_the_lease_is_refused() {
    let (c, _) = ready().await;
    // The lease names a head that central aq-git does not show for the task ref.
    sqlx::query("INSERT INTO delivery_git_grants(attempt_id,repository_id,base,task_ref,lease) SELECT a.attempt_id,(l.execution_spec#>>'{source,repository_id}')::UUID,repeat('a',40),'refs/heads/aq/'||a.task_id,repeat('c',40) FROM execution_attempts a JOIN profiled_launches l USING(attempt_id) WHERE a.attempt_id=$1")
        .bind(c.request.binding.attempt_id)
        .execute(&c.case.env.env.pool).await.unwrap();
    refused(&c, "closure_task_ref_moved").await;
}

/// The attempt's git grant leases the task ref at a commit central aq-git holds (the fixture's
/// aq-git serves `ref.show` from `git-http-repositories/<repository>`).
#[cfg(feature = "delivery_playbook")]
async fn retained_head(c: &Prepared, task: Uuid) -> String {
    use aq_delivery::git_grant::fake::git;
    let retention = c.case.env.env.state.source_retention.as_ref().unwrap();
    let repository: Uuid = scalar(c, "SELECT (execution_spec#>>'{source,repository_id}')::UUID FROM profiled_launches WHERE attempt_id=$1").await;
    let bare = retention
        .config
        .git_root
        .join("git-http-repositories")
        .join(repository.to_string());
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare"]);
    let tree = git(&bare, &["mktree"]);
    let head = git(
        &bare,
        &["commit-tree", &tree, "-m", "the attempt's retained work"],
    );
    git(
        &bare,
        &["update-ref", &format!("refs/heads/aq/{task}"), &head],
    );
    sqlx::query("INSERT INTO delivery_git_grants(attempt_id,repository_id,base,task_ref,lease) VALUES($1,$2,repeat('a',40),$3,$4)")
        .bind(c.request.binding.attempt_id).bind(repository).bind(format!("refs/heads/aq/{task}")).bind(&head)
        .execute(&c.case.env.env.pool).await.unwrap();
    head
}
/// The lease the successor's offer gets (`producer::build` calls this grant owner).
#[cfg(feature = "delivery_playbook")]
async fn successor_lease(c: &Prepared, task: Uuid) -> Option<String> {
    let retention = c.case.env.env.state.source_retention.as_ref().unwrap();
    let git = aq_delivery::commands::SocketGit::new(&retention.config.git_socket);
    let source: Value = scalar(
        c,
        "SELECT execution_spec->'source' FROM profiled_launches WHERE attempt_id=$1",
    )
    .await;
    let source: aq_admission::SourceAdmission = serde_json::from_value(source).unwrap();
    let pool = &c.case.env.env.pool;
    let grant = aq_delivery::git_grant::offer(pool, &git, &source, task)
        .await
        .unwrap();
    assert_eq!(
        grant.base, source.resolved_object.hex,
        "the base stays the export's commit"
    );
    grant.lease
}
