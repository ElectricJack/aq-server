//! Synthetic SQL/Git-owner component coverage; no provider or actual Git export.
mod common;
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
    let mut tx = pool.begin().await.unwrap();
    let first = source_exports::link_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        &token,
        &fact,
        &retention,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let predecessor = f.profiled.offer.binding.attempt_id;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(predecessor).bind(json!(hot)).execute(&pool).await.unwrap();
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
        .execute(&pool)
        .await
        .unwrap();
    let replacement = Replacement {
        predecessor,
        attempts_started: 1,
        elapsed_ms: 1,
        sandbox_ms: 0,
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
        .bind(predecessor).bind(task).bind(replacement.stop_receipt.digest.as_str()).bind(replacement.retained_source_receipt.digest.as_str()).bind(replacement.effect_reconciliation_receipt.digest.as_str()).bind(NOW as i64).execute(&pool).await.unwrap();
    sqlx::query("UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1")
        .bind(predecessor)
        .execute(&pool)
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
    let retained = replacement::retained_source(&f.state, task, &replacement)
        .await
        .unwrap();
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
    let attempt = Uuid::new_v4();
    let session = aq_session_protocol::SessionIdentity::Remote {
        node_id: f.node.node_id,
        boot_id: f.node.boot_id,
        attempt_id: attempt,
        assignment_generation: 2,
        session_id: Uuid::new_v4(),
    };
    let offer = &mut f.profiled.offer;
    offer.binding.session = session.clone();
    offer.binding.attempt_id = attempt;
    offer.binding.assignment_generation = 2;
    offer.binding.offer_id = Uuid::new_v4();
    offer.manifest.bindings.session = session;
    offer.manifest.bindings.attempt_id = attempt;
    offer.manifest.bindings.assignment_generation = 2;
    offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&offer.manifest).unwrap();
    spec.lifecycle = aq_admission::transport::lifecycle_binding(offer).unwrap();
    offer.binding.execution_spec_digest = spec.artifact().unwrap().digest;
    f.decision = Uuid::new_v4();
    profiled::insert_typed(&f, &spec).await;
    let mut tx = pool.begin().await.unwrap();
    let second = source_exports::link_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        retained.token(),
        retained.fact(),
        &retention,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_ne!(first.attempt, second.attempt);
    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(first.bundle_digest, second.bundle_digest);
    assert_eq!(first.manifest_digest, second.manifest_digest);
    assert_eq!(first.source, second.source);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_source_exports")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_launch_source_exports WHERE operation_id=$1"
        )
        .bind(request.operation_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT sum(reserved_bytes)::BIGINT FROM source_bases")
            .fetch_one(&pool)
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
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("shared source requires released predecessor")
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
        catalog_activation_revision: 1, worker_membership_digest: "synthetic".into(),
        binding_observation: serde_json::from_value(row.get("binding_observation")).unwrap(), node_id: f.node.node_id,
        environment: "synthetic".into(), agent_profile: "synthetic".into(), intelligence_profile: "synthetic".into(), execution_profile: "synthetic".into(),
    };
    let socket = &f.state.source_retention.as_ref().unwrap().config.git_socket;
    assert!(socket.starts_with(dir.path()));
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let retained_request = request.clone();
    let peer = tokio::spawn(async move {
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
            let r = &retained_request;
            let result = if operation == "inspect" {
                assert_eq!(q["params"]["oid"]["hex"], r.base.hex);
                json!({"oid":r.base,"tree":{"algorithm":"sha1","hex":"b".repeat(40)}})
            } else {
                assert_eq!(q["params"]["handle_id"], r.operation_id.to_string());
                json!({"version":1,"id":r.operation_id,"repository_id":r.repository_id,"binding_generation":r.binding_generation,"generation":1,"state":"active","refs":[{"name":format!("refs/aq-git/transfers/{}",r.operation_id),"oid":r.base.hex}],"release_operation":null,"release_reason":null})
            };
            let mut reply = serde_json::to_vec(&json!({"protocol_version":1,"request_id":q["request_id"],"ok":true,"result":result})).unwrap();
            reply.push(b'\n');
            reader.get_mut().write_all(&reply).await.unwrap();
        }
    });
    let observed = source::observe_retained(&f.state, &project, &policy, &request)
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
            source::observe_retained(&f.state, &project, &policy, &wrong)
                .await
                .is_err()
        );
    }
}
