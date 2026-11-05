//! Real PostgreSQL persistence; synthetic trusted evidence, no physical host.
use aq_central_authority::{owner::*, *};
use aq_server::test_support::TestDb;
use uuid::Uuid;

#[derive(Clone)]
struct FixtureVerifier;
#[async_trait::async_trait]
impl EvidenceVerifier for FixtureVerifier {
    async fn authorize_plan(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &CentralPlan,
    ) -> OwnerResult<()> {
        Ok(())
    }
    async fn verify_event(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &Run,
        _: &Event,
    ) -> OwnerResult<()> {
        Ok(())
    }
}
fn artifact(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
fn plan() -> CentralPlan {
    let examples: serde_json::Value = serde_json::from_str(include_str!(
        "../../aq-central-authority/artifacts/examples.json"
    ))
    .unwrap();
    let Document::Plan(mut p) = decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
    else {
        panic!()
    };
    p.batch.messages.truncate(1);
    p.batch.through_cursor = 1;
    p.wall_time_ms = 300_000;
    p.max_permit_ms = 120_000;
    p
}
fn clock(ms: u64) -> Clock {
    Clock {
        machine_boot_id: Uuid::from_u128(9),
        boot_ms: ms,
    }
}
fn launch(p: &CentralPlan) -> HostLaunchRef {
    HostLaunchRef {
        binding: p.binding().unwrap(),
        launch_id: Uuid::from_u128(100 + p.generation as u128),
        intent: artifact("launch-intent"),
    }
}
fn observation(p: &CentralPlan) -> HostObservationRef {
    HostObservationRef {
        binding: p.binding().unwrap(),
        launch_id: launch(p).launch_id,
        guardian: artifact("guardian"),
        observation: artifact("observation"),
    }
}
async fn setup() -> (TestDb, SessionOwner<FixtureVerifier>) {
    let db = TestDb::create().await;
    let store = db.store().await;
    drop(store);
    let owner = SessionOwner::new(db.pool().await, FixtureVerifier);
    (db, owner)
}
async fn started(owner: &SessionOwner<FixtureVerifier>, p: &CentralPlan) -> Receipt {
    owner.prepare(p.clone(), clock(1000)).await.unwrap();
    owner
        .start(&p.binding().unwrap(), launch(p), 120_000, clock(1000))
        .await
        .unwrap()
}
fn successor(p: &CentralPlan) -> CentralPlan {
    let mut next = p.clone();
    next.generation += 1;
    if let SessionIdentity::Central {
        session_id,
        generation,
        ..
    } = &mut next.session
    {
        *session_id = Uuid::new_v4();
        *generation = next.generation;
    }
    next.dispatch_id = Uuid::new_v4();
    next.host_operation_id = Uuid::new_v4();
    next.batch.messages[0].message_id = Uuid::new_v4();
    next.batch.messages[0].cursor += 10;
    next.batch.through_cursor += 10;
    next
}

#[tokio::test]
async fn central_owner_reopens_committed_permit_without_new_generation() {
    let (db, owner) = setup().await;
    let p = plan();
    let receipt = started(&owner, &p).await;
    let permit = receipt.snapshot.run.permit().unwrap();
    assert_eq!(
        (
            permit.sequence,
            permit.issued_boot_ms,
            permit.expires_boot_ms
        ),
        (1, 1000, 121_000)
    );
    drop(owner);
    let owner = SessionOwner::new(db.pool().await, FixtureVerifier);
    assert_eq!(
        owner.inspect(p.subject.actor_id()).await.unwrap().unwrap(),
        receipt.snapshot
    );
    assert_eq!(
        owner
            .prepare(p.clone(), clock(2000))
            .await
            .unwrap()
            .run
            .plan(),
        &p
    );
    let replay = owner
        .start(&p.binding().unwrap(), launch(&p), 120_000, clock(2000))
        .await
        .unwrap();
    assert_eq!(replay.decision, Decision::Replayed);
    assert_eq!(replay.snapshot.run.permit(), Some(permit));
    assert!(owner.prepare(successor(&p), clock(2000)).await.is_err());
}

#[tokio::test]
async fn central_owner_serializes_start_and_successor_races() {
    let (_db, owner) = setup().await;
    let p = plan();
    let (a, b) = tokio::join!(
        owner.prepare(p.clone(), clock(1000)),
        owner.prepare(p.clone(), clock(1000))
    );
    assert_eq!(a.unwrap(), b.unwrap());
    let binding = p.binding().unwrap();
    let (a, b) = tokio::join!(
        owner.start(&binding, launch(&p), 120_000, clock(1000)),
        owner.start(&binding, launch(&p), 120_000, clock(1000))
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.snapshot.run, b.snapshot.run);
    assert!(matches!(
        (a.decision, b.decision),
        (Decision::Applied, Decision::Replayed) | (Decision::Replayed, Decision::Applied)
    ));
    owner
        .report(
            &binding,
            Event::Stopped {
                evidence: StopEvidenceRef {
                    host: observation(&p),
                    stopped_writers: artifact("synthetic-stop"),
                },
            },
            clock(2000),
        )
        .await
        .unwrap();
    let next = successor(&p);
    let alternative = successor(&p);
    let (a, b) = tokio::join!(
        owner.prepare(next, clock(2001)),
        owner.prepare(alternative, clock(2001))
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(
        owner
            .inspect(p.subject.actor_id())
            .await
            .unwrap()
            .unwrap()
            .run
            .plan()
            .generation,
        2
    );
    assert_eq!(
        owner
            .generation(p.subject.actor_id(), 1)
            .await
            .unwrap()
            .unwrap()
            .run
            .phase(),
        Phase::StoppedReported
    );
}

#[tokio::test]
async fn central_owner_expiry_and_cancel_survive_refused_renewal_and_running() {
    let (_db, owner) = setup().await;
    let p = plan();
    let binding = p.binding().unwrap();
    started(&owner, &p).await;
    let renewed = owner
        .renew(&binding, 1, 120_000, clock(60_000))
        .await
        .unwrap();
    assert_eq!(
        renewed.snapshot.run.permit().unwrap().expires_boot_ms,
        180_000
    );
    let late = owner
        .renew(&binding, 2, 120_000, clock(180_000))
        .await
        .unwrap();
    assert!(matches!(late.decision, Decision::Refused { .. }));
    assert_eq!(late.snapshot.run.phase(), Phase::StopRequested);
    let running = owner
        .report(
            &binding,
            Event::Running {
                observation: observation(&p),
            },
            clock(180_001),
        )
        .await
        .unwrap();
    assert_eq!(running.snapshot.run.phase(), Phase::StopRequested);
    assert!(owner.prepare(successor(&p), clock(180_002)).await.is_err());
    let fresh = owner.inspect(p.subject.actor_id()).await.unwrap().unwrap();
    assert_eq!(fresh.run.phase(), Phase::StopRequested);
}

#[tokio::test]
async fn central_owner_boot_clock_sequence_deadline_and_identity_fences() {
    let (_db, owner) = setup().await;
    let p = plan();
    let binding = p.binding().unwrap();
    started(&owner, &p).await;
    assert!(
        owner
            .renew(&binding, 1, 120_001, clock(2000))
            .await
            .is_err()
    );
    for (seq, time) in [
        (
            1,
            Clock {
                machine_boot_id: Uuid::new_v4(),
                boot_ms: 2000,
            },
        ),
        (1, clock(999)),
        (2, clock(2000)),
    ] {
        assert!(matches!(
            owner
                .renew(&binding, seq, 120_000, time)
                .await
                .unwrap()
                .decision,
            Decision::Refused { .. }
        ));
    }
    let mut foreign = binding.clone();
    foreign.plan_digest = Digest::of(b"foreign");
    assert!(
        owner
            .renew(&foreign, 1, 120_000, clock(2000))
            .await
            .is_err()
    );
    owner
        .renew(&binding, 1, 120_000, clock(120_000))
        .await
        .unwrap();
    let last = owner
        .renew(&binding, 2, 120_000, clock(230_000))
        .await
        .unwrap();
    assert_eq!(last.snapshot.run.permit().unwrap().expires_boot_ms, 301_000);
    owner
        .report(
            &binding,
            Event::Cancel {
                reason: CancelReason::Operator,
            },
            clock(230_001),
        )
        .await
        .unwrap();
    let refused = owner
        .renew(&binding, 3, 120_000, clock(230_002))
        .await
        .unwrap();
    assert!(matches!(refused.decision, Decision::Refused { .. }));
    assert_eq!(
        refused.snapshot.run.cancellation(),
        Some(CancelReason::Operator)
    );
}

#[tokio::test]
async fn central_owner_retained_history_blocks_old_mail_and_cross_role_ids() {
    let (_db, owner) = setup().await;
    let p = plan();
    let binding = p.binding().unwrap();
    started(&owner, &p).await;
    owner
        .report(
            &binding,
            Event::StopUncertain {
                observation: observation(&p),
            },
            clock(2000),
        )
        .await
        .unwrap();
    assert!(owner.prepare(successor(&p), clock(2001)).await.is_err());
    owner
        .report(
            &binding,
            Event::Stopped {
                evidence: StopEvidenceRef {
                    host: observation(&p),
                    stopped_writers: artifact("synthetic-stop"),
                },
            },
            clock(2002),
        )
        .await
        .unwrap();
    let p2 = successor(&p);
    owner.prepare(p2.clone(), clock(2003)).await.unwrap();
    owner
        .report(
            &p2.binding().unwrap(),
            Event::Cancel {
                reason: CancelReason::Operator,
            },
            clock(2004),
        )
        .await
        .unwrap();
    let mut p3 = successor(&p2);
    p3.batch = p.batch.clone();
    assert!(owner.prepare(p3, clock(2005)).await.is_err());
    let mut p3 = successor(&p2);
    p3.host_operation_id = p.dispatch_id;
    assert!(owner.prepare(p3, clock(2005)).await.is_err());
    let mut p3 = successor(&p2);
    p3.batch.messages[0].cursor = p.batch.messages[0].cursor;
    assert!(owner.prepare(p3, clock(2005)).await.is_err());
    let valid = successor(&p2);
    assert_eq!(
        owner
            .prepare(valid, clock(2005))
            .await
            .unwrap()
            .run
            .plan()
            .generation,
        3
    );
}

struct DenyVerifier;
#[async_trait::async_trait]
impl EvidenceVerifier for DenyVerifier {
    async fn authorize_plan(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &CentralPlan,
    ) -> OwnerResult<()> {
        Err(OwnerError::Evidence)
    }
    async fn verify_event(
        &self,
        _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        _: &Run,
        _: &Event,
    ) -> OwnerResult<()> {
        Err(OwnerError::Evidence)
    }
}
#[tokio::test]
async fn central_owner_rejects_unverified_admission_and_stop() {
    let (db, owner) = setup().await;
    let p = plan();
    let denied = SessionOwner::new(db.pool().await, DenyVerifier);
    assert!(denied.prepare(p.clone(), clock(1000)).await.is_err());
    assert!(owner.inspect(p.subject.actor_id()).await.unwrap().is_none());
    started(&owner, &p).await;
    assert!(
        denied
            .report(
                &p.binding().unwrap(),
                Event::Stopped {
                    evidence: StopEvidenceRef {
                        host: observation(&p),
                        stopped_writers: artifact("forged")
                    }
                },
                clock(2000)
            )
            .await
            .is_err()
    );
    assert_eq!(
        owner
            .inspect(p.subject.actor_id())
            .await
            .unwrap()
            .unwrap()
            .run
            .phase(),
        Phase::StartCommitted
    );
}

#[tokio::test]
async fn central_owner_concurrent_renewal_has_one_winner_and_no_shorter_permit() {
    let (_db, owner) = setup().await;
    let p = plan();
    started(&owner, &p).await;
    let binding = p.binding().unwrap();
    let short = owner.renew(&binding, 1, 1000, clock(2000)).await.unwrap();
    assert_eq!(
        short.decision,
        Decision::Refused {
            reason: Refusal::Deadline
        }
    );
    assert_eq!(short.snapshot.run.permit().unwrap().sequence, 1);
    let (a, b) = tokio::join!(
        owner.renew(&binding, 1, 120_000, clock(60_000)),
        owner.renew(&binding, 1, 120_000, clock(60_000))
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(matches!(
        (a.decision, b.decision),
        (
            Decision::Applied,
            Decision::Refused {
                reason: Refusal::Sequence
            }
        ) | (
            Decision::Refused {
                reason: Refusal::Sequence
            },
            Decision::Applied
        )
    ));
    assert_eq!(
        owner
            .inspect(p.subject.actor_id())
            .await
            .unwrap()
            .unwrap()
            .run
            .permit()
            .unwrap()
            .sequence,
        2
    );
}

#[tokio::test]
async fn central_owner_historical_rows_and_identity_cannot_be_rewritten() {
    let (db, owner) = setup().await;
    let p = plan();
    owner.prepare(p.clone(), clock(1000)).await.unwrap();
    owner
        .report(
            &p.binding().unwrap(),
            Event::Cancel {
                reason: CancelReason::Operator,
            },
            clock(1001),
        )
        .await
        .unwrap();
    let old = owner
        .generation(p.subject.actor_id(), 1)
        .await
        .unwrap()
        .unwrap();
    owner.prepare(successor(&p), clock(1002)).await.unwrap();
    let pool = db.pool().await;
    for sql in [
        "UPDATE central_session_generations SET revision=revision+1 WHERE generation=1",
        "DELETE FROM central_session_generations WHERE generation=1",
        "TRUNCATE central_session_generations",
        "UPDATE central_session_heads SET authority_id='00000000-0000-0000-0000-000000000099'",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err());
    }
    assert_eq!(
        owner
            .generation(p.subject.actor_id(), 1)
            .await
            .unwrap()
            .unwrap(),
        old
    );
}

#[tokio::test]
async fn central_owner_reused_conversation_for_new_actor_is_typed_conflict() {
    let (_db, owner) = setup().await;
    let p = plan();
    owner.prepare(p.clone(), clock(1000)).await.unwrap();
    let mut other = p.clone();
    let CentralSubject::CentralConversation { actor_id, .. } = &mut other.subject;
    *actor_id = Uuid::from_u128(0xa11ce);
    assert_ne!(other.subject.actor_id(), p.subject.actor_id());
    assert_eq!(other.subject.conversation_id(), p.subject.conversation_id());
    assert!(matches!(
        owner.prepare(other.clone(), clock(1000)).await,
        Err(OwnerError::Conflict)
    ));
    // The refusal leaves no partial head for the new actor and keeps the owner.
    assert!(
        owner
            .inspect(other.subject.actor_id())
            .await
            .unwrap()
            .is_none()
    );
    assert!(owner.inspect(p.subject.actor_id()).await.unwrap().is_some());
}

/// Read-only inspection takes no row lock, so a writer holding the head row
/// (e.g. a concurrent `start`/`report`) never blocks `dispatch::live`.
#[tokio::test]
async fn inspect_reads_without_locking_the_session_head() {
    let (db, owner) = setup().await;
    let p = plan();
    let receipt = started(&owner, &p).await;
    let actor = p.subject.actor_id();
    let pool = db.pool().await;
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM central_session_heads WHERE actor_id=$1 FOR UPDATE")
        .bind(actor)
        .execute(&mut *writer)
        .await
        .unwrap();
    let wait = std::time::Duration::from_millis(1000);
    let inspected = tokio::time::timeout(wait, owner.inspect(actor))
        .await
        .expect("inspect must not wait on the head row lock")
        .unwrap()
        .unwrap();
    assert_eq!(inspected, receipt.snapshot);
    let generation = tokio::time::timeout(wait, owner.generation(actor, 1))
        .await
        .expect("generation reads must not wait on the head row lock")
        .unwrap()
        .unwrap();
    assert_eq!(generation, receipt.snapshot);
    writer.rollback().await.unwrap();
}
