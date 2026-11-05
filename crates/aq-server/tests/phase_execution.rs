//! Actual phase issuance SQL/owners; synthetic Graph, Git and worker installation.
//! No physical host, real provider, or production route qualification.
mod common;
#[path = "common/phase_execution.rs"]
mod fixture;
use aq_admission::{Digest, preparation_v2 as phase};
use aq_server::profiled_execution::{phase_execution, policies};
use axum::body::to_bytes;
use common::native_offer::{self, Case};
use fixture::{Prepared, assert_unissued, auth, prepared, prepared_with_short_observation};
use serde_json::Value;
use uuid::Uuid;
#[tokio::test]
async fn actual_phase_issuer_commits_exact_receipt_and_replay_without_extension() {
    let f = prepared(true).await;
    let auth = auth(&f.c).await;
    let nonce = Uuid::new_v4();
    let first = phase_execution::prepare(&f.c.env.env.state, &auth, &f.prepared, nonce)
        .await
        .unwrap();
    first
        .validate_for(&f.prepared, &f.a, &f.q, &f.p, nonce)
        .unwrap();
    let replay = phase_execution::prepare(&f.c.env.env.state, &auth, &f.prepared, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        first.receipt.digest().unwrap(),
        replay.receipt.digest().unwrap()
    );
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM profiled_phase_execution_grants),(SELECT count(*) FROM admission_material_receipts),(SELECT count(*) FROM profiled_execution_receipts WHERE kind='prepare')").fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(counts, (1, 1, 0));
}
#[tokio::test]
async fn absent_execution_qualification_refuses_without_mutation() {
    let mut f = prepared(true).await;
    let mut config = (*f.c.env.env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    profiled.definition_bodies.retain(|d| {
        !d.artifact()
            .id
            .as_str()
            .starts_with("aq.phase-execution-qualification/")
    });
    config.profiled_execution = Some(std::sync::Arc::new(profiled));
    f.c.env.env.state.config = std::sync::Arc::new(config);
    assert!(
        phase_execution::prepare(
            &f.c.env.env.state,
            &auth(&f.c).await,
            &f.prepared,
            Uuid::new_v4()
        )
        .await
        .is_err()
    );
    assert_unissued(&f.c).await;
}
#[tokio::test]
async fn original_legacy_inventory_is_not_relabelled_as_phase_qualification() {
    let f = prepared(false).await;
    assert!(
        phase_execution::prepare(
            &f.c.env.env.state,
            &auth(&f.c).await,
            &f.prepared,
            Uuid::new_v4()
        )
        .await
        .is_err()
    );
    assert_unissued(&f.c).await;
}

#[path = "phase_execution/atomicity.rs"]
mod atomicity;

#[path = "phase_execution/refusals.rs"]
mod refusals;

#[path = "phase_execution/legacy_origin.rs"]
mod legacy_origin;
