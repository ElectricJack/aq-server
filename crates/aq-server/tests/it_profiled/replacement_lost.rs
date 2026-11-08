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
    profiled::{Fixture, NOW, insert_typed},
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
    /// The verified input export and its retention fact, linked again by a successor.
    export: (
        source_exports::ReservedExport,
        aq_server::source_retention::profiled_base::VerifiedExport,
        Value,
    ),
    _dir: tempfile::TempDir,
}
/// A granted attempt whose host was lost: its task held, stop unknown to the server
/// (`replacement_prior_execution_uncertain`), its input export linked (unless `link` is
/// false) and its launch clock known. With `started`, a Started record names the guardian's
/// boot.
async fn held_with(started: bool, link: bool) -> Held {
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
    let retention = json!({"request_digest":token.request_digest(),
        "export_operation_id":request.operation_id,
        "inspection":serde_json::from_slice::<Value>(&original).unwrap()["retention_inspection"],
        "observed_at_ms":NOW,"expires_at_ms":NOW+30000});
    let attempt = f.profiled.offer.binding.attempt_id;
    let mut h = Held {
        f,
        pool,
        attempt,
        export: (token, fact, retention),
        _dir: dir,
    };
    if link {
        link_export(&h).await;
    }
    running(&h, NOW - 9_000, started).await;
    h.attempt = attempt;
    h
}
async fn held(started: bool) -> Held {
    held_with(started, true).await
}
async fn link_export(h: &Held) {
    let mut tx = h.pool.begin().await.unwrap();
    let session = h.f.profiled.offer.binding.session.clone();
    let retention = serde_json::from_value(h.export.2.clone()).unwrap();
    source_exports::link_locked(
        &h.f.state,
        &mut tx,
        &session,
        &h.export.0,
        &h.export.1,
        &retention,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}
/// Makes the fixture's current attempt granted and running (offered at `offered_at_ms`).
async fn running(h: &Held, offered_at_ms: u64, started: bool) {
    let (f, pool) = (&h.f, &h.pool);
    let attempt = f.profiled.offer.binding.attempt_id;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)")
        .bind(attempt).bind(json!(hot)).execute(pool).await.unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    set_hot(pool, attempt, &hot).await;
    hot.status = Status::Granted;
    hot.stop = Stop::Running;
    hot.execution_deadline_ms = Some(NOW + 60_000);
    hot.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 1_000,
    });
    set_hot(pool, attempt, &hot).await;
    // Synthetic history rows, written with the storage guards bypassed.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_launches SET graph_task_observation=jsonb_set(graph_task_observation,'{native_offer}',$2) WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!({"offered":{"clock":{"server_time_ms":offered_at_ms}}}))
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
}
/// The task's next attempt (after the current one was released), sharing its input export,
/// granted and running.
async fn successor(h: &mut Held, generation: u64, offered_at_ms: u64) {
    let mut spec: aq_admission::execution_spec::ProfiledExecutionSpec = serde_json::from_value(
        sqlx::query_scalar::<_, Value>(
            "SELECT execution_spec FROM profiled_launches WHERE attempt_id=$1",
        )
        .bind(h.attempt)
        .fetch_one(&h.pool)
        .await
        .unwrap(),
    )
    .unwrap();
    let f = &mut h.f;
    let attempt = Uuid::new_v4();
    let session = aq_session_protocol::SessionIdentity::Remote {
        node_id: f.node.node_id,
        boot_id: f.node.boot_id,
        attempt_id: attempt,
        assignment_generation: generation,
        session_id: Uuid::new_v4(),
    };
    let offer = &mut f.profiled.offer;
    offer.binding.session = session.clone();
    offer.binding.attempt_id = attempt;
    offer.binding.assignment_generation = generation;
    offer.binding.offer_id = Uuid::new_v4();
    offer.manifest.bindings.session = session;
    offer.manifest.bindings.attempt_id = attempt;
    offer.manifest.bindings.assignment_generation = generation;
    offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&offer.manifest).unwrap();
    spec.lifecycle = aq_admission::transport::lifecycle_binding(offer).unwrap();
    offer.binding.execution_spec_digest = spec.artifact().unwrap().digest;
    f.decision = Uuid::new_v4();
    insert_typed(f, &spec).await;
    link_export(h).await;
    running(h, offered_at_ms, false).await;
    h.attempt = attempt;
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
/// Runs `sql` (one bound attempt id) with the storage guards bypassed.
async fn raw(pool: &PgPool, sql: &str, attempt: Uuid) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(sql)
        .bind(attempt)
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
async fn releases(h: &Held) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM lost_attempt_releases")
        .fetch_one(&h.pool)
        .await
        .unwrap()
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

/// #117 review I1 and its re-review: three crash releases of one lineage. The first two are
/// retried; the third exhausts the retry budget, so B4 holds the task: the release answers
/// `held` with the reason, records it, and the node is free (not busy, not a 409).
#[tokio::test]
async fn a_budget_held_lost_attempt_holds_the_task_not_the_node() {
    let mut h = held(false).await;
    let auth = context(&h.f).await;
    for (generation, offered) in [(2, NOW - 6_000), (3, NOW - 3_000)] {
        let released = release(&h, torn_down()).await.unwrap();
        assert!(released.released, "{released:?}");
        successor(&mut h, generation, offered).await;
    }
    let held = release(&h, torn_down()).await.unwrap();
    assert_eq!(
        (held.charged, held.released, held.held.as_deref()),
        (true, false, Some("lineage_retry_budget_exhausted"))
    );
    let reason: Option<String> =
        sqlx::query_scalar("SELECT held_reason FROM lost_attempt_releases WHERE attempt_id=$1")
            .bind(h.attempt)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("lineage_retry_budget_exhausted"));
    let busy = aq_server::test_support::node_busy(&h.f.state, &auth).await;
    assert_eq!(
        busy.map_err(|e| format!("{e:?}")),
        Ok(false),
        "free, not a 409"
    );
    // A replay answers the same held state.
    assert_eq!(release(&h, torn_down()).await.unwrap(), held);
}

/// #117 review I2: refused, with nothing recorded, when the agent made an effect, when a
/// result or stop is on record, and when no verified input export is linked.
#[tokio::test]
async fn a_lost_release_is_refused_with_effects_a_record_or_no_source() {
    let h = held(true).await;
    raw(&h.pool, "INSERT INTO agent_graph_operations(operation_id,attempt_id,node_id,request,payload_digest,mapped,policy,status,phase,created_at_ms,retained_bytes) SELECT gen_random_uuid(),$1,node_id,'{}','x','{}','{}','{}','queued',1,2 FROM execution_attempts WHERE attempt_id=$1", h.attempt).await;
    let e = release(&h, torn_down()).await.unwrap_err();
    assert!(e.contains("lost_attempt_effects_present"), "{e}");
    let h = held(true).await;
    raw(&h.pool, "INSERT INTO profiled_stopped_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,proof_bytes,proof_digest,receipt_bytes,before_record_revision,before_grant_revision) SELECT $1,node_id,gen_random_uuid(),'\x7b7d',encode(sha256('\x7b7d'),'hex'),'\x7b7d',encode(sha256('\x7b7d'),'hex'),'\x7b7d',1,1 FROM execution_attempts WHERE attempt_id=$1", h.attempt).await;
    let e = release(&h, torn_down()).await.unwrap_err();
    assert!(e.contains("lost_attempt_reported"), "{e}");
    let h = held_with(true, false).await;
    let e = release(&h, torn_down()).await.unwrap_err();
    assert!(e.contains("lost_attempt_source_unknown"), "{e}");
    assert_eq!(releases(&h).await, 0);
}

/// #117 review I2: the widened stopped-projection guard still refuses a stop that names a
/// lost-attempt release when no release is recorded, or with another digest.
#[tokio::test]
async fn the_stopped_guard_refuses_an_unrecorded_or_mismatched_lost_release() {
    let h = held(true).await;
    let stop = |digest: &str| {
        json!({"kind":"stopped","receipt":{"id":format!("aq.lost-attempt-release/{}", h.attempt),
            "version":"1","digest":digest}})
    };
    let update = |digest: String| {
        let pool = h.pool.clone();
        let attempt = h.attempt;
        let stop = stop(&digest);
        async move {
            sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(jsonb_set(state,'{stop}',$2),'{status}','\"finished\"') WHERE attempt_id=$1")
                .bind(attempt).bind(stop).execute(&pool).await.map(drop).map_err(|e| e.to_string())
        }
    };
    let digest = "a".repeat(64);
    let e = update(digest.clone()).await.unwrap_err();
    assert!(e.contains("native stopped projection requires"), "{e}");
    sqlx::query("INSERT INTO lost_attempt_releases(attempt_id,node_id,proof_kind,proof,reason,charged,receipt_digest,recorded_at) SELECT $1,node_id,'cgroup_teardown','{}','lost_after_crash',TRUE,$2,now() FROM execution_attempts WHERE attempt_id=$1")
        .bind(h.attempt).bind(&digest).execute(&h.pool).await.unwrap();
    let e = update("b".repeat(64)).await.unwrap_err();
    assert!(e.contains("native stopped projection requires"), "{e}");
    update(digest).await.unwrap();
}

/// #117 review M2: an attempt never granted is refused (it is closed through the launch
/// closure), and the ownership check comes before any replay (M1).
#[tokio::test]
async fn a_lost_release_refuses_an_ungranted_attempt_and_checks_ownership_first() {
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
    hot.status = Status::Preparing;
    hot.grant = None;
    set_hot(&h.pool, h.attempt, &hot).await;
    let e = release(&h, torn_down()).await.unwrap_err();
    assert!(e.contains("lost_attempt_not_granted"), "{e}");
    // A released attempt moved to another node: the replay is refused as not owned.
    let h = held(true).await;
    release(&h, torn_down()).await.unwrap();
    raw(
        &h.pool,
        "UPDATE execution_attempts SET node_id=gen_random_uuid() WHERE attempt_id=$1",
        h.attempt,
    )
    .await;
    let e = release(&h, torn_down()).await.unwrap_err();
    assert!(e.contains("attempt_not_owned"), "{e}");
}
