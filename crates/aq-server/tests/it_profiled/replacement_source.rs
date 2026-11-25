//! Synthetic SQL/Git-owner component coverage; no provider or actual Git export.
use crate::common;
use aq_admission::{Label, SourceSelection};
use aq_server::{
    execution::{
        authority::hot::{HotState, LaunchOwner},
        profiled::{Effects, Retention, Status, Stop},
    },
    profiled_execution::{
        replacement::{self, Replacement},
        source,
        source_exports::{self, Reservation},
    },
    project_bindings::domain::FrozenProjectBinding,
};
use common::{
    profiled::{self, NOW},
    source_exports::{setup, synthetic_fact},
};
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

#[tokio::test]
async fn successor_reuses_predecessor_export_only_with_exact_release_receipt() {
    let (mut f, request, _dir) = setup().await;
    let pool = f.db.pool().await;
    let Reservation::Pending(token) = source_exports::reserve(&f.state, request.clone())
        .await
        .unwrap()
    else {
        panic!()
    };
    let fact = synthetic_fact(&token);
    source_exports::finalize(&f.state, &token, &fact)
        .await
        .unwrap();
    let original = fact.canonical_bytes().unwrap();
    let retention = serde_json::from_value(json!({"request_digest":token.request_digest(),"export_operation_id":request.operation_id,"inspection":serde_json::from_slice::<Value>(&original).unwrap()["retention_inspection"],"observed_at_ms":NOW,"expires_at_ms":NOW+30000})).unwrap();
    let first = link(
        &f,
        &pool,
        source_exports::LinkEvidence::new(&token, &fact, &retention),
    )
    .await;
    let predecessor = f.profiled.offer.binding.attempt_id;
    check_predecessor_revision(&pool, predecessor, request.project_revision).await;
    finish_predecessor(&f, &pool, predecessor).await;
    let retained = release_predecessor(&f, &pool, predecessor).await;
    assert_eq!(retained.token().request_digest(), token.request_digest());
    assert_eq!(retained.fact().canonical_bytes().unwrap(), original);

    // Publish a synthetic successor with a new attempt, session and generation.
    let mut spec: aq_admission::execution_spec::ProfiledExecutionSpec = serde_json::from_value(
        sqlx::query_scalar::<_, Value>(
            "SELECT execution_spec FROM profiled_launches WHERE attempt_id=$1",
        )
        .bind(predecessor)
        .fetch_one(&pool)
        .await
        .unwrap(),
    )
    .unwrap();
    let attempt = insert_successor(&mut f, &mut spec, 2).await;
    let evidence =
        || source_exports::LinkEvidence::new(retained.token(), retained.fact(), &retention);
    let second = link(&f, &pool, evidence()).await;
    assert_ne!(first.attempt, second.attempt);
    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(first.bundle_digest, second.bundle_digest);
    assert_eq!(first.manifest_digest, second.manifest_digest);
    assert_eq!(first.source, second.source);
    check_shared_source(&pool, &request, (predecessor, attempt), &original).await;
    close_never_granted(&f, &pool, attempt).await;
    insert_successor(&mut f, &mut spec, 3).await;
    let linked = link(&f, &pool, evidence()).await;
    assert_eq!(linked.artifact_id, first.artifact_id);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_launch_source_exports WHERE operation_id=$1"
        )
        .bind(request.operation_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        3
    );
}

#[tokio::test]
async fn replacement_observes_retained_commit_without_resolving_moved_configured_ref() {
    let (mut f, request, dir) = setup().await;
    let mut config = (*f.state.config).clone();
    let mut native = (**config.profiled_execution.as_ref().unwrap()).clone();
    native.projects[0].source = SourceSelection::NamedRef {
        reference: Label::new("refs/heads/moved").unwrap(),
    };
    native.projects[0].retention_handle = Uuid::new_v4();
    let policy = native.project_policy_record(request.project_id).unwrap();
    config.profiled_execution = Some(Arc::new(native));
    f.state.config = Arc::new(config);
    let pool = f.db.pool().await;
    let row = sqlx::query("SELECT revision,binding,binding_observation FROM project_execution_bindings WHERE project_id=$1").bind(request.project_id).fetch_one(&pool).await.unwrap();
    let project = FrozenProjectBinding {
        project: serde_json::from_value(json!({"revision":row.get::<i64,_>("revision"),"binding":row.get::<Value,_>("binding"),"validated_at_ms":NOW,"validation":row.get::<Value,_>("binding_observation")})).unwrap(),
        snapshot_digest: "synthetic".into(), worker_membership_digest: "synthetic".into(),
        binding_observation: serde_json::from_value(row.get("binding_observation")).unwrap(), node_id: f.node.node_id,
        environment: "synthetic".into(), role: "synthetic".into(), intelligence_profile: "synthetic".into(), execution_profile: "synthetic".into(),
    };
    let socket = &f.state.source_retention.as_ref().unwrap().config.git_socket;
    assert!(socket.starts_with(dir.path()));
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let peer = tokio::spawn(serve_retained_head(listener, request.clone()));
    let observed = source::observe_retained(&f.state, &project, &policy, &request, None)
        .await
        .unwrap();
    peer.await.unwrap();
    assert_eq!(observed.admission.resolved_object.hex, request.base.hex);
    assert!(matches!(
        observed.admission.selection,
        SourceSelection::Immutable { .. }
    ));
    assert_eq!(observed.retention_handle, request.operation_id);
    source::validate_body(&observed.bytes, request.project_id, NOW).unwrap();
    for field in ["project", "revision", "repository", "generation"] {
        let mut wrong = request.clone();
        match field {
            "project" => wrong.project_id = Uuid::new_v4(),
            "revision" => wrong.project_revision += 1,
            "repository" => wrong.repository_id = Uuid::new_v4(),
            _ => wrong.binding_generation += 1,
        }
        assert!(
            source::observe_retained(&f.state, &project, &policy, &wrong, None)
                .await
                .is_err()
        );
    }
}

async fn link(
    f: &profiled::Fixture,
    pool: &sqlx::PgPool,
    evidence: source_exports::LinkEvidence<'_>,
) -> aq_worker_protocol::TransferManifest {
    let mut tx = pool.begin().await.unwrap();
    let first = source_exports::link_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        evidence,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    first
}
async fn check_predecessor_revision(pool: &sqlx::PgPool, predecessor: Uuid, revision: u64) {
    // The successor's revision check reads the revision this export pins.
    assert_eq!(
        replacement::predecessor_project_revision(pool, predecessor)
            .await
            .unwrap(),
        Some(revision)
    );
    assert_eq!(
        replacement::predecessor_project_revision(pool, Uuid::new_v4())
            .await
            .unwrap(),
        None
    );
}
async fn finish_predecessor(f: &profiled::Fixture, pool: &sqlx::PgPool, predecessor: Uuid) {
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(predecessor).bind(json!(hot)).execute(pool).await.unwrap();
    hot.status = Status::Finished;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    hot.stop = Stop::Stopped {
        receipt: profiled::artifact("stop", b"stop"),
    };
    hot.retention = Retention::Retained {
        receipt: profiled::artifact("retained", b"retained"),
    };
    hot.effects = Effects::Quiescent {
        receipt: profiled::artifact("effects", b"effects"),
    };
    hot.validate_against(&f.profiled.offer.binding).unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(predecessor)
        .bind(json!(hot))
        .execute(pool)
        .await
        .unwrap();
}
async fn release_predecessor(
    f: &profiled::Fixture,
    pool: &sqlx::PgPool,
    predecessor: Uuid,
) -> source_exports::LinkedExport {
    let replacement = Replacement {
        predecessor,
        attempts_started: 1,
        exempt_attempts: 0,
        elapsed_ms: 1,
        stop_receipt: profiled::artifact("stop", b"stop"),
        retained_source_receipt: profiled::artifact("retained", b"retained"),
        effect_reconciliation_receipt: profiled::artifact("effects", b"effects"),
        needs_release: false,
    };
    let task = f.selection.lineage.task_id;
    assert!(
        replacement::retained_source(&f.state, task, &replacement)
            .await
            .is_err()
    );
    // Exercise the real release INSERT guard with explicit synthetic owner facts.
    sqlx::query("INSERT INTO profiled_replacement_releases(attempt_id,task_id,reason,stop_receipt_digest,retained_source_receipt_digest,effect_reconciliation_receipt_digest,attempts_started,recorded_at_ms) VALUES($1,$2,'failed_attempt_replacement',$3,$4,$5,1,$6)")
        .bind(predecessor).bind(task).bind(replacement.stop_receipt.digest.as_str()).bind(replacement.retained_source_receipt.digest.as_str()).bind(replacement.effect_reconciliation_receipt.digest.as_str()).bind(NOW as i64).execute(pool).await.unwrap();
    sqlx::query("UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1")
        .bind(predecessor)
        .execute(pool)
        .await
        .unwrap();
    let mut wrong = replacement.clone();
    wrong.retained_source_receipt = profiled::artifact("wrong", b"wrong");
    assert!(
        replacement::retained_source(&f.state, task, &wrong)
            .await
            .is_err()
    );
    assert!(
        replacement::retained_source(&f.state, Uuid::new_v4(), &replacement)
            .await
            .is_err()
    );
    replacement::retained_source(&f.state, task, &replacement)
        .await
        .unwrap()
}
async fn insert_successor(
    f: &mut profiled::Fixture,
    spec: &mut aq_admission::execution_spec::ProfiledExecutionSpec,
    generation: u64,
) -> Uuid {
    let attempt = Uuid::new_v4();
    let session = aq_session_protocol::SessionIdentity::Remote {
        node_id: f.node.node_id,
        boot_id: f.node.boot_id,
        attempt_id: attempt,
        assignment_generation: generation,
        session_id: Uuid::new_v4(),
    };
    let offer = &mut f.profiled.offer;
    offer.binding.session = session.clone();
    offer.binding.attempt_id = attempt;
    offer.binding.assignment_generation = generation;
    offer.binding.offer_id = Uuid::new_v4();
    offer.manifest.bindings.session = session;
    offer.manifest.bindings.attempt_id = attempt;
    offer.manifest.bindings.assignment_generation = generation;
    offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&offer.manifest).unwrap();
    spec.lifecycle = aq_admission::transport::lifecycle_binding(offer).unwrap();
    offer.binding.execution_spec_digest = spec.artifact().unwrap().digest;
    f.decision = Uuid::new_v4();
    profiled::insert_typed(f, spec).await;
    attempt
}
async fn check_shared_source(
    pool: &sqlx::PgPool,
    request: &source_exports::Request,
    (predecessor, attempt): (Uuid, Uuid),
    original: &[u8],
) {
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_source_exports")
            .fetch_one(pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_launch_source_exports WHERE operation_id=$1"
        )
        .bind(request.operation_id)
        .fetch_one(pool)
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT sum(reserved_bytes)::BIGINT FROM source_bases")
            .fetch_one(pool)
            .await
            .unwrap(),
        0
    );
    let mut tx = pool.begin().await.unwrap();
    let linked = source_exports::load_linked_locked(&mut tx, attempt)
        .await
        .unwrap();
    assert_eq!(linked.fact().canonical_bytes().unwrap(), original);
    tx.commit().await.unwrap();
    let error = sqlx::query(
        "INSERT INTO profiled_launch_source_exports(attempt_id,operation_id) VALUES($1,$2)",
    )
    .bind(predecessor)
    .bind(request.operation_id)
    .execute(pool)
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("shared source requires released predecessor")
    );
}
async fn close_never_granted(f: &profiled::Fixture, pool: &sqlx::PgPool, attempt: Uuid) {
    // 0095: a successor that was offered but never granted (valid never-granted
    // closure, no claim) no longer blocks the next successor of the same task.
    // Synthetic owner rows written with guards bypassed, as a closure would.
    let never = attempt;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    let launch: Value =
        sqlx::query_scalar("SELECT launch_binding FROM profiled_launches WHERE attempt_id=$1")
            .bind(never)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    let mut closed = HotState::from_compat(&f.profiled).unwrap();
    closed.status = Status::NeverGranted;
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0) ON CONFLICT(attempt_id) DO UPDATE SET state=EXCLUDED.state")
        .bind(never).bind(json!(closed)).execute(&mut *tx).await.unwrap();
    let proof =
        serde_json::to_vec(&json!({"binding":launch,"synthetic":"never-granted proof"})).unwrap();
    sqlx::query("INSERT INTO profiled_never_granted_closures(attempt_id,closure_id,operation_id,binding,observed_record_revision,closed_at_ms,reason,proof_bytes,proof_digest) VALUES($1,$2,$3,$4,1,$5,'offer_expired',$6,encode(sha256($6),'hex'))")
        .bind(never).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(&launch).bind(NOW as i64).bind(&proof).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1")
        .bind(never)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT profiled_never_granted_valid($1)")
            .bind(never)
            .fetch_one(pool)
            .await
            .unwrap()
    );
}
async fn serve_retained_head(
    listener: tokio::net::UnixListener,
    retained_request: source_exports::Request,
) {
    for operation in ["inspect", "retention.inspect"] {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let q: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            q["operation"], operation,
            "no repo.observation or artifact.export call"
        );
        let result = inspection(operation, &retained_request, &q);
        let mut reply = serde_json::to_vec(
            &json!({"protocol_version":1,"request_id":q["request_id"],"ok":true,"result":result}),
        )
        .unwrap();
        reply.push(b'\n');
        reader.get_mut().write_all(&reply).await.unwrap();
    }
}
fn inspection(operation: &str, r: &source_exports::Request, q: &Value) -> Value {
    if operation == "inspect" {
        assert_eq!(q["params"]["oid"]["hex"], r.base.hex);
        json!({"oid":r.base,"tree":{"algorithm":"sha1","hex":"b".repeat(40)}})
    } else {
        assert_eq!(q["params"]["handle_id"], r.operation_id.to_string());
        json!({"version":1,"id":r.operation_id,"repository_id":r.repository_id,"binding_generation":r.binding_generation,"generation":1,"state":"active","refs":[{"name":format!("refs/aq-git/transfers/{}",r.operation_id),"oid":r.base.hex}],"release_operation":null,"release_reason":null})
    }
}
