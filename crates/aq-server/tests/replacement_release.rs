//! DB end-to-end qualification of failed-attempt replacement release: an
//! actual profiled attempt row with hot authority, released through
//! `replacement::release`, and the storage guards that refuse uncertain or
//! accepted predecessors. Synthetic owner receipts; no guardian or HTTP path.
mod common;
use aq_server::{
    error::AppError,
    execution::{
        authority::hot::{HotState, LaunchOwner},
        profiled::{Effects, Retention, Status, Stop},
    },
    profiled_execution::replacement::{self, Assessment, Replacement},
};
use aq_session_protocol::ProfiledGrant;
use common::profiled::{Fixture, NOW, artifact, fixture, insert_offered};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

const RELEASE_INSERT: &str = "INSERT INTO profiled_replacement_releases(attempt_id,task_id,reason,stop_receipt_digest,retained_source_receipt_digest,effect_reconciliation_receipt_digest,attempts_started,recorded_at_ms) VALUES($1,$2,'failed_attempt_replacement',$3,$4,$5,1,$6)";

async fn set_hot(pool: &PgPool, attempt: Uuid, hot: &HotState) {
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(pool)
        .await
        .unwrap();
}

/// A held, granted, still-running profiled attempt offered at `NOW`.
async fn granted(f: &Fixture) -> (Uuid, PgPool, HotState) {
    let attempt = insert_offered(f, NOW).await;
    let pool = f.db.pool().await;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(hot)).execute(&pool).await.unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    set_hot(&pool, attempt, &hot).await;
    hot.status = Status::Granted;
    hot.stop = Stop::Running;
    hot.execution_deadline_ms = Some(NOW + 60_000);
    hot.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 1_000,
    });
    set_hot(&pool, attempt, &hot).await;
    (attempt, pool, hot)
}

/// Drives the granted attempt to a failed, stopped, retained, quiescent end.
async fn fail(pool: &PgPool, attempt: Uuid, hot: &mut HotState) {
    hot.status = Status::Cancelling;
    hot.revoked = true;
    set_hot(pool, attempt, hot).await;
    hot.status = Status::Finished;
    hot.stop = Stop::Stopped {
        receipt: artifact("synthetic-stop-owner-receipt", b"synthetic-stop"),
    };
    hot.retention = Retention::Retained {
        receipt: artifact("synthetic-source-owner-receipt", b"synthetic-source"),
    };
    hot.effects = Effects::Quiescent {
        receipt: artifact("synthetic-effect-owner-receipt", b"synthetic-effects"),
    };
    set_hot(pool, attempt, hot).await;
}

async fn holds_task(pool: &PgPool, attempt: Uuid) -> bool {
    sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
        .bind(attempt)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn releases(pool: &PgPool, attempt: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM profiled_replacement_releases WHERE attempt_id=$1")
        .bind(attempt)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Direct storage insert with the attempt's own hot receipt digests.
async fn direct_release(pool: &PgPool, attempt: Uuid, task: Uuid) -> Result<(), sqlx::Error> {
    let digest = |kind: &str| {
        let path = format!("{{{kind},receipt,digest}}");
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT state#>>$2::TEXT[] FROM profiled_authority_hot WHERE attempt_id=$1",
        )
        .bind(attempt)
        .bind(path)
        .fetch_one(pool)
    };
    let stop = digest("stop").await.unwrap();
    let retained = digest("retention").await.unwrap();
    let effects = digest("effects").await.unwrap();
    let fallback = || Some("0".repeat(64));
    sqlx::query(RELEASE_INSERT)
        .bind(attempt)
        .bind(task)
        .bind(stop.or_else(fallback))
        .bind(retained.or_else(fallback))
        .bind(effects.or_else(fallback))
        .bind(NOW as i64)
        .execute(pool)
        .await
        .map(|_| ())
}

fn replacement_of(assessment: Assessment) -> Replacement {
    match assessment {
        Assessment::Replace(r) => *r,
        other => panic!("expected a replacement, got {other:?}"),
    }
}

#[tokio::test]
async fn failed_profiled_attempt_is_released_once_and_its_claim_stays_released() {
    let f = fixture().await;
    let task = f.selection.lineage.task_id;
    let (attempt, pool, mut hot) = granted(&f).await;
    fail(&pool, attempt, &mut hot).await;
    assert!(holds_task(&pool, attempt).await);

    let assessed = replacement_of(replacement::assess_task(&f.state, task, NOW).await.unwrap());
    assert_eq!(assessed.predecessor, attempt);
    assert_eq!(assessed.attempts_started, 1);
    assert!(assessed.needs_release);
    let Stop::Stopped { receipt } = &hot.stop else {
        unreachable!()
    };
    assert_eq!(&assessed.stop_receipt, receipt);

    // A mismatched receipt digest is refused by storage even for a valid attempt.
    let wrong = "f".repeat(64);
    assert!(
        sqlx::query(RELEASE_INSERT)
            .bind(attempt)
            .bind(task)
            .bind(&wrong)
            .bind(assessed.retained_source_receipt.digest.as_str())
            .bind(assessed.effect_reconciliation_receipt.digest.as_str())
            .bind(NOW as i64)
            .execute(&pool)
            .await
            .is_err()
    );

    replacement::release(&f.state, task, &assessed)
        .await
        .unwrap();
    assert!(!holds_task(&pool, attempt).await, "holds_task flips");
    assert_eq!(releases(&pool, attempt).await, 1);
    let (reason, started): (String, i32) = sqlx::query_as(
        "SELECT reason,attempts_started FROM profiled_replacement_releases WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (reason.as_str(), started),
        ("failed_attempt_replacement", 1)
    );

    // The released predecessor is still the lineage head but needs no release.
    let after = replacement_of(replacement::assess_task(&f.state, task, NOW).await.unwrap());
    assert_eq!(after.predecessor, attempt);
    assert!(!after.needs_release);
    // Replaying the stale assessment is idempotent.
    replacement::release(&f.state, task, &assessed)
        .await
        .unwrap();
    replacement::release(&f.state, task, &after).await.unwrap();
    assert_eq!(releases(&pool, attempt).await, 1);

    // The released claim cannot be reacquired and the release is immutable.
    assert!(
        sqlx::query("UPDATE execution_attempts SET holds_task=true WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err()
    );
    for sql in [
        "UPDATE profiled_replacement_releases SET recorded_at_ms=recorded_at_ms+1 WHERE attempt_id=$1",
        "DELETE FROM profiled_replacement_releases WHERE attempt_id=$1",
    ] {
        assert!(sqlx::query(sql).bind(attempt).execute(&pool).await.is_err());
    }
    assert_eq!(releases(&pool, attempt).await, 1);
}

#[tokio::test]
async fn uncertain_predecessor_is_never_released() {
    let f = fixture().await;
    let task = f.selection.lineage.task_id;
    let (attempt, pool, _) = granted(&f).await;
    assert_eq!(
        replacement::assess_task(&f.state, task, NOW).await.unwrap(),
        Assessment::Blocked("replacement_prior_execution_uncertain")
    );
    // Even a forged replacement is re-assessed under the locks and refused.
    let forged = Replacement {
        predecessor: attempt,
        attempts_started: 1,
        elapsed_ms: 0,
        sandbox_ms: 60_000,
        stop_receipt: artifact("synthetic-stop-owner-receipt", b"synthetic-stop"),
        retained_source_receipt: artifact("synthetic-source-owner-receipt", b"synthetic-source"),
        effect_reconciliation_receipt: artifact(
            "synthetic-effect-owner-receipt",
            b"synthetic-effects",
        ),
        needs_release: true,
    };
    assert!(matches!(
        replacement::release(&f.state, task, &forged).await,
        Err(AppError::Conflict(_))
    ));
    assert!(direct_release(&pool, attempt, task).await.is_err());
    assert!(
        sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err(),
        "no release record means the claim stays held"
    );
    assert!(holds_task(&pool, attempt).await);
    assert_eq!(releases(&pool, attempt).await, 0);
}

#[tokio::test]
async fn accepted_predecessor_is_never_released() {
    let f = fixture().await;
    let task = f.selection.lineage.task_id;
    let (attempt, pool, mut hot) = granted(&f).await;
    fail(&pool, attempt, &mut hot).await;
    let assessed = replacement_of(replacement::assess_task(&f.state, task, NOW).await.unwrap());

    // Acceptance lands after assessment. Deliberate synthetic evaluation in a
    // disposable test database: replica role skips the settlement owner's
    // foreign keys and guards, never a runtime path.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let hex = "a".repeat(64);
    sqlx::query("INSERT INTO profiled_settlement_evaluations(operation_id,attempt_id,report_id,source_operation_id,decision_id,actor_id,actor_identity_digest,request,request_digest,receipt,receipt_digest,evidence,disposition,created_at_ms,release_id,reserved_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,'{}',$7,'{}',$7,'{}','accepted',$8,$9,196608)")
        .bind(Uuid::new_v4()).bind(attempt).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(f.actor).bind(&hex).bind(NOW as i64).bind(Uuid::new_v4())
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        replacement::assess_task(&f.state, task, NOW).await.unwrap(),
        Assessment::Blocked("task_already_settled_accepted")
    );
    assert!(matches!(
        replacement::release(&f.state, task, &assessed).await,
        Err(AppError::Conflict(_))
    ));
    let refused = direct_release(&pool, attempt, task).await.unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("replacement release requires stopped, retained, quiescent, unaccepted"),
        "{refused}"
    );
    assert!(holds_task(&pool, attempt).await);
    assert_eq!(releases(&pool, attempt).await, 0);
}
