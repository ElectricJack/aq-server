//! Actual inline retention ownership; no source bundles or provider secrets.
use crate::common;
use aq_admission::{ArtifactKind, Digest};
use aq_server::profiled_execution::artifacts::{self, OwnerKind, Retain};
use common::profiled::{NOW, artifact, fixture, insert};
use serde_json::json;
use uuid::Uuid;
#[tokio::test]
async fn immutable_definition_identity_conflict_is_named_and_preserves_retained_bytes() {
    let f = fixture().await;
    let project = f.selection.lineage.project_id;
    let original = artifact("immutable-definition", b"original definition");
    let retained = artifacts::retain(
        &f.state,
        Retain {
            artifact_id: Uuid::new_v4(),
            project_id: project,
            owner: OwnerKind::Catalog,
            artifact: original.clone(),
            kind: ArtifactKind::Definition,
            bytes: b"original definition",
            retain_until_ms: NOW + 60_000,
        },
    )
    .await
    .unwrap();
    let mut changed = original.clone();
    changed.digest = Digest::of(b"changed definition");
    let error = artifacts::retain(
        &f.state,
        Retain {
            artifact_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            owner: OwnerKind::Startup,
            artifact: changed,
            kind: ArtifactKind::Definition,
            bytes: b"changed definition",
            retain_until_ms: NOW + 60_000,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        error.to_api_error().error,
        "admission_definition_identity_conflict"
    );
    // Identical bytes under a new retention operation still work.
    let replay = artifacts::retain(
        &f.state,
        Retain {
            artifact_id: Uuid::new_v4(),
            project_id: project,
            owner: OwnerKind::Catalog,
            artifact: original,
            kind: ArtifactKind::Definition,
            bytes: b"original definition",
            retain_until_ms: NOW + 60_000,
        },
    )
    .await
    .unwrap();
    assert_eq!(retained.artifact, replay.artifact);
}
#[tokio::test]
async fn retained_bytes_have_real_replayable_immutable_receipt_and_exact_project_links() {
    let f = fixture().await;
    let attempt = insert(&f).await;
    let project = f.selection.lineage.project_id;
    let id = Uuid::new_v4();
    let a = artifact("synthetic-context", b"context bytes");
    let input = || Retain {
        artifact_id: id,
        project_id: project,
        owner: OwnerKind::GraphSnapshot,
        artifact: a.clone(),
        kind: ArtifactKind::Context,
        bytes: b"context bytes",
        retain_until_ms: NOW + 60_000,
    };
    let first = artifacts::retain(&f.state, input()).await.unwrap();
    let replay = artifacts::retain(&f.state, input()).await.unwrap();
    assert_eq!(json!(first), json!(replay));
    let mut changed = input();
    changed.retain_until_ms += 1;
    assert!(artifacts::retain(&f.state, changed).await.is_err());
    let mut changed = input();
    changed.project_id = Uuid::new_v4();
    assert!(artifacts::retain(&f.state, changed).await.is_err());
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    let item = artifacts::load_locked(&mut tx, id).await.unwrap();
    assert_eq!(item.bytes, b"context bytes");
    assert_eq!(Digest::of(&item.receipt), first.retention_receipt.digest);
    let proof: artifacts::Receipt = serde_json::from_slice(&item.receipt).unwrap();
    assert_eq!(proof.artifact_id, id);
    assert_eq!(proof.project_id, project);
    assert_eq!(
        artifacts::link_locked(&mut tx, attempt, project, &[id], NOW, NOW + 30_000)
            .await
            .unwrap()
            .len(),
        1
    );
    tx.commit().await.unwrap();
    for sql in [
        "UPDATE admission_artifacts SET body='different' WHERE artifact_id=$1",
        "DELETE FROM admission_artifacts WHERE artifact_id=$1",
        "DELETE FROM admission_inline_receipts WHERE artifact_id=$1",
        "DELETE FROM profiled_launch_artifacts WHERE artifact_id=$1",
    ] {
        assert!(sqlx::query(sql).bind(id).execute(&pool).await.is_err());
    }
    let mut tx = pool.begin().await.unwrap();
    assert!(
        artifacts::link_locked(&mut tx, attempt, Uuid::new_v4(), &[id], NOW, NOW + 30_000)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        artifacts::link_locked(&mut tx, attempt, project, &[id], NOW, NOW + 60_001)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
}
#[tokio::test]
async fn missing_or_forged_receipt_cannot_commit_and_source_bundle_cannot_use_inline_owner() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let id = Uuid::new_v4();
    let a = artifact("synthetic-context", b"body");
    let project = f.selection.lineage.project_id;
    let receipt = artifact("aq.inline-retention/forged", b"invented");
    let descriptor = json!({"artifact":a,"kind":"context","bytes":4,"retention_receipt":receipt,"retained_at_ms":NOW,"retain_until_ms":NOW+1000});
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO admission_artifacts(artifact_id,descriptor,content_digest,byte_length,owner_kind,owner_key,body,created_at_ms,retain_until_ms) VALUES($1,$2,$3,4,'graph_snapshot',$4,$5,$6,$7)").bind(id).bind(descriptor).bind(a.digest.as_str()).bind(project.to_string()).bind(b"body".as_slice()).bind(NOW as i64).bind((NOW+1000)as i64).execute(&mut *tx).await.unwrap();
    assert!(tx.commit().await.is_err());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admission_artifacts WHERE artifact_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    let source = Retain {
        artifact_id: Uuid::new_v4(),
        project_id: project,
        owner: OwnerKind::SourceRetention,
        artifact: a,
        kind: ArtifactKind::Source,
        bytes: b"body",
        retain_until_ms: NOW + 1000,
    };
    assert!(artifacts::retain(&f.state, source).await.is_err());
}
#[tokio::test]
async fn finite_project_history_refuses_new_rows_without_evicting_or_blocking_exact_replay() {
    let f = fixture().await;
    let project = f.selection.lineage.project_id;
    let a = artifact("synthetic-context", b"x");
    let first = Uuid::new_v4();
    for n in 0..511 {
        let id = if n == 0 { first } else { Uuid::new_v4() };
        artifacts::retain(
            &f.state,
            Retain {
                artifact_id: id,
                project_id: project,
                owner: OwnerKind::GraphSnapshot,
                artifact: a.clone(),
                kind: ArtifactKind::Context,
                bytes: b"x",
                retain_until_ms: NOW + 60_000,
            },
        )
        .await
        .unwrap();
    }
    let left = Retain {
        artifact_id: Uuid::new_v4(),
        project_id: project,
        owner: OwnerKind::GraphSnapshot,
        artifact: a.clone(),
        kind: ArtifactKind::Context,
        bytes: b"x",
        retain_until_ms: NOW + 60_000,
    };
    let right = Retain {
        artifact_id: Uuid::new_v4(),
        project_id: project,
        owner: OwnerKind::GraphSnapshot,
        artifact: a.clone(),
        kind: ArtifactKind::Context,
        bytes: b"x",
        retain_until_ms: NOW + 60_000,
    };
    let (left, right) = tokio::join!(
        artifacts::retain(&f.state, left),
        artifacts::retain(&f.state, right)
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert!(
        artifacts::retain(
            &f.state,
            Retain {
                artifact_id: Uuid::new_v4(),
                project_id: project,
                owner: OwnerKind::GraphSnapshot,
                artifact: a.clone(),
                kind: ArtifactKind::Context,
                bytes: b"x",
                retain_until_ms: NOW + 60_000
            }
        )
        .await
        .is_err()
    );
    artifacts::retain(
        &f.state,
        Retain {
            artifact_id: first,
            project_id: project,
            owner: OwnerKind::GraphSnapshot,
            artifact: a,
            kind: ArtifactKind::Context,
            bytes: b"x",
            retain_until_ms: NOW + 60_000,
        },
    )
    .await
    .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM admission_artifacts WHERE owner_key=$1")
            .bind(project.to_string())
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
    assert_eq!(count, 512);
}

#[tokio::test]
async fn assigned_native_http_ranges_require_exact_sealed_binding_and_live_offer() {
    use aq_admission::{
        RetainedArtifact,
        transport::{ArtifactFetch, Feature, Register, WorkerIdentity},
    };
    use aq_server::{
        execution::authority::hot::HotState,
        profiled_execution::{
            artifact_api,
            config::{ProfiledConfig, QualifiedRuntime},
            registration::{self, NativeRegistration},
        },
    };
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use http_body_util::BodyExt;
    use std::sync::{Arc, atomic::Ordering};
    use tower::ServiceExt;
    async fn post(app: Router, body: &ArtifactFetch, token: &str) -> axum::response::Response {
        app.oneshot(
            Request::post("/v1/worker/profiled/artifact")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
    }
    let mut f = fixture().await;
    let project = f.selection.lineage.project_id;
    let request = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime: artifact("synthetic-runtime", b"runtime"),
        capabilities: vec![],
        features: vec![Feature::ArtifactTransferV1],
    };
    let qualification = artifact("synthetic-runtime-qualification", b"qualified");
    let mut cfg = (*f.state.config).clone();
    cfg.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        context_qualifications: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: request.runtime.clone(),
            qualification: qualification.clone(),
            capabilities: vec![],
            features: request.features.clone(),
            material_delivery: None,
        }],
        projects: vec![],
    }));
    f.state.config = Arc::new(cfg);
    aq_server::profiled_execution::policies::install(&f.state)
        .await
        .unwrap();
    let metadata = NativeRegistration {
        schema_version: 1,
        semantic_digest: registration::semantic_digest(&request).unwrap(),
        request,
        qualification,
        configuration_revision: 1,
        registered_at_ms: NOW,
    };
    let pool = f.db.pool().await;
    sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{native}',$2) WHERE node_id=$1")
        .bind(f.node.node_id)
        .bind(json!(metadata))
        .execute(&pool)
        .await
        .unwrap();
    let attempt = insert(&f).await;
    let hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(hot)).execute(&pool).await.unwrap();
    let body = b"retained context";
    let id = Uuid::new_v4();
    let reference = artifact("synthetic-context", body);
    let retained = artifacts::retain(
        &f.state,
        Retain {
            artifact_id: id,
            project_id: project,
            owner: OwnerKind::GraphSnapshot,
            artifact: reference.clone(),
            kind: ArtifactKind::Context,
            bytes: body,
            retain_until_ms: NOW + 60_000,
        },
    )
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    artifacts::link_locked(&mut tx, attempt, project, &[id], NOW, NOW + 30_000)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let worker = aq_server::build_worker_router(f.state.clone());
    let mut request = ArtifactFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        binding: f.profiled.offer.binding.clone(),
        artifact: reference,
        offset: 0,
        length: 5,
    };
    let mut collected = vec![];
    for offset in [0, 5, 10, 15] {
        request.offset = offset;
        let response = post(worker.clone(), &request, &f.credential).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(
            response.headers()[artifact_api::OFFSET_HEADER],
            offset.to_string()
        );
        let descriptor: RetainedArtifact = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(response.headers()[artifact_api::RETENTION_HEADER].as_bytes())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(json!(descriptor), json!(retained));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.len() <= 5);
        collected.extend_from_slice(&bytes);
    }
    assert_eq!(collected, body);
    retained.verify(&collected, NOW).unwrap();
    request.offset = body.len() as u64;
    assert_eq!(
        post(worker.clone(), &request, &f.credential).await.status(),
        StatusCode::BAD_REQUEST
    );
    request.offset = 0;
    let original = request.binding.graph_access_profile_digest.clone();
    request.binding.graph_access_profile_digest = Digest::of(b"wrong-graph-binding");
    assert_eq!(
        post(worker.clone(), &request, &f.credential).await.status(),
        StatusCode::CONFLICT
    );
    request.binding.graph_access_profile_digest = original;
    assert_eq!(
        post(worker.clone(), &request, "wrong-credential")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT artifact_id FROM admission_artifacts WHERE artifact_id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *held)
        .await
        .unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let credential = f.credential.clone();
    let pending = tokio::spawn(async move { post(worker, &request, &credential).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
            .bind(blocker).fetch_one(&pool).await.unwrap();
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fetch did not reach retained artifact lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    f.time.store((NOW + 30_000) as i64, Ordering::SeqCst);
    held.commit().await.unwrap();
    let response = pending.await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        !response
            .headers()
            .contains_key(artifact_api::RETENTION_HEADER)
    );
}

#[tokio::test]
async fn orphaned_and_settled_artifacts_do_not_count_toward_the_project_cap() {
    let f = fixture().await;
    let project = f.selection.lineage.project_id;
    let pool = f.db.pool().await;
    let now = NOW as i64;
    // 600 unlinked rows older than the 10-minute pending window: orphans.
    let mut tx = crate::common::sealed::replica(&pool).await;
    for _ in 0..600 {
        crate::common::sealed::artifact(&mut tx, project, now - 700_000).await;
    }
    tx.commit().await.unwrap();
    let retain = |name: &str| {
        let a = artifact(name, b"x");
        let state = f.state.clone();
        async move {
            artifacts::retain(
                &state,
                Retain {
                    artifact_id: Uuid::new_v4(),
                    project_id: project,
                    owner: OwnerKind::GraphSnapshot,
                    artifact: a,
                    kind: ArtifactKind::Context,
                    bytes: b"x",
                    retain_until_ms: NOW + 60_000,
                },
            )
            .await
        }
    };
    retain("live-cap-context-1")
        .await
        .expect("orphans are not live");
    // 511 pending rows plus the one just retained make 512 live rows.
    let mut tx = crate::common::sealed::replica(&pool).await;
    for _ in 0..511 {
        crate::common::sealed::artifact(&mut tx, project, now - 1_000).await;
    }
    tx.commit().await.unwrap();
    assert!(retain("live-cap-context-2").await.is_err());
}
