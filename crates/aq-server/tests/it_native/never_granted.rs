//! Actual SQL absence-of-authorization owner; never physical guardian stop proof.
use crate::common;
use aq_server::{
    execution::{
        authority::hot::HotState,
        authority::{self, Policy},
        profiled::{Status, Stop},
    },
    profiled_execution::no_grant,
};
use common::profiled::{NOW, fixture, insert};
use serde_json::json;
use sqlx::Row;
use std::sync::atomic::Ordering;
use uuid::Uuid;
async fn setup() -> (common::profiled::Fixture, Uuid, sqlx::PgPool) {
    let f = fixture().await;
    let attempt = insert(&f).await;
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(HotState::from_compat(&f.profiled).unwrap())).execute(&pool).await.unwrap();
    (f, attempt, pool)
}
#[tokio::test]
async fn offline_expired_offer_closes_without_guardian_and_preserves_history_budget_and_replay() {
    let (f, attempt, pool) = setup().await;
    common::roles::pin_active(&pool, attempt).await;
    assert!(
        no_grant::close_expired(&f.state, f.node.node_id, attempt)
            .await
            .unwrap()
            .is_none()
    );
    assert!(common::roles::pinned(&pool, attempt).await);
    f.time.store((NOW + 30000) as i64, Ordering::SeqCst);
    let first = no_grant::close_expired(&f.state, f.node.node_id, attempt)
        .await
        .unwrap()
        .unwrap();
    assert!(first.changed);
    assert_eq!(first.proof.binding, f.profiled.offer.binding);
    assert_eq!(first.proof.reason, "offer_expired");
    let replay = no_grant::close_expired(&f.state, f.node.node_id, attempt)
        .await
        .unwrap()
        .unwrap();
    assert!(!replay.changed);
    assert_eq!(first.bytes, replay.bytes);
    let counts = no_grant::attempt_counts(&pool, f.selection.lineage.task_id)
        .await
        .unwrap();
    assert_eq!(
        (counts.started, counts.history, counts.never_granted),
        (0, 1, 1)
    );
    assert_eq!(counts.budget_revision(), 2);
    let mut tx = pool.begin().await.unwrap();
    let view = authority::load_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        Policy::IdentityOnly,
    )
    .await
    .unwrap();
    assert_eq!(view.status, Status::NeverGranted);
    assert!(!view.holds_task);
    // Spec 0014 §7.1: the snapshot pin is released with the attempt's hold on its task.
    assert!(!common::roles::pinned(&pool, attempt).await);
    assert!(!view.stopped);
    assert!(matches!(view.stop, Some(Stop::Unknown)));
    assert!(view.active_at(NOW + 30000).is_err());
    tx.rollback().await.unwrap();
    assert!(
        sqlx::query("UPDATE execution_attempts SET holds_task=true WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM profiled_never_granted_closures WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{status}','\"preparing\"') WHERE attempt_id=$1").bind(attempt).execute(&pool).await.is_err());
}
#[tokio::test]
async fn contradictory_effects_are_never_absence_proofs() {
    let (f, attempt, pool) = setup().await;
    f.time.store((NOW + 30000) as i64, Ordering::SeqCst);
    sqlx::query("INSERT INTO observation_nodes(node_id) VALUES($1)")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO observation_sources(node_id,source_id,generation,attempt_id,authority,authority_digest) VALUES($1,$2,1,$3,'{}',$4)").bind(f.node.node_id).bind(Uuid::new_v4()).bind(attempt).bind("a".repeat(64)).execute(&pool).await.unwrap();
    assert!(
        no_grant::close_expired(&f.state, f.node.node_id, attempt)
            .await
            .is_err()
    );
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held);
    let row = sqlx::query("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
        .bind(attempt)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.get::<serde_json::Value, _>("state")["status"],
        json!("offered")
    );
}

async fn lease(
    f: &common::profiled::Fixture,
    attempt: Uuid,
    pool: &sqlx::PgPool,
    expiry: u64,
) -> Uuid {
    use aq_admission::{Digest, Label, MaterialLease};
    let id = Uuid::new_v4();
    let choice = &f.selection.choice;
    let body = MaterialLease {
        schema_version: 1,
        lease_id: id,
        binding: f.profiled.offer.binding.clone(),
        credential_profile: choice.credential_profile.clone(),
        credential_revision: f.selection.credential_revision,
        provider: Label::new(&choice.provider).unwrap(),
        account_reference: Label::new(&choice.account_reference).unwrap(),
        endpoint_reference: Label::new(&choice.endpoint_reference).unwrap(),
        purpose: Label::new("model-authentication").unwrap(),
        material_digest: Digest::of(b"synthetic-key"),
        material_bytes: 13,
        issued_at_ms: NOW,
        expires_at_ms: expiry,
    };
    body.validate().unwrap();
    sqlx::query("INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,admitted_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$6,$7,'synthetic-key@1',1,'model-authentication',$8,$9,'admitted',$10,$11)")
        .bind(id).bind(Uuid::new_v4()).bind("a".repeat(64)).bind(f.node.node_id).bind(f.node.boot_id).bind(attempt).bind(f.profiled.offer.binding.offer_id).bind(json!(body)).bind(Digest::of(id.as_bytes()).as_str()).bind(NOW as i64).bind(expiry as i64).execute(pool).await.unwrap();
    id
}
async fn plan(
    f: &common::profiled::Fixture,
    hot: &mut HotState,
    pool: &sqlx::PgPool,
    material: Uuid,
    expiry: u64,
) {
    use aq_admission::transport::*;
    let body: serde_json::Value =
        sqlx::query_scalar("SELECT lease FROM admission_material_leases WHERE lease_id=$1")
            .bind(material)
            .fetch_one(pool)
            .await
            .unwrap();
    let operation = Uuid::new_v4();
    let owner = hot.owner.as_ref().unwrap();
    let value = PreparationPlan {
        schema_version: 1,
        operation_id: operation,
        binding: f.profiled.offer.binding.clone(),
        reservation: NoLaunchReservation {
            reservation_id: owner.reservation_id,
            reservation_operation_id: Uuid::new_v4(),
            reservation_journal_digest: aq_admission::Digest::of(b"synthetic-reservation"),
        },
        guardian_id: owner.guardian_id,
        launch_id: owner.launch_id,
        clock: DeliveryClock::new(NOW, expiry).unwrap(),
        materials: vec![serde_json::from_value(body).unwrap()],
    };
    value.validate().unwrap();
    sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'accept',$3,$4)").bind(f.node.node_id).bind(operation).bind("a".repeat(64)).bind(json!(TransportDocument::PreparationPlan(value))).execute(pool).await.unwrap();
    hot.preparation_operation_id = Some(operation);
}

#[tokio::test]
async fn preparing_deadline_revokes_actual_material_and_ever_issued_grant_never_closes() {
    use aq_server::execution::authority::hot::LaunchOwner;
    let (f, attempt, pool) = setup().await;
    let old_lease = lease(&f, attempt, &pool, NOW + 1000).await;
    sqlx::query("UPDATE admission_material_leases SET state='revoked' WHERE lease_id=$1")
        .bind(old_lease)
        .execute(&pool)
        .await
        .unwrap();
    let lease = lease(&f, attempt, &pool, NOW + 60000).await;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    plan(&f, &mut hot, &pool, lease, NOW + 60000).await;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    f.time.store((NOW + 30000) as i64, Ordering::SeqCst);
    assert!(
        no_grant::close_expired(&f.state, f.node.node_id, attempt)
            .await
            .unwrap()
            .is_none()
    );
    f.time.store((NOW + 60000) as i64, Ordering::SeqCst);
    let closed = no_grant::close_expired(&f.state, f.node.node_id, attempt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(closed.proof.reason, "preparation_expired");
    assert_eq!(closed.proof.material_leases_revoked.len(), 2);
    assert!(closed.proof.material_leases_revoked.contains(&lease));
    let state: String =
        sqlx::query_scalar("SELECT state FROM admission_material_leases WHERE lease_id=$1")
            .bind(lease)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "revoked");
    assert!(
        sqlx::query("UPDATE admission_material_leases SET state='admitted' WHERE lease_id=$1")
            .bind(lease)
            .execute(&pool)
            .await
            .is_err()
    );
    let (f, attempt, pool) = setup().await;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    hot.status = Status::Granted;
    hot.execution_deadline_ms = Some(NOW + 60000);
    hot.grant = Some(aq_session_protocol::ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 1,
    });
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    f.time.store((NOW + 60000) as i64, Ordering::SeqCst);
    assert!(
        no_grant::close_expired(&f.state, f.node.node_id, attempt)
            .await
            .is_err()
    );
    assert_eq!(
        no_grant::attempt_counts(&pool, f.selection.lineage.task_id)
            .await
            .unwrap()
            .started,
        1
    );
}
#[tokio::test]
async fn repeated_unissued_offers_keep_lineage_generation_and_finite_retry_budget() {
    let (mut f, mut attempt, pool) = setup().await;
    let lineage = aq_admission::transport::task_lineage_id(
        f.selection.lineage.project_id,
        f.selection.lineage.workspace_id,
        f.selection.lineage.task_id,
    )
    .unwrap();
    f.time.store((NOW + 30000) as i64, Ordering::SeqCst);
    // The bound is Settings › Selection defaults `max_never_granted_offers`.
    let max = aq_profiles::roles::SelectionDefaults::default().max_never_granted_offers;
    for generation in 1..=max {
        assert_eq!(
            no_grant::next_generation(&pool, f.selection.lineage.task_id)
                .await
                .unwrap(),
            u64::from(generation) + 1
        );
        no_grant::close_expired(&f.state, f.node.node_id, attempt)
            .await
            .unwrap()
            .unwrap();
        let counts = no_grant::attempt_counts(&pool, f.selection.lineage.task_id)
            .await
            .unwrap();
        assert_eq!(counts.started, 0);
        assert_eq!(counts.history, generation);
        assert_eq!(counts.budget_revision(), u64::from(generation) + 1);
        assert_eq!(
            lineage,
            aq_admission::transport::task_lineage_id(
                f.selection.lineage.project_id,
                f.selection.lineage.workspace_id,
                f.selection.lineage.task_id
            )
            .unwrap()
        );
        if generation == max {
            assert!(no_grant::check_retry_bound(counts).is_err());
            break;
        }
        no_grant::check_retry_bound(counts).unwrap();
        let next = Uuid::new_v4();
        let session = Uuid::new_v4();
        let next_generation_value = u64::from(generation) + 1;
        let identity = aq_session_protocol::SessionIdentity::Remote {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
            attempt_id: next,
            assignment_generation: next_generation_value,
            session_id: session,
        };
        f.profiled.offer.binding.session = identity.clone();
        f.profiled.offer.binding.attempt_id = next;
        f.profiled.offer.binding.assignment_generation = next_generation_value;
        f.profiled.offer.binding.offer_id = Uuid::new_v4();
        f.profiled.offer.manifest.bindings.session = identity;
        f.profiled.offer.manifest.bindings.attempt_id = next;
        f.profiled.offer.manifest.bindings.assignment_generation = next_generation_value;
        f.profiled.offer.binding.launch_manifest_digest =
            aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
        f.decision = Uuid::new_v4();
        attempt = insert(&f).await;
        sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(HotState::from_compat(&f.profiled).unwrap())).execute(&pool).await.unwrap();
    }
}

#[tokio::test]
async fn queued_mail_becomes_stale_but_received_or_unknown_mail_refuses_closure() {
    for disposition in [
        "pending",
        "stale",
        "received_by_node",
        "handed_to_harness",
        "delivery_unknown",
    ] {
        let (f, attempt, pool) = setup().await;
        f.time.store((NOW + 30000) as i64, Ordering::SeqCst);
        sqlx::query("INSERT INTO mailbox_heads(mailbox_key) VALUES('synthetic-task')")
            .execute(&pool)
            .await
            .unwrap();
        let message = Uuid::new_v4();
        let delivery = Uuid::new_v4();
        sqlx::query("INSERT INTO mailbox_messages(message_id,mailbox_key,cursor,thread_id,sender,recipient,payload_digest,body,created_at_ms) VALUES($1,'synthetic-task',1,$2,'{\"kind\":\"human\"}','{}',$3,'synthetic queued task mail',$4)").bind(message).bind(Uuid::new_v4()).bind("a".repeat(64)).bind(NOW as i64).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO mailbox_deliveries(delivery_id,message_id,attempt_id,disposition,updated_at_ms) VALUES($1,$2,$3,$4,$5)").bind(delivery).bind(message).bind(attempt).bind(disposition).bind(NOW as i64).execute(&pool).await.unwrap();
        let result = no_grant::close_expired(&f.state, f.node.node_id, attempt).await;
        if ["pending", "stale"].contains(&disposition) {
            let closure = result.unwrap().unwrap();
            assert_eq!(
                closure.proof.pending_deliveries_staled_count,
                u32::from(disposition == "pending")
            );
            if disposition == "pending" {
                assert_eq!(closure.proof.pending_deliveries_staled, vec![delivery]);
            }
            let actual: String = sqlx::query_scalar(
                "SELECT disposition FROM mailbox_deliveries WHERE delivery_id=$1",
            )
            .bind(delivery)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(actual, "stale");
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn missing_or_unsupported_empty_material_plan_is_unknown_not_fake_preparation_authority() {
    use aq_server::execution::authority::hot::LaunchOwner;
    let (f, attempt, pool) = setup().await;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    f.time.store((NOW + 120000) as i64, Ordering::SeqCst);
    assert!(
        no_grant::close_expired(&f.state, f.node.node_id, attempt)
            .await
            .is_err()
    );
    let lease = lease(&f, attempt, &pool, NOW + 120000).await;
    plan(&f, &mut hot, &pool, lease, NOW + 120000).await;
    sqlx::query("UPDATE profiled_execution_receipts SET response=jsonb_set(response,'{body,materials}','[]') WHERE node_id=$1 AND operation_id=$2").bind(f.node.node_id).bind(hot.preparation_operation_id.unwrap()).execute(&pool).await.unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        no_grant::close_expired(&f.state, f.node.node_id, attempt)
            .await
            .is_err()
    );
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held);
}

#[tokio::test]
async fn observed_node_lock_race_issued_authority_wins_and_closure_cannot_erase_it() {
    use aq_server::execution::authority::hot::LaunchOwner;
    let (f, attempt, pool) = setup().await;
    let material = lease(&f, attempt, &pool, NOW + 60000).await;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    plan(&f, &mut hot, &pool, material, NOW + 60000).await;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *held)
        .await
        .unwrap();
    sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
        .bind(attempt)
        .execute(&mut *held)
        .await
        .unwrap();
    f.time.store((NOW + 60000) as i64, Ordering::SeqCst);
    let state = f.state.clone();
    let node = f.node.node_id;
    let waiting = tokio::spawn(async move { no_grant::close_expired(&state, node, attempt).await });
    let observer = f.db.pool().await;
    let mut blocked = false;
    for _ in 0..200 {
        let observed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query='SELECT id FROM nodes WHERE id=$1 FOR UPDATE')").fetch_one(&observer).await.unwrap();
        if observed {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(
        blocked,
        "actual node wait observed before changing authority"
    );
    // Fixture-only issued authorization; no process start or provider call is claimed.
    hot.status = Status::Granted;
    hot.execution_deadline_ms = Some(NOW + 120000);
    hot.grant = Some(aq_session_protocol::ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 60000,
    });
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&mut *held)
        .await
        .unwrap();
    held.commit().await.unwrap();
    assert!(waiting.await.unwrap().is_err());
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held);
    let records: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profiled_never_granted_closures WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(records, 0);
}
