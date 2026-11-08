//! Qualified synthetic definitions, actual private reads/current auth/SQL owners.
use crate::common;
use aq_admission::{Digest, scoped_presence::*};
use aq_server::{
    profiled_execution::{policies, presence::inventory},
    worker_api,
};
use common::presence as presence_fixture;
use presence_fixture::{Case, definition, setup};
use serde_json::{Value, json};
use uuid::Uuid;
async fn auth(c: &Case) -> worker_api::AuthenticatedNode {
    let mut h = axum::http::HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {}", c.c.env.env.credential)
            .parse()
            .unwrap(),
    );
    worker_api::authenticate_context(&c.c.env.env.state, &h)
        .await
        .unwrap()
}
async fn record(c: &Case, r: &InventoryV2) -> InventoryReceipt {
    inventory::record(&c.c.env.env.state, &auth(c).await, r)
        .await
        .unwrap_or_else(|error| panic!("inventory seq{} failed: {error:?}", r.sequence))
}
async fn epoch(c: &Case) -> (i64, String, i64) {
    sqlx::query_as("SELECT compatibility_epoch,mode,revision FROM profiled_inventory_semantic_heads WHERE node_id=$1").bind(c.request.identity.node_id).fetch_one(&c.c.env.env.pool).await.unwrap()
}
#[tokio::test]
async fn actual_qualified_private_owner_records_provisionability_and_exact_replay_never_retimes() {
    let c = setup().await;
    let first = record(&c, &c.request).await;
    first.validate_for(&c.request).unwrap();
    assert_eq!(epoch(&c).await, (1, "scoped_v1".into(), 1));
    let snapshot: Value =
        sqlx::query_scalar("SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1")
            .bind(c.request.identity.node_id)
            .fetch_one(&c.c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(snapshot["credential_bindings"][0]["available"], false);
    assert!(!snapshot["credential_bindings"][0]["preparation"].is_null());
    assert_eq!(
        aq_selection::digest(&snapshot).unwrap(),
        first.inventory.snapshot_digest
    );
    let replay = record(&c, &c.request).await;
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&replay).unwrap()
    );
    assert_eq!(epoch(&c).await, (1, "scoped_v1".into(), 1));
    let mut changed = c.request.clone();
    changed.ttl_ms -= 1;
    assert!(
        inventory::record(&c.c.env.env.state, &auth(&c).await, &changed)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn compatible_revisions_preserve_epoch_but_new_legacy_and_restore_create_barriers() {
    let c = setup().await;
    record(&c, &c.request).await;
    let mut next = c.request.clone();
    next.operation_id = Uuid::new_v4();
    next.sequence += 1;
    record(&c, &next).await;
    assert_eq!(epoch(&c).await, (1, "scoped_v1".into(), 2));
    let legacy = json!({"operation_id":Uuid::new_v4(),"boot_id":c.request.identity.boot_id,"sequence":4,"ttl_ms":30_000,"qualifications":c.request.qualifications,"credentials":[]});
    let response = common::native_offer::post(
        &c.c.env.env.state,
        &c.c.env.env.credential,
        "/v1/worker/intelligence/inventory",
        serde_json::to_vec(&legacy).unwrap(),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(epoch(&c).await, (2, "legacy".into(), 3));
    next.operation_id = Uuid::new_v4();
    next.sequence = 5;
    record(&c, &next).await;
    assert_eq!(epoch(&c).await, (3, "scoped_v1".into(), 4));
    // Historical v1 retry must not reinstall its snapshot or invalidate v2 again.
    let response = common::native_offer::post(
        &c.c.env.env.state,
        &c.c.env.env.credential,
        "/v1/worker/intelligence/inventory",
        serde_json::to_vec(&legacy).unwrap(),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(epoch(&c).await, (3, "scoped_v1".into(), 4));
}
#[tokio::test]
async fn missing_exact_qualification_or_referenced_body_is_not_capability_inference() {
    let c = setup().await;
    let mut config = (*c.c.env.env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    profiled
        .definition_bodies
        .retain(|d| d.artifact().id.as_str() != "aq.presence-adapter/test");
    profiled.revision += 1;
    config.profiled_execution = Some(std::sync::Arc::new(profiled));
    let mut state = c.c.env.env.state.clone();
    state.config = std::sync::Arc::new(config);
    policies::install(&state).await.unwrap();
    assert!(
        inventory::record(&state, &auth(&c).await, &c.request)
            .await
            .is_err()
    );
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_presence_operations")
            .fetch_one(&c.c.env.env.pool)
            .await
            .unwrap()
            == 0
    );
}
#[tokio::test]
async fn empty_inventory_derives_only_actual_authorized_backing_and_never_installed() {
    let c = setup().await;
    let mut request = c.request.clone();
    request.provisionable_only.clear();
    let ack = record(&c, &request).await;
    let snapshot: Value =
        sqlx::query_scalar("SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1")
            .bind(request.identity.node_id)
            .fetch_one(&c.c.env.env.pool)
            .await
            .unwrap();
    let bindings = snapshot["credential_bindings"].as_array().unwrap();
    assert!(!bindings.is_empty());
    assert!(
        bindings
            .iter()
            .all(|b| b["available"] == false && !b["preparation"].is_null())
    );
    assert_eq!(
        aq_selection::digest(&snapshot).unwrap(),
        ack.inventory.snapshot_digest
    );
    let mut next = request;
    next.operation_id = Uuid::new_v4();
    next.sequence += 1;
    next.provisionable_only.push(CredentialReference {
        profile: common::profiled::artifact("unknown-credential", b"not backed"),
        revision: 1,
    });
    assert!(
        inventory::record(&c.c.env.env.state, &auth(&c).await, &next)
            .await
            .is_err()
    );
    assert_eq!(epoch(&c).await, (1, "scoped_v1".into(), 1));
}
#[tokio::test]
async fn qualification_change_then_restore_retains_barrier_and_exact_retry_cannot_reset_it() {
    let c = setup().await;
    record(&c, &c.request).await;
    let mut config = (*c.c.env.env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    let old = profiled
        .definition_bodies
        .iter()
        .find(|d| d.artifact() == &c.request.negotiation.qualification)
        .unwrap();
    let mut q: Qualification = serde_json::from_str(old.body()).unwrap();
    q.maximum_age_ms = 29_999;
    q.maximum_ttl_ms = 29_999;
    let new = definition(
        "aq.scoped-material-presence-qualification/test-second",
        serde_json::to_string(&q).unwrap(),
    );
    let reference = new.artifact().clone();
    profiled.definition_bodies.push(new);
    profiled.revision += 1;
    config.profiled_execution = Some(std::sync::Arc::new(profiled));
    let mut state = c.c.env.env.state.clone();
    state.config = std::sync::Arc::new(config);
    policies::install(&state).await.unwrap();
    let mut next = c.request.clone();
    next.operation_id = Uuid::new_v4();
    next.sequence = 3;
    next.ttl_ms = 29_999;
    next.negotiation.qualification = reference;
    inventory::record(&state, &auth(&c).await, &next)
        .await
        .unwrap();
    assert_eq!(epoch(&c).await, (2, "scoped_v1".into(), 2));
    let mut restored = c.request.clone();
    restored.operation_id = Uuid::new_v4();
    restored.sequence = 4;
    inventory::record(&state, &auth(&c).await, &restored)
        .await
        .unwrap();
    assert_eq!(epoch(&c).await, (3, "scoped_v1".into(), 3));
    inventory::record(&state, &auth(&c).await, &next)
        .await
        .unwrap();
    assert_eq!(epoch(&c).await, (3, "scoped_v1".into(), 3));
}
#[tokio::test]
async fn immutable_operation_bytes_and_derived_head_revisions_cannot_be_retimed_by_sql() {
    let c = setup().await;
    let first = record(&c, &c.request).await;
    assert!(
        sqlx::query(
            "UPDATE profiled_presence_operations SET expires_at_ms=expires_at_ms+1 WHERE node_id=$1"
        )
        .bind(c.request.identity.node_id)
        .execute(&c.c.env.env.pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM profiled_presence_operations WHERE node_id=$1")
            .bind(c.request.identity.node_id)
            .execute(&c.c.env.env.pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE profiled_inventory_semantic_heads SET revision=999,compatibility_epoch=999 WHERE node_id=$1").bind(c.request.identity.node_id).execute(&c.c.env.env.pool).await.unwrap();
    assert_eq!(epoch(&c).await, (1, "scoped_v1".into(), 1));
    assert!(
        sqlx::query(
            "UPDATE profiled_inventory_semantic_heads SET snapshot_digest=$2 WHERE node_id=$1"
        )
        .bind(c.request.identity.node_id)
        .bind(Digest::of(b"different bytes").as_str())
        .execute(&c.c.env.env.pool)
        .await
        .is_err()
    );
    let (count, logical): (i64, i64) = sqlx::query_as(
        "SELECT operations,logical_bytes FROM profiled_presence_storage WHERE singleton",
    )
    .fetch_one(&c.c.env.env.pool)
    .await
    .unwrap();
    let expected:i64=sqlx::query_scalar("SELECT sum(octet_length(request_bytes)+octet_length(receipt_bytes)+1024)::bigint FROM profiled_presence_operations").fetch_one(&c.c.env.env.pool).await.unwrap();
    assert_eq!(count, 1);
    assert_eq!(logical, expected);
    assert_eq!(
        record(&c, &c.request).await.inventory.snapshot_digest,
        first.inventory.snapshot_digest
    );
}
/// Live acceptance D11: inventory-v2 answered an unlogged 500 while a
/// concurrent owner (a renewal reads the snapshot row FOR SHARE) held the
/// node's snapshot row. A lock timeout is a retryable 503, never a 500.
#[tokio::test]
async fn snapshot_row_lock_timeout_is_retryable_not_internal() {
    let c = setup().await;
    record(&c, &c.request).await;
    let pool = &c.c.env.env.pool;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM intelligence_node_snapshots WHERE node_id=$1 FOR SHARE")
        .bind(c.request.identity.node_id)
        .execute(&mut *holder)
        .await
        .unwrap();
    let mut next = c.request.clone();
    next.operation_id = Uuid::new_v4();
    next.sequence += 1;
    let error = inventory::record(&c.c.env.env.state, &auth(&c).await, &next)
        .await
        .unwrap_err();
    assert_eq!(error.status(), 503, "{error:?}");
    holder.rollback().await.unwrap();
    record(&c, &next).await;
}
