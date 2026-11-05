//! H6 hibernation wake scheduling: a resolved wait schedules exactly one fresh
//! continuation on any eligible live node, never requiring the original node.
use super::waits::{WaitEnv, registered, wait_env};
use super::*;
use aq_server::agent_access::{waits, wakes};
use aq_wait_engine::wire::{self, worker_lifecycle as lc};

fn text(s: &str) -> wire::Text {
    wire::Text::new(s).unwrap()
}
fn art(id: &str) -> wire::ArtifactRef {
    wire::ArtifactRef {
        id: text(id),
        version: text("1"),
        digest: wire::Digest::of(id.as_bytes()),
    }
}
fn receipt(binding: &wire::Binding, role: lc::ArtifactRole, id: &str) -> lc::RetentionReceipt {
    lc::RetentionReceipt {
        receipt_id: Uuid::new_v4(),
        owner: text("aq-git"),
        binding_digest: wire::canonical_digest(binding).unwrap(),
        artifact: art(id),
        role,
        bytes: 1024,
        retained_at_ms: NOW,
        retained_until_ms: NOW + 86_400_000,
    }
}

/// A registered HumanDecision wait whose predecessor parked with retained
/// source and checkpoint, then the decision resolved and H4 recorded intent.
async fn resolved_hibernated_wait() -> (WaitEnv, wire::RegisteredWait) {
    let w = Box::pin(wait_env()).await;
    let f = &w.env.f;
    let request = wire::Request::seal(
        Uuid::new_v4(),
        wire::Command::RegisterWait(w.registration(Uuid::new_v4(), w.condition().await)),
    )
    .unwrap();
    let wait = registered(&w.register(&request).await);
    let directive = wakes::suspend(&f.state, w.node(), w.attempt(), wait.wait.wait_id)
        .await
        .unwrap();
    assert_eq!(directive.binding, w.binding);
    let report = lc::ParkingReport {
        binding: w.binding.clone(),
        continuation: wait.continuation.clone(),
        disposition: lc::SuspensionDisposition::RetainedSuspended,
        stop: lc::StopObservation::ReportedStopped {
            observation_id: Uuid::new_v4(),
            guardian_instance_id: Uuid::new_v4(),
            writer_set_digest: wire::Digest::of(b"writers"),
            writer_count: 0,
            observed_at_ms: NOW,
            proof: art("stop-proof"),
        },
        local_retained_bytes: 0,
        central_receipts: vec![
            receipt(&w.binding, lc::ArtifactRole::Source, "source"),
            receipt(&w.binding, lc::ArtifactRole::Checkpoint, "checkpoint"),
        ],
        effects: lc::EffectsObservation::Reconciled {
            receipt: art("effects"),
        },
        compute_released: true,
        protected_disk_released: false,
        disk_release_directive: None,
    };
    let disposition = wakes::accept_parking(&f.state, w.node(), w.attempt(), &report)
        .await
        .unwrap();
    assert_eq!(disposition, "suspended");
    Box::pin(w.env.service.resolve(
        f.actor,
        &aq_review_protocol::ResolveRequest {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            decision_id: w.decision_id,
            expected_decision_revision: 1,
            choice: aq_review_protocol::Choice::Approve,
            comment: Some("Synthetic".into()),
        },
    ))
    .await
    .unwrap();
    f.time.store((NOW + 61_000) as i64, Ordering::SeqCst);
    waits::reconcile_due(&f.state, 64).await.unwrap();
    (w, wait)
}

async fn enroll_second_node(f: &Fixture) -> Uuid {
    let store = &f.state.store;
    let at = chrono::TimeZone::timestamp_millis_opt(&chrono::Utc, NOW as i64).unwrap();
    store
        .create_enrollment(
            "wake-second",
            f.actor,
            at,
            at + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();
    let node = store
        .redeem_enrollment(
            "wake-second",
            aq_server::store::NewNode {
                name: "wake-second".into(),
                credential_hash: aq_server::tokens::verifier(
                    &aq_server::tokens::new_node_credential(),
                ),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            at,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .execution_register(&aq_worker_protocol::RegisterRequest {
            identity: aq_worker_protocol::NodeIdentity {
                node_id: node.id,
                boot_id: Uuid::new_v4(),
            },
            operation_id: Uuid::new_v4(),
            protocol_version: 2,
            worker_version: "0.1.0".into(),
            capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
        })
        .await
        .unwrap();
    node.id
}

async fn seen(f: &Fixture, node: Uuid, at_ms: u64) {
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(node)
    .bind(at_ms as f64)
    .execute(&f.db.pool().await)
    .await
    .unwrap();
}

#[tokio::test]
async fn wake_schedules_continuation_on_other_node_when_original_offline() {
    let (w, wait) = Box::pin(resolved_hibernated_wait()).await;
    let f = &w.env.f;
    let pool = f.db.pool().await;
    let second = enroll_second_node(f).await;
    let now = NOW + 61_000;
    seen(f, w.node(), now - 600_000).await;
    seen(f, second, now).await;

    let first = wakes::schedule(&f.state, 64).await.unwrap();
    assert_eq!(first.admitted, 1);
    // A retry queries the durable winner instead of launching another.
    let retry = wakes::schedule(&f.state, 64).await.unwrap();
    assert_eq!(retry.admitted, 0);
    let admissions: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_wait_wake_admissions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(admissions, 1);

    let winner = wakes::winner(&f.state, wait.wait.wait_id)
        .await
        .unwrap()
        .expect("one winner");
    assert_eq!(winner.node_id, Some(second));
    assert_eq!(winner.dispatch_generation, 1);
    let a = &winner.attachment;
    wire::Validate::validate(a).unwrap();
    assert_eq!(a.predecessor, w.binding);
    assert_ne!(a.successor.attempt_id, w.binding.attempt_id);
    assert_ne!(a.successor.session_id(), w.binding.session_id());
    assert_eq!(
        a.successor.attempt_generation,
        w.binding.attempt_generation + 1
    );
    assert_eq!(a.successor.lineage_id, w.binding.lineage_id);
    let wire::SessionIdentity::Remote { node_id, .. } = &a.successor.session else {
        panic!("remote predecessor resumes remotely");
    };
    assert_eq!(*node_id, second);
    assert_eq!(a.continuation, wait.continuation);
    // Budgets carry forward: the predecessor's attempt is already spent.
    assert!(
        a.remaining.remaining_failure_retries
            < aq_server::profiled_execution::replacement::MAX_ATTEMPTS
    );
    assert!(
        a.remaining.remaining_elapsed_ms
            < aq_server::profiled_execution::replacement::MAX_LINEAGE_ELAPSED_MS
    );
    assert_eq!(winner.differences["node"]["changed"], json!(true));
    let state: String = sqlx::query_scalar("SELECT state FROM agent_wait_wakes WHERE wait_id=$1")
        .bind(wait.wait.wait_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "admitted");
}

#[tokio::test]
async fn wake_cancelled_task_does_not_resume() {
    let (w, wait) = Box::pin(resolved_hibernated_wait()).await;
    let f = &w.env.f;
    let pool = f.db.pool().await;
    seen(f, w.node(), NOW + 61_000).await;
    sqlx::query("INSERT INTO profiled_cancellations(attempt_id,cancellation_id,binding,reason,authority_revision,recorded_at_ms) VALUES($1,$2,$3,'owner cancelled',1,$4)")
        .bind(w.attempt())
        .bind(Uuid::new_v4())
        .bind(json!({"attempt_id": w.attempt()}))
        .bind((NOW + 61_000) as i64)
        .execute(&pool)
        .await
        .unwrap();
    let out = wakes::schedule(&f.state, 64).await.unwrap();
    assert_eq!((out.admitted, out.cancelled), (0, 1));
    assert!(
        wakes::winner(&f.state, wait.wait.wait_id)
            .await
            .unwrap()
            .is_none()
    );
    let (wake, intent): (String, String) = sqlx::query_as("SELECT w.state,i.state FROM agent_wait_wakes w JOIN agent_wait_resume_intents i USING(intent_id) WHERE w.wait_id=$1")
        .bind(wait.wait.wait_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((wake.as_str(), intent.as_str()), ("cancelled", "retired"));
}
