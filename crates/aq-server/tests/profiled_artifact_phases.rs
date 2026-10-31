//! Artifact HTTP phase fences over real accepted-plan/grant SQL records.
//! Runtime, material and guardian fixtures are synthetic; no process is launched.
mod common;
use aq_admission::{
    ArtifactKind, Digest, Label, MaterialLease, RetainedArtifact,
    transport::{
        ArtifactFetch, DeliveryClock, Feature, NoLaunchReservation, PreparationPlan, Register,
        TransportDocument, WorkerIdentity,
    },
};
use aq_server::{
    execution::{
        authority::hot::{HotState, LaunchOwner},
        profiled::{Status, Stop},
    },
    profiled_execution::{
        artifact_api,
        artifacts::{self, OwnerKind, Retain},
        config::{ProfiledConfig, QualifiedRuntime},
        journal,
        registration::{self, NativeRegistration},
    },
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::profiled::{Fixture, NOW, artifact, fixture, insert};
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::{Arc, atomic::Ordering};
use tower::ServiceExt;
use uuid::Uuid;
const BODY: &[u8] = b"retained phase-specific context";
const PLAN_EXPIRY: u64 = NOW + 60_000;
const GRANT_EXPIRY: u64 = NOW + 5_000;
struct Case {
    f: Fixture,
    request: ArtifactFetch,
    descriptor: RetainedArtifact,
}
async fn setup(granted: bool) -> Case {
    let mut f = fixture().await;
    let registration = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime: artifact("synthetic-artifact-phase-runtime", b"runtime"),
        capabilities: vec![],
        features: vec![Feature::ArtifactTransferV1],
    };
    let qualification = artifact(
        "synthetic-artifact-phase-qualification",
        b"qualified fixture",
    );
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: registration.runtime.clone(),
            qualification: qualification.clone(),
            capabilities: vec![],
            features: registration.features.clone(),
            material_delivery: None,
        }],
        projects: vec![],
    }));
    f.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&f.state)
        .await
        .unwrap();
    let native = NativeRegistration {
        schema_version: 1,
        semantic_digest: registration::semantic_digest(&registration).unwrap(),
        request: registration,
        qualification,
        configuration_revision: 1,
        registered_at_ms: NOW,
    };
    let pool = f.db.pool().await;
    sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{native}',$2) WHERE node_id=$1")
        .bind(f.node.node_id)
        .bind(json!(native))
        .execute(&pool)
        .await
        .unwrap();
    let attempt = insert(&f).await;
    let artifact_id = Uuid::new_v4();
    let reference = artifact("synthetic-phase-context", BODY);
    let descriptor = artifacts::retain(
        &f.state,
        Retain {
            artifact_id,
            project_id: f.selection.lineage.project_id,
            owner: OwnerKind::GraphSnapshot,
            artifact: reference.clone(),
            kind: ArtifactKind::Context,
            bytes: BODY,
            retain_until_ms: NOW + 120_000,
        },
    )
    .await
    .unwrap();
    let owner = LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    };
    let operation = Uuid::new_v4();
    let lease = MaterialLease {
        schema_version: 1,
        lease_id: Uuid::new_v4(),
        binding: f.profiled.offer.binding.clone(),
        credential_profile: f.selection.choice.credential_profile.clone(),
        credential_revision: f.selection.credential_revision,
        provider: Label::new(&f.selection.choice.provider).unwrap(),
        account_reference: Label::new(&f.selection.choice.account_reference).unwrap(),
        endpoint_reference: Label::new(&f.selection.choice.endpoint_reference).unwrap(),
        purpose: Label::new("model-authentication").unwrap(),
        material_digest: Digest::of(b"synthetic-only-key"),
        material_bytes: 18,
        issued_at_ms: NOW,
        expires_at_ms: PLAN_EXPIRY,
    };
    let plan = PreparationPlan {
        schema_version: 1,
        operation_id: operation,
        binding: lease.binding.clone(),
        reservation: NoLaunchReservation {
            reservation_id: owner.reservation_id,
            reservation_operation_id: Uuid::new_v4(),
            reservation_journal_digest: Digest::of(b"synthetic reserved journal"),
        },
        guardian_id: owner.guardian_id,
        launch_id: owner.launch_id,
        clock: DeliveryClock::new(NOW, PLAN_EXPIRY).unwrap(),
        materials: vec![lease.clone()],
    };
    let document = TransportDocument::PreparationPlan(plan);
    document.validate().unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
        .bind(attempt)
        .execute(&mut *tx)
        .await
        .unwrap();
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(hot)).execute(&mut *tx).await.unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(owner.clone());
    hot.preparation_operation_id = Some(operation);
    sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'accept',$3,$4)").bind(f.node.node_id).bind(operation).bind(Digest::of(b"synthetic phase accept").as_str()).bind(json!(document)).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&mut *tx)
        .await
        .unwrap();
    journal::record_reserved(
        &mut tx,
        &f.profiled.offer.binding,
        &owner,
        1,
        &Digest::of(b"synthetic reserved journal"),
    )
    .await
    .unwrap();
    let identity = format!(
        "{}@{}",
        lease.credential_profile.id.as_str(),
        lease.credential_profile.version.as_str()
    );
    sqlx::query("INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,admitted_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$6,$7,$8,$9,$10,$11,$12,'admitted',$13,$14)").bind(lease.lease_id).bind(Uuid::new_v4()).bind(Digest::of(b"synthetic lease issuance").as_str()).bind(f.node.node_id).bind(f.node.boot_id).bind(attempt).bind(lease.binding.offer_id).bind(identity).bind(lease.credential_revision as i64).bind(lease.purpose.as_str()).bind(json!(lease)).bind(Digest::of(b"synthetic private verifier").as_str()).bind(NOW as i64).bind(PLAN_EXPIRY as i64).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO profiled_launch_materials(attempt_id,lease_id) VALUES($1,$2)")
        .bind(attempt)
        .bind(lease.lease_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    if granted {
        hot.status = Status::Granted;
        hot.execution_deadline_ms = Some(NOW + 20_000);
        hot.grant = Some(aq_session_protocol::ProfiledGrant {
            binding: f.profiled.offer.binding.clone(),
            grant_id: Uuid::new_v4(),
            expires_at_ms: GRANT_EXPIRY,
        });
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(json!(hot))
            .execute(&mut *tx)
            .await
            .unwrap();
        journal::record_prepared(
            &mut tx,
            &f.profiled.offer.binding,
            &owner,
            hot.grant.as_ref().unwrap().grant_id,
            2,
            &Digest::of(b"synthetic prepared journal"),
        )
        .await
        .unwrap();
    }
    artifacts::link_locked(
        &mut tx,
        attempt,
        f.selection.lineage.project_id,
        &[artifact_id],
        NOW,
        PLAN_EXPIRY,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    Case {
        request: ArtifactFetch {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity: WorkerIdentity {
                node_id: f.node.node_id,
                boot_id: f.node.boot_id,
            },
            binding: f.profiled.offer.binding.clone(),
            artifact: reference,
            offset: 4,
            length: 9,
        },
        f,
        descriptor,
    }
}
async fn fetch(case: &Case) -> Response {
    aq_server::build_worker_router(case.f.state.clone())
        .oneshot(
            Request::post("/v1/worker/profiled/artifact")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", case.f.credential))
                .body(Body::from(serde_json::to_vec(&case.request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn assert_bytes(case: &Case) {
    let response = fetch(case).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()[artifact_api::OFFSET_HEADER], "4");
    let descriptor: RetainedArtifact = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(response.headers()[artifact_api::RETENTION_HEADER].as_bytes())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(json!(descriptor), json!(case.descriptor));
    assert_eq!(
        &response.into_body().collect().await.unwrap().to_bytes()[..],
        &BODY[4..13]
    );
}
async fn assert_refused(case: &Case) {
    let response = fetch(case).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        !response
            .headers()
            .contains_key(artifact_api::RETENTION_HEADER)
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error"],
        "conflict"
    );
    assert!(!body.windows(BODY.len()).any(|window| window == BODY));
}
#[tokio::test]
async fn preparing_fetch_uses_accepted_plan_after_offer_expiry_and_refuses_plan_expiry() {
    let case = setup(false).await;
    // The sealed offer expired atNOW+30s; the actual accepted plan lasts60s.
    case.f.time.store((NOW + 31_000) as i64, Ordering::SeqCst);
    assert_bytes(&case).await;
    case.f.time.store(PLAN_EXPIRY as i64, Ordering::SeqCst);
    assert_refused(&case).await;
}
#[tokio::test]
async fn granted_fetch_uses_current_grant_expiry_without_inventing_started_state() {
    let case = setup(true).await;
    case.f
        .time
        .store((GRANT_EXPIRY - 1) as i64, Ordering::SeqCst);
    let pool = case.f.db.pool().await;
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(h) FROM profiled_authority_hot h WHERE attempt_id=$1")
            .bind(case.request.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_bytes(&case).await;
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(h) FROM profiled_authority_hot h WHERE attempt_id=$1")
            .bind(case.request.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    let hot: HotState = serde_json::from_value(after["state"].clone()).unwrap();
    assert!(matches!(hot.stop, Stop::Unknown));
    case.f.time.store(GRANT_EXPIRY as i64, Ordering::SeqCst);
    // The accepted plan and retained blob are both still live here.
    assert_refused(&case).await;
}
#[tokio::test]
async fn preparing_and_granted_fetches_refuse_cancellation_while_deadlines_are_live() {
    for granted in [false, true] {
        let case = setup(granted).await;
        assert_bytes(&case).await;
        let pool = case.f.db.pool().await;
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(case.f.node.node_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
            .bind(case.request.binding.attempt_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let state: serde_json::Value = sqlx::query_scalar(
            "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(case.request.binding.attempt_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let mut hot: HotState = serde_json::from_value(state).unwrap();
        hot.status = Status::Cancelling;
        hot.cancellation_id = Some(Uuid::new_v4());
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(case.request.binding.attempt_id)
            .bind(json!(hot))
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_refused(&case).await;
    }
}

#[tokio::test]
async fn preparing_fetch_refuses_missing_current_accept_receipt_despite_live_offer_and_blob() {
    let case = setup(false).await;
    assert_bytes(&case).await;
    let pool = case.f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(case.f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
        .bind(case.request.binding.attempt_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let state: serde_json::Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(case.request.binding.attempt_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let mut hot: HotState = serde_json::from_value(state).unwrap();
    hot.preparation_operation_id = Some(Uuid::new_v4());
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(case.request.binding.attempt_id)
        .bind(json!(hot))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_refused(&case).await;
}
