//! Artifact HTTP phase fences over real accepted-plan/grant SQL records.
//! Runtime, material and guardian fixtures are synthetic; no process is launched.
use crate::common;
use crate::profiled_artifacts::transfer::{post, register};
use aq_admission::{
    ArtifactKind, Digest, Label, MaterialLease, RetainedArtifact,
    transport::{
        ArtifactFetch, DeliveryClock, NoLaunchReservation, PreparationPlan, TransportDocument,
        WorkerIdentity,
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
        journal,
    },
};
use axum::{http::StatusCode, response::Response};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::profiled::{Fixture, NOW, artifact, fixture, insert};
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::atomic::Ordering;
mod preparation;
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
    let pool = register(
        &mut f,
        artifact("synthetic-artifact-phase-runtime", b"runtime"),
        artifact(
            "synthetic-artifact-phase-qualification",
            b"qualified fixture",
        ),
    )
    .await;
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
    let preparation = preparation::Preparation::new(&f, attempt);
    let mut tx = pool.begin().await.unwrap();
    preparation.accept(&mut tx, &f, granted).await;
    artifacts::link_locked(
        &mut tx,
        artifacts::LinkScope::new(attempt, f.selection.lineage.project_id, NOW..PLAN_EXPIRY),
        &[artifact_id],
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
    post(
        aq_server::build_worker_router(case.f.state.clone()),
        &case.request,
        &case.f.credential,
    )
    .await
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
        case.change_hot(|hot| {
            hot.status = Status::Cancelling;
            hot.cancellation_id = Some(Uuid::new_v4());
        })
        .await;
        assert_refused(&case).await;
    }
}

#[tokio::test]
async fn preparing_fetch_refuses_missing_current_accept_receipt_despite_live_offer_and_blob() {
    let case = setup(false).await;
    assert_bytes(&case).await;
    case.change_hot(|hot| hot.preparation_operation_id = Some(Uuid::new_v4()))
        .await;
    assert_refused(&case).await;
}

impl Case {
    async fn change_hot(&self, change: impl FnOnce(&mut HotState)) {
        let pool = self.f.db.pool().await;
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(self.f.node.node_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
            .bind(self.request.binding.attempt_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let state: serde_json::Value = sqlx::query_scalar(
            "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(self.request.binding.attempt_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let mut hot: HotState = serde_json::from_value(state).unwrap();
        change(&mut hot);
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(self.request.binding.attempt_id)
            .bind(json!(hot))
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
}
