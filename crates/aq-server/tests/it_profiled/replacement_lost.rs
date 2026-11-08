//! Swarm phase 1, D2 ruling C: a restarted node releases an attempt whose host was lost, with
//! an outside proof (`boot_changed`, `cgroup_teardown`). Real SQL owners; the export, the
//! Started record and the launch clock are synthetic rows.
use crate::common;
use aq_protocol::{LostAttemptRelease, LostReason, StopProof};
use aq_server::{
    execution::{
        authority::hot::{HotState, LaunchOwner},
        profiled::{Status, Stop},
    },
    profiled_execution::{lost, replacement, source_exports},
};
use aq_session_protocol::ProfiledGrant;
use common::{
    native_started::context,
    profiled::{Fixture, NOW},
    source_exports::{setup, synthetic_fact},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const OLD_BOOT: &str = "00000000-0000-4000-8000-0000000000aa";
const NEW_BOOT: &str = "00000000-0000-4000-8000-0000000000bb";

struct Held {
    f: Fixture,
    pool: PgPool,
    attempt: Uuid,
    _dir: tempfile::TempDir,
}
/// A granted attempt whose host was lost: its task held, stop unknown to the server
/// (`replacement_prior_execution_uncertain`), its input export linked and its launch clock
/// known. With `started`, a Started record names the guardian's boot.
async fn held(started: bool) -> Held {
    let (f, request, dir) = setup().await;
    let pool = f.db.pool().await;
    let source_exports::Reservation::Pending(token) =
        source_exports::reserve(&f.state, request.clone())
            .await
            .unwrap()
    else {
        panic!("a fresh reservation")
    };
    let fact = synthetic_fact(&token);
    source_exports::finalize(&f.state, &token, &fact)
        .await
        .unwrap();
    let original = fact.canonical_bytes().unwrap();
    let retention = serde_json::from_value(json!({"request_digest":token.request_digest(),
        "export_operation_id":request.operation_id,
        "inspection":serde_json::from_slice::<Value>(&original).unwrap()["retention_inspection"],
        "observed_at_ms":NOW,"expires_at_ms":NOW+30000}))
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let session = f.profiled.offer.binding.session.clone();
    source_exports::link_locked(&f.state, &mut tx, &session, &token, &fact, &retention)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let attempt = f.profiled.offer.binding.attempt_id;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)")
        .bind(attempt).bind(json!(hot)).execute(&pool).await.unwrap();
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
    // Synthetic history rows, written with the storage guards bypassed.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_launches SET graph_task_observation=jsonb_set(graph_task_observation,'{native_offer}',$2) WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!({"offered":{"clock":{"server_time_ms":NOW - 5_000}}}))
        .execute(&mut *tx)
        .await
        .unwrap();
    if started {
        let bytes = serde_json::to_vec(
            &json!({"observation":{"guardian":{"machine_boot_id":OLD_BOOT}},"synthetic":true}),
        )
        .unwrap();
        sqlx::query("INSERT INTO profiled_started_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,receipt_bytes,before_record_revision) VALUES($1,$2,$3,$4,encode(sha256($4),'hex'),$5,1)")
            .bind(attempt).bind(f.node.node_id).bind(Uuid::new_v4()).bind(&bytes).bind(b"{}".to_vec())
            .execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    Held {
        f,
        pool,
        attempt,
        _dir: dir,
    }
}
/// A synthetic hot projection, written with the storage guards bypassed (a real running
/// native attempt has the Started history these rows stand in for).
async fn set_hot(pool: &PgPool, attempt: Uuid, hot: &HotState) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}
fn rebooted() -> StopProof {
    StopProof::BootChanged {
        old_boot_id: OLD_BOOT.parse().unwrap(),
        new_boot_id: NEW_BOOT.parse().unwrap(),
    }
}
fn torn_down() -> StopProof {
    StopProof::CgroupTeardown {
        invocation_id: "0123456789abcdef0123456789abcdef".into(),
        unit: "aq-node@ms1.service".into(),
    }
}
fn request(attempt: Uuid, proof: StopProof) -> LostAttemptRelease {
    LostAttemptRelease {
        schema_version: 1,
        attempt_id: attempt,
        proof,
    }
}
async fn release(h: &Held, proof: StopProof) -> Result<aq_protocol::LostAttemptReleased, String> {
    let auth = context(&h.f).await;
    (lost::release(&h.f.state, &auth, &request(h.attempt, proof)).await)
        .map_err(|e| format!("{e:?}"))
}
/// Whether the replacement history charges the attempt (an exempt prior is not charged).
async fn charged(h: &Held) -> bool {
    let mut conn = h.pool.acquire().await.unwrap();
    let priors = replacement::load(&mut conn, h.f.selection.lineage.task_id, 1)
        .await
        .unwrap()
        .unwrap();
    !priors[0].exempt
}

/// Each kind fails the attempt with its reason, records its facts, releases the task claim
/// for a retry from the attempt's own retained input, and replays idempotently. A reboot is
/// not charged to the lineage budgets; a crash is.
#[tokio::test]
async fn a_lost_attempt_is_released_per_proof_kind_and_charged_only_after_a_crash() {
    for (proof, reason, charge) in [
        (rebooted(), LostReason::LostAfterReboot, false),
        (torn_down(), LostReason::LostAfterCrash, true),
    ] {
        let h = held(true).await;
        let released = release(&h, proof.clone()).await.unwrap();
        assert_eq!(
            (
                released.reason,
                released.charged,
                released.released,
                released.held.clone()
            ),
            (reason, charge, true, None),
            "{proof:?}"
        );
        let row: (String, Value, String, bool) = sqlx::query_as(
            "SELECT proof_kind,proof,reason,charged FROM lost_attempt_releases WHERE attempt_id=$1",
        )
        .bind(h.attempt)
        .fetch_one(&h.pool)
        .await
        .unwrap();
        assert_eq!(row.0, proof.kind());
        assert_eq!(row.1, serde_json::to_value(&proof).unwrap());
        assert_eq!(row.3, charge);
        let hot: Value =
            sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
                .bind(h.attempt)
                .fetch_one(&h.pool)
                .await
                .unwrap();
        assert_eq!(hot["status"], "finished");
        assert_eq!(
            hot["stop"]["receipt"]["id"],
            format!("aq.lost-attempt-release/{}", h.attempt)
        );
        assert_eq!(hot["retention"]["kind"], "retained");
        assert_eq!(hot["effects"]["kind"], "quiescent");
        let holds: bool =
            sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
                .bind(h.attempt)
                .fetch_one(&h.pool)
                .await
                .unwrap();
        assert!(!holds, "the task claim is released for the retry");
        assert_eq!(charged(&h).await, charge, "{proof:?}");
        // A replay answers the same; another proof for the attempt is refused.
        let again = release(&h, proof.clone()).await.unwrap();
        assert_eq!(again, released);
        let other = if charge { rebooted() } else { torn_down() };
        assert!(
            release(&h, other)
                .await
                .unwrap_err()
                .contains("another_proof")
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM lost_attempt_releases")
            .fetch_one(&h.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}

/// Refused, with nothing recorded: a guardian proof, another node's attempt, an attempt no
/// longer held, and a reboot proof that does not name the guardian's recorded boot (or with
/// no Started record of it).
#[tokio::test]
async fn a_lost_release_is_refused_unless_proven_owned_and_held() {
    let h = held(true).await;
    let e = release(&h, StopProof::Guardian).await.unwrap_err();
    assert!(e.contains("lost_attempt_guardian_proof"), "{e}");
    let wrong_boot = StopProof::BootChanged {
        old_boot_id: Uuid::from_u128(7),
        new_boot_id: NEW_BOOT.parse().unwrap(),
    };
    let e = release(&h, wrong_boot).await.unwrap_err();
    assert!(e.contains("lost_attempt_proof"), "{e}");
    // Another node's attempt (the foreign key is bypassed with the guards).
    let other = Uuid::new_v4();
    let mut tx = h.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE execution_attempts SET node_id=$2 WHERE attempt_id=$1")
        .bind(h.attempt)
        .bind(other)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = release(&h, torn_down()).await.unwrap_err();
    assert!(e.contains("attempt_not_owned"), "{e}");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM lost_attempt_releases")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Not held: its stop is on record.
    let h = held(true).await;
    let mut hot: HotState = serde_json::from_value(
        sqlx::query_scalar::<_, Value>(
            "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1",
        )
        .bind(h.attempt)
        .fetch_one(&h.pool)
        .await
        .unwrap(),
    )
    .unwrap();
    hot.status = Status::Finished;
    hot.stop = Stop::Stopped {
        receipt: common::profiled::artifact("stop", b"stop"),
    };
    set_hot(&h.pool, h.attempt, &hot).await;
    let e = release(&h, torn_down()).await.unwrap_err();
    assert!(e.contains("lost_attempt_not_held"), "{e}");

    // No Started record names the guardian's boot: a reboot cannot be checked.
    let h = held(false).await;
    let e = release(&h, rebooted()).await.unwrap_err();
    assert!(e.contains("lost_attempt_boot_unknown"), "{e}");
}
