//! aq-machine-control store: machine keys, slot nodes, desired state, handoffs,
//! revocation and the owner's availability switch (spec 0007 phase 1).
use crate::common::profiled::{NOW, artifact, fixture, insert_offered};
use aq_machine_control::{self as mc, Availability, DesiredState, Error, HandoffReason, NewSlot};
use aq_server::execution::{
    authority::hot::{HotState, LaunchOwner},
    profiled::{Effects, Retention, Status, Stop},
};
use aq_server::test_support::TestDb;
use aq_session_protocol::ProfiledGrant;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

async fn owner(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,$2,'owner',$3)")
        .bind(id)
        .bind(format!("{id}@localhost"))
        .bind(now())
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn node(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,$2,$3,'0.1.0',1,$4)")
        .bind(id)
        .bind(name)
        .bind(format!("hash-{id}"))
        .bind(now())
        .execute(pool)
        .await
        .unwrap();
    id
}

fn hashes(n: usize) -> Vec<String> {
    (0..n).map(|_| format!("hash-{}", Uuid::new_v4())).collect()
}

fn new_slot() -> NewSlot<'static> {
    NewSlot {
        base_name: "desk",
        worker_version: "0.1.0",
        protocol_version: 1,
    }
}

/// A machine "desk" with `slots` live slot nodes; returns the node ids by slot.
async fn machine(pool: &PgPool, slots: u32) -> Vec<Uuid> {
    let first = node(pool, "desk").await;
    let mut c = pool.acquire().await.unwrap();
    let caller = mc::claim_key(&mut c, first, "desk").await.unwrap();
    let mut ids = vec![first];
    if slots > 1 {
        let minted = mc::mint_slots(&mut c, &caller, slots, &new_slot(), &hashes(7), now())
            .await
            .unwrap();
        ids.extend(minted.iter().map(|m| m.node_id));
    }
    ids
}

#[tokio::test]
async fn legacy_node_has_no_machine_and_accepts_work() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let n = node(&pool, "legacy").await;
    let mut c = pool.acquire().await.unwrap();
    let m = mc::machine_of(&mut c, n).await.unwrap();
    assert_eq!(m.machine_key, None);
    assert!(mc::accepts_new_work(&mut c, n).await.unwrap());
    assert!(mc::node_services(&mut c, &[n]).await.unwrap().is_empty());
}

#[tokio::test]
async fn claim_key_then_mint_slots_once() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let first = node(&pool, "desk").await;
    let mut c = pool.acquire().await.unwrap();
    let caller = mc::claim_key(&mut c, first, "desk").await.unwrap();
    assert_eq!(caller.slot, Some(1));
    let a = mc::mint_slots(&mut c, &caller, 3, &new_slot(), &hashes(2), now())
        .await
        .unwrap();
    assert_eq!(
        a.iter().map(|m| (m.slot, m.created)).collect::<Vec<_>>(),
        [(2, true), (3, true)]
    );
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM nodes WHERE slot>1 ORDER BY slot")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(names, ["desk#2", "desk#3"]);
    let b = mc::mint_slots(&mut c, &caller, 3, &new_slot(), &hashes(2), now())
        .await
        .unwrap();
    assert_eq!(
        a.iter().map(|m| m.node_id).collect::<Vec<_>>(),
        b.iter().map(|m| m.node_id).collect::<Vec<_>>()
    );
    assert!(b.iter().all(|m| !m.created));
    let sibling = mc::machine_of(&mut c, a[0].node_id).await.unwrap();
    assert!(matches!(
        mc::mint_slots(&mut c, &sibling, 3, &new_slot(), &hashes(2), now()).await,
        Err(Error::Invalid("not_slot_one"))
    ));
    assert!(matches!(
        mc::mint_slots(&mut c, &caller, 9, &new_slot(), &hashes(8), now()).await,
        Err(Error::Invalid("slots_out_of_range"))
    ));
}

#[tokio::test]
async fn claim_key_refuses_a_key_held_by_another_live_machine() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    machine(&pool, 1).await;
    let other = node(&pool, "other").await;
    let mut c = pool.acquire().await.unwrap();
    assert!(matches!(
        mc::claim_key(&mut c, other, "desk").await,
        Err(Error::KeyInUse)
    ));
    mc::revoke_machine(&mut c, "desk", now()).await.unwrap();
    assert!(mc::claim_key(&mut c, other, "desk").await.is_ok());
}

#[tokio::test]
async fn set_desired_is_compare_and_swap() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    machine(&pool, 1).await;
    let who = owner(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    let r = mc::set_desired(
        &mut c,
        "desk",
        DesiredState::Stopped,
        1,
        who,
        Uuid::new_v4(),
        now(),
    )
    .await
    .unwrap();
    assert_eq!((r.desired, r.generation), (DesiredState::Stopped, 2));
    assert!(matches!(
        mc::set_desired(
            &mut c,
            "desk",
            DesiredState::Running,
            1,
            who,
            Uuid::new_v4(),
            now()
        )
        .await,
        Err(Error::Conflict { generation: 2 })
    ));
    let r = mc::set_desired(
        &mut c,
        "desk",
        DesiredState::Running,
        2,
        who,
        Uuid::new_v4(),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(r.generation, 3);
}

fn set_hot_sql() -> &'static str {
    "UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1"
}

async fn set_hot(pool: &PgPool, attempt: Uuid, hot: &HotState) {
    sqlx::query(set_hot_sql())
        .bind(attempt)
        .bind(json!(hot))
        .execute(pool)
        .await
        .unwrap();
}

/// A held, granted, running profiled attempt on the fixture's node.
pub(crate) async fn granted(f: &crate::common::profiled::Fixture) -> (Uuid, PgPool, HotState) {
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

#[tokio::test]
async fn request_handoffs_selects_running_attempts_only() {
    let f = fixture().await;
    let (attempt, pool, mut hot) = granted(&f).await;
    let who = f.node.node_id;
    let mut c = pool.acquire().await.unwrap();
    mc::claim_key(&mut c, who, "desk").await.unwrap();
    assert_eq!(
        mc::request_handoffs(&mut c, "desk", now()).await.unwrap(),
        [attempt]
    );
    assert_eq!(
        mc::request_handoffs(&mut c, "desk", now()).await.unwrap(),
        [attempt]
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM attempt_handoffs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(
        mc::pending_handoffs(&mut c, "desk").await.unwrap(),
        [attempt]
    );
    assert_eq!(
        mc::pending_counts(&mut c, &["desk".into()]).await.unwrap()["desk"],
        1
    );
    // Once the attempt has stopped it is no longer pending or selected.
    hot.status = Status::Cancelling;
    hot.revoked = true;
    set_hot(&pool, attempt, &hot).await;
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
    set_hot(&pool, attempt, &hot).await;
    assert!(
        mc::pending_handoffs(&mut c, "desk")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        mc::request_handoffs(&mut c, "desk", now())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn record_handoff_requires_the_owning_node() {
    let f = fixture().await;
    let (attempt, pool, _) = granted(&f).await;
    let other = node(&pool, "other").await;
    let mut c = pool.acquire().await.unwrap();
    assert!(matches!(
        mc::record_handoff(&mut c, other, attempt, HandoffReason::ServiceStop, now()).await,
        Err(Error::Invalid("attempt_not_owned"))
    ));
    mc::record_handoff(
        &mut c,
        f.node.node_id,
        attempt,
        HandoffReason::ServiceStop,
        now(),
    )
    .await
    .unwrap();
    let row = mc::handoff_of(&mut c, attempt).await.unwrap().unwrap();
    assert_eq!(row.reason, HandoffReason::ServiceStop);
}

/// Review I1(a): a node-posted owner_stop counts only while the machine is
/// desired stopped by a stop requested before the handoff; else it is a restart.
#[tokio::test]
async fn node_owner_stop_needs_a_requested_stop() {
    let f = fixture().await;
    let (attempt, pool, _) = granted(&f).await;
    let (me, who) = (f.node.node_id, owner(&pool).await);
    let mut c = pool.acquire().await.unwrap();
    mc::claim_key(&mut c, me, "desk").await.unwrap();
    let later = now() + chrono::Duration::minutes(5);
    async fn post(
        c: &mut sqlx::PgConnection,
        me: Uuid,
        attempt: Uuid,
        at: DateTime<Utc>,
    ) -> HandoffReason {
        sqlx::query("DELETE FROM attempt_handoffs")
            .execute(&mut *c)
            .await
            .unwrap();
        let r = mc::record_handoff(c, me, attempt, HandoffReason::OwnerStop, at).await;
        let stored = mc::handoff_of(c, attempt).await.unwrap().unwrap().reason;
        assert_eq!(r.unwrap(), stored);
        stored
    }
    assert_eq!(
        post(&mut c, me, attempt, now()).await,
        HandoffReason::Restart,
        "running"
    );
    let stop = DesiredState::Stopped;
    mc::set_desired(&mut c, "desk", stop, 1, who, Uuid::new_v4(), later)
        .await
        .unwrap();
    let r = post(&mut c, me, attempt, now()).await;
    assert_eq!(r, HandoffReason::Restart, "stop after the handoff");
    assert_eq!(
        post(&mut c, me, attempt, later).await,
        HandoffReason::OwnerStop
    );
}

/// Review I1(b): owner_stop/service_stop are refused (typed) once the attempt
/// no longer runs; a restart is still recorded.
#[tokio::test]
async fn exempt_handoff_is_refused_after_the_attempt_stopped() {
    let f = fixture().await;
    let (attempt, pool, mut hot) = granted(&f).await;
    hot.status = Status::Finished;
    hot.stop = Stop::Stopped {
        receipt: artifact("synthetic-stop-owner-receipt", b"synthetic-stop"),
    };
    set_hot(&pool, attempt, &hot).await;
    let me = f.node.node_id;
    let mut c = pool.acquire().await.unwrap();
    for reason in [HandoffReason::OwnerStop, HandoffReason::ServiceStop] {
        let r = mc::record_handoff(&mut c, me, attempt, reason, now()).await;
        assert!(matches!(r, Err(Error::Refused(m)) if m.starts_with("handoff_not_running")));
    }
    assert!(mc::handoff_of(&mut c, attempt).await.unwrap().is_none());
    let r = mc::record_handoff(&mut c, me, attempt, HandoffReason::Restart, now()).await;
    assert_eq!(r.unwrap(), HandoffReason::Restart);
}

#[tokio::test]
async fn accepts_new_work_follows_desired() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let ids = machine(&pool, 1).await;
    let who = owner(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    assert!(mc::accepts_new_work(&mut c, ids[0]).await.unwrap());
    mc::set_desired(
        &mut c,
        "desk",
        DesiredState::Draining,
        1,
        who,
        Uuid::new_v4(),
        now(),
    )
    .await
    .unwrap();
    assert!(!mc::accepts_new_work(&mut c, ids[0]).await.unwrap());
}

/// Review I3: a revoke racing a slot mint waits for it, so no minted sibling
/// stays live.
#[tokio::test]
async fn concurrent_mint_and_revoke_leave_no_live_sibling() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let first = machine(&pool, 1).await[0];
    let mut mint = pool.begin().await.unwrap();
    let caller = mc::claim_key(&mut mint, first, "desk").await.unwrap();
    mc::mint_slots(&mut mint, &caller, 3, &new_slot(), &hashes(2), now())
        .await
        .unwrap();
    let revoke = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            let n = mc::revoke_machine(&mut tx, "desk", now()).await.unwrap();
            tx.commit().await.unwrap();
            n
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!revoke.is_finished(), "the revoke waits for the mint");
    mint.commit().await.unwrap();
    assert_eq!(revoke.await.unwrap(), 3);
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM nodes WHERE machine_key='desk' AND revoked_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 0);
}

/// Review I4: the key stays in use while any live slot holds it, and a slot
/// alone is not revoked (`node:` of a machine node is refused).
#[tokio::test]
async fn key_is_not_adopted_while_a_sibling_lives() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let ids = machine(&pool, 3).await;
    let mut c = pool.acquire().await.unwrap();
    for id in &ids {
        let r = mc::revoke_machine(&mut c, &format!("node:{id}"), now()).await;
        assert!(
            matches!(r, Err(Error::MachineNode(ref k)) if k == "desk"),
            "{r:?}"
        );
    }
    // Slot 1 revoked by older code: its live siblings still hold the key.
    sqlx::query("UPDATE nodes SET revoked_at=$2 WHERE id=$1")
        .bind(ids[0])
        .bind(now())
        .execute(&pool)
        .await
        .unwrap();
    let other = node(&pool, "other").await;
    assert!(matches!(
        mc::claim_key(&mut c, other, "desk").await,
        Err(Error::KeyInUse)
    ));
    assert_eq!(mc::revoke_machine(&mut c, "desk", now()).await.unwrap(), 2);
    assert_eq!(
        mc::claim_key(&mut c, other, "desk").await.unwrap().slot,
        Some(1)
    );
}

/// Fix round 2 (I2): a publication whose SERIALIZABLE snapshot predates a
/// committed Stop cannot lock its node row: a hold (no offer), not an error.
#[tokio::test]
async fn publishing_node_lock_after_a_committed_stop_is_a_hold() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let ids = machine(&pool, 1).await;
    let who = owner(&pool).await;
    let begin = || async {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT 1").execute(&mut *tx).await.unwrap();
        tx
    };
    let mut quiet = begin().await;
    assert!(mc::lock_publishing_node(&mut quiet, ids[0]).await.unwrap());
    drop(quiet);
    let mut publish = begin().await;
    let mut stop = pool.acquire().await.unwrap();
    let stopped = DesiredState::Stopped;
    mc::set_desired(&mut stop, "desk", stopped, 1, who, Uuid::new_v4(), now())
        .await
        .unwrap();
    assert!(
        !mc::lock_publishing_node(&mut publish, ids[0])
            .await
            .unwrap()
    );
}

/// Fix round 3: a switch set by a former owner reads as unset (with the flag),
/// and a current owner setting the same state writes a new row under them.
#[tokio::test]
async fn former_owners_switch_reads_unset_and_can_be_set_again() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let (old, new) = (owner(&pool).await, owner(&pool).await);
    let mut c = pool.acquire().await.unwrap();
    mc::set_owner_availability(&mut c, true, 1, old, Uuid::new_v4(), now())
        .await
        .unwrap();
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();
    let read = mc::owner_availability(&mut c).await.unwrap();
    assert_eq!((read.state, read.generation), (Availability::Unset, 2));
    assert!(read.set_by_former_owner);
    let again = mc::set_owner_availability(&mut c, true, 2, new, Uuid::new_v4(), now())
        .await
        .unwrap();
    assert_eq!((again.state, again.generation), (Availability::On, 3));
    assert!(!again.set_by_former_owner);
    assert_eq!(mc::owner_switch(&mut c).await.unwrap(), Some((new, 3)));
}

/// Fix round 3: a publication racing an availability change, or still waiting
/// past its lock timeout, holds instead of failing.
#[tokio::test]
async fn publishing_node_lock_holds_after_a_switch_change_or_a_lock_timeout() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let ids = machine(&pool, 1).await;
    let who = owner(&pool).await;
    let begin = |timeout: bool| {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
                .execute(&mut *tx)
                .await
                .unwrap();
            if timeout {
                sqlx::query("SET LOCAL lock_timeout='100ms'")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            sqlx::query("SELECT 1").execute(&mut *tx).await.unwrap();
            tx
        }
    };
    let mut publish = begin(false).await;
    let mut switch = pool.acquire().await.unwrap();
    mc::set_owner_availability(&mut switch, false, 1, who, Uuid::new_v4(), now())
        .await
        .unwrap();
    assert!(
        !mc::lock_publishing_node(&mut publish, ids[0])
            .await
            .unwrap()
    );
    drop((publish, switch));
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(ids[0])
        .execute(&mut *holder)
        .await
        .unwrap();
    let mut waiting = begin(true).await;
    assert!(
        !mc::lock_publishing_node(&mut waiting, ids[0])
            .await
            .unwrap()
    );
}

/// Fix round 2 (I4): a `node:` revoke racing the node's own key claim waits
/// for it and then refuses (the node is now a machine slot); it never revokes
/// slot 1 alone.
#[tokio::test]
async fn node_revoke_racing_a_key_claim_refuses() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let n = node(&pool, "desk").await;
    let mut claim = pool.begin().await.unwrap();
    mc::claim_key(&mut claim, n, "desk").await.unwrap();
    let revoke = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            mc::revoke_machine(&mut tx, &format!("node:{n}"), now()).await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!revoke.is_finished(), "the revoke waits for the claim");
    claim.commit().await.unwrap();
    let r = revoke.await.unwrap();
    assert!(
        matches!(&r, Err(Error::MachineNode(k)) if k == "desk"),
        "{r:?}"
    );
}

/// Review minor: of two racing slot-1 claims of one key, the loser gets
/// KeyInUse (a 409), never a database error.
#[tokio::test]
async fn racing_slot_one_claims_lose_with_key_in_use() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let (a, b) = (node(&pool, "a").await, node(&pool, "b").await);
    let mut first = pool.begin().await.unwrap();
    mc::claim_key(&mut first, a, "desk").await.unwrap();
    let second = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            mc::claim_key(&mut tx, b, "desk").await.map(drop)
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    first.commit().await.unwrap();
    assert!(matches!(second.await.unwrap(), Err(Error::KeyInUse)));
}

/// Review I6: "enrolled" is a live node that redeemed an owner's code, or a
/// live minted slot of its machine; not a revoked node or any other node row.
#[tokio::test]
async fn enrolled_means_code_enrolled_or_a_minted_sibling() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let ids = machine(&pool, 3).await;
    let stray = node(&pool, "stray").await;
    let mut c = pool.acquire().await.unwrap();
    async fn all(c: &mut sqlx::PgConnection, ids: &[Uuid]) -> Vec<bool> {
        let mut out = vec![];
        for id in ids {
            out.push(mc::enrolled(c, *id).await.unwrap());
        }
        out
    }
    let ids = [ids.clone(), vec![stray]].concat();
    assert_eq!(all(&mut c, &ids).await, [false; 4], "no code: not enrolled");
    sqlx::query("INSERT INTO enrollment_codes(id,code_hash,created_by,created_at,expires_at,used_at,node_id) VALUES($1,'h',$2,$3,$3,$3,$4)")
        .bind(Uuid::new_v4()).bind(owner(&pool).await).bind(now()).bind(ids[0])
        .execute(&pool).await.unwrap();
    assert_eq!(all(&mut c, &ids).await, [true, true, true, false]);
    sqlx::query("UPDATE nodes SET revoked_at=$2 WHERE id=$1")
        .bind(ids[2])
        .bind(now())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(all(&mut c, &ids).await, [true, true, false, false]);
}

#[tokio::test]
async fn revoke_machine_revokes_every_slot_and_clears_control() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    machine(&pool, 3).await;
    let legacy = node(&pool, "legacy").await;
    let who = owner(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    mc::set_desired(
        &mut c,
        "desk",
        DesiredState::Stopped,
        1,
        who,
        Uuid::new_v4(),
        now(),
    )
    .await
    .unwrap();
    assert_eq!(mc::revoke_machine(&mut c, "desk", now()).await.unwrap(), 3);
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM nodes WHERE machine_key='desk' AND revoked_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 0);
    let ctl = mc::control(&mut c, "desk").await.unwrap();
    assert_eq!((ctl.desired, ctl.generation), (DesiredState::Running, 1));
    assert_eq!(
        mc::revoke_machine(&mut c, &format!("node:{legacy}"), now())
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn owner_availability_defaults_unset_and_is_compare_and_swap() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let who = owner(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    let d = mc::owner_availability(&mut c).await.unwrap();
    assert_eq!((d.state, d.generation), (Availability::Unset, 1));
    let on = mc::set_owner_availability(&mut c, true, 1, who, Uuid::new_v4(), now())
        .await
        .unwrap();
    assert_eq!((on.state, on.generation), (Availability::On, 2));
    assert!(matches!(
        mc::set_owner_availability(&mut c, false, 1, who, Uuid::new_v4(), now()).await,
        Err(Error::Conflict { generation: 2 })
    ));
}

/// Fix round 2: on to on (and off to off) at the current generation changes
/// nothing, so it cannot invalidate offers selected under the switch.
#[tokio::test]
async fn owner_availability_unchanged_state_keeps_the_generation() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let who = owner(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    let mut set = async |on: bool, expected: u64| {
        let r = mc::set_owner_availability(&mut c, on, expected, who, Uuid::new_v4(), now());
        r.await.unwrap().generation
    };
    assert_eq!(set(true, 1).await, 2);
    assert_eq!(set(true, 2).await, 2, "on to on");
    assert_eq!(set(false, 2).await, 3);
    assert_eq!(set(false, 3).await, 3, "off to off");
}

/// Fix round 2: a switch turned on by a user who is no longer an owner counts
/// as unset.
#[tokio::test]
async fn owner_switch_of_a_demoted_user_is_unset() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let who = owner(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    mc::set_owner_availability(&mut c, true, 1, who, Uuid::new_v4(), now())
        .await
        .unwrap();
    assert_eq!(mc::owner_switch(&mut c).await.unwrap(), Some((who, 2)));
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(who)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(mc::owner_switch(&mut c).await.unwrap(), None);
}

#[tokio::test]
async fn node_services_report_desired_and_since() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let ids = machine(&pool, 3).await;
    let who = owner(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    mc::set_desired(
        &mut c,
        "desk",
        DesiredState::Draining,
        1,
        who,
        Uuid::new_v4(),
        now(),
    )
    .await
    .unwrap();
    let s = mc::node_services(&mut c, &ids).await.unwrap();
    assert_eq!(s.len(), 3);
    for n in s {
        assert_eq!(
            (n.desired, n.generation, n.since_ms),
            (DesiredState::Draining, 2, now().timestamp_millis() as u64)
        );
    }
}

/// The migrated test database (the store applies the migrations).
async fn migrated(db: &TestDb) -> PgPool {
    db.store().await;
    db.pool().await
}

/// The migrated database through a pool wide enough for a holder, the action
/// under test and a probe at once.
async fn wide(db: &TestDb) -> PgPool {
    let options = migrated(db).await.connect_options().as_ref().clone();
    let wide = sqlx::postgres::PgPoolOptions::new().max_connections(8);
    wide.connect_with(options).await.unwrap()
}

/// A machine "desk" of `n` live nodes whose ids ascend while their heap order
/// descends (the highest id is inserted first). Returns the ids ascending.
async fn reversed_machine(pool: &PgPool, n: u128) -> Vec<Uuid> {
    let ids: Vec<Uuid> = (1..=n).map(|i| Uuid::from_u128(i << 64)).collect();
    for (slot, id) in ids.iter().enumerate().rev() {
        sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,machine_key,slot) VALUES($1,$2,$3,'0.1.0',1,$4,'desk',$5)")
            .bind(id).bind(format!("desk-{}", slot + 1)).bind(format!("hash-{id}")).bind(now()).bind(slot as i32 + 1)
            .execute(pool).await.unwrap();
    }
    ids
}

/// The owner actions that lock several node rows, and the touch they replaced.
#[derive(Debug, Clone, Copy)]
enum Locker {
    UnorderedTouch,
    Switch,
    Stop,
    Revoke,
}
async fn act(c: &mut sqlx::PgConnection, locker: Locker, who: Uuid) -> Result<(), Error> {
    let op = Uuid::new_v4();
    match locker {
        Locker::UnorderedTouch => {
            let touch = "UPDATE nodes SET machine_key=machine_key WHERE revoked_at IS NULL";
            sqlx::query(touch).execute(c).await.map(|_| ())?
        }
        Locker::Switch => mc::set_owner_availability(c, true, 1, who, op, now())
            .await
            .map(|_| ())?,
        Locker::Stop => {
            let stopped = DesiredState::Stopped;
            mc::set_desired(&mut *c, "desk", stopped, 1, who, op, now()).await?;
            mc::request_handoffs(c, "desk", now()).await.map(|_| ())?
        }
        Locker::Revoke => mc::revoke_machine(c, "desk", now()).await.map(|_| ())?,
    }
    Ok(())
}

/// Runs `locker` in its own transaction while another holds the lowest node's
/// row, waits until it blocks, and reports whether the highest node's row was
/// still free (an id-ordered locker holds nothing while it waits on the
/// lowest). The action then commits once the holder lets go.
async fn locks_lowest_first(pool: &PgPool, ids: &[Uuid], locker: Locker, who: Uuid) -> bool {
    let (low, high) = (ids[0], *ids.last().unwrap());
    let mut holder = pool.begin().await.unwrap();
    let lock = "SELECT 1 FROM nodes WHERE id=$1 FOR UPDATE";
    sqlx::query(lock)
        .bind(low)
        .execute(&mut *holder)
        .await
        .unwrap();
    let run = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            act(&mut tx, locker, who).await?;
            tx.commit().await.map_err(Error::from)
        }
    });
    let waiting = "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock'";
    for _ in 0..200 {
        let n: i64 = sqlx::query_scalar(waiting).fetch_one(pool).await.unwrap();
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(!run.is_finished(), "{locker:?} waits for the lowest node");
    let mut probe = pool.begin().await.unwrap();
    let free = sqlx::query(&format!("{lock} NOWAIT"))
        .bind(high)
        .execute(&mut *probe)
        .await;
    if let Err(e) = &free {
        assert!(mc::is_lock_timeout(e), "{locker:?}: {e}");
    }
    probe.rollback().await.unwrap();
    holder.rollback().await.unwrap();
    run.await.unwrap().unwrap();
    free.is_ok()
}

/// N1: the availability switch, Stop and Revoke lock every node they touch in
/// id order, whatever the heap order, so two of them over shared nodes wait for
/// each other instead of deadlocking (40P01). The old unordered touch fails the
/// same probe: it locks in heap order, highest id first here.
#[tokio::test]
async fn multi_node_owner_actions_lock_nodes_in_id_order() {
    let db = TestDb::create().await;
    let pool = wide(&db).await;
    let ids = reversed_machine(&pool, 4).await;
    let who = owner(&pool).await;
    assert!(!locks_lowest_first(&pool, &ids, Locker::UnorderedTouch, who).await);
    for locker in [Locker::Switch, Locker::Stop, Locker::Revoke] {
        assert!(
            locks_lowest_first(&pool, &ids, locker, who).await,
            "{locker:?}"
        );
    }
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM nodes WHERE revoked_at IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(live, 0, "the revoke committed");
}

/// N1: the switch and Stop/Start run concurrently over the same nodes, then a
/// Revoke races a switch: no deadlock. A real lock-order inversion between two
/// transactions is reported as 40P01, so the check below would see one.
#[tokio::test]
async fn switch_and_stop_or_revoke_run_concurrently_without_deadlock() {
    let db = TestDb::create().await;
    let pool = wide(&db).await;
    let ids = reversed_machine(&pool, 4).await;
    let who = owner(&pool).await;
    // Failure case: two transactions taking two nodes in opposite order deadlock.
    let (mut a, mut b) = (pool.begin().await.unwrap(), pool.begin().await.unwrap());
    let lock = "SELECT 1 FROM nodes WHERE id=$1 FOR UPDATE";
    sqlx::query(lock)
        .bind(ids[0])
        .execute(&mut *a)
        .await
        .unwrap();
    sqlx::query(lock)
        .bind(ids[1])
        .execute(&mut *b)
        .await
        .unwrap();
    let (ra, rb) = tokio::join!(
        sqlx::query(lock).bind(ids[1]).execute(&mut *a),
        sqlx::query(lock).bind(ids[0]).execute(&mut *b)
    );
    let deadlocked = [ra.err(), rb.err()].into_iter().flatten();
    assert_eq!(deadlocked.filter(mc::is_serialization_failure).count(), 1);
    drop((a, b));

    let deadlock =
        |r: &Result<(), Error>| matches!(r, Err(Error::Db(e)) if mc::is_serialization_failure(e));
    let switches = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut results = vec![];
            for i in 0..20 {
                let mut tx = pool.begin().await.unwrap();
                let generation = mc::owner_availability(&mut tx).await.unwrap().generation;
                let r = mc::set_owner_availability(
                    &mut tx,
                    i % 2 == 0,
                    generation,
                    who,
                    Uuid::new_v4(),
                    now(),
                )
                .await;
                results.push(match r {
                    Ok(_) => tx.commit().await.map_err(Error::from),
                    Err(e) => Err(e),
                });
            }
            results
        }
    });
    let controls = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut results = vec![];
            for i in 0..20 {
                let mut tx = pool.begin().await.unwrap();
                let generation = mc::control(&mut tx, "desk").await.unwrap().generation;
                let desired = [DesiredState::Stopped, DesiredState::Running][i % 2];
                let r = mc::set_desired(
                    &mut tx,
                    "desk",
                    desired,
                    generation,
                    who,
                    Uuid::new_v4(),
                    now(),
                )
                .await;
                let r = match r {
                    Ok(_) => mc::request_handoffs(&mut tx, "desk", now())
                        .await
                        .map(|_| ()),
                    Err(e) => Err(e),
                };
                results.push(match r {
                    Ok(()) => tx.commit().await.map_err(Error::from),
                    Err(e) => Err(e),
                });
            }
            results
        }
    });
    let (switches, controls) = (switches.await.unwrap(), controls.await.unwrap());
    assert!(
        switches.iter().chain(&controls).all(|r| r.is_ok()),
        "{switches:?} {controls:?}"
    );
    let revoke = {
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            mc::revoke_machine(&mut tx, "desk", now()).await?;
            tx.commit().await.map_err(Error::from)
        }
    };
    let switch = async {
        let mut tx = pool.begin().await.unwrap();
        let generation = mc::owner_availability(&mut tx).await.unwrap().generation;
        mc::set_owner_availability(&mut tx, false, generation, who, Uuid::new_v4(), now()).await?;
        tx.commit().await.map_err(Error::from)
    };
    let (revoked, switched) = tokio::join!(revoke, switch);
    assert!(
        !deadlock(&revoked) && !deadlock(&switched),
        "{revoked:?} {switched:?}"
    );
    assert!(
        revoked.is_ok() && switched.is_ok(),
        "{revoked:?} {switched:?}"
    );
}

/// N2: a no-op switch (the state it already has, at the current generation) and
/// a stale one write no node row and wait on no node lock: every live node keeps
/// its `xmin` while another transaction holds one of them. A real flip still
/// touches every live node (the I2 race needs it).
#[tokio::test]
async fn no_op_switch_writes_and_locks_no_node_row() {
    let db = TestDb::create().await;
    let pool = wide(&db).await;
    let ids = reversed_machine(&pool, 3).await;
    let who = owner(&pool).await;
    let mut c = pool.acquire().await.unwrap();
    mc::set_owner_availability(&mut c, true, 1, who, Uuid::new_v4(), now())
        .await
        .unwrap();
    let xmins = || async {
        let sql = "SELECT xmin::text FROM nodes WHERE revoked_at IS NULL ORDER BY id";
        sqlx::query_scalar::<_, String>(sql)
            .fetch_all(&pool)
            .await
            .unwrap()
    };
    let before = xmins().await;
    assert_eq!(before.len(), 3);
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(ids[1])
        .execute(&mut *holder)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let quick = std::time::Duration::from_millis(500);
    let same = mc::set_owner_availability(&mut tx, true, 2, who, Uuid::new_v4(), now());
    let same = tokio::time::timeout(quick, same)
        .await
        .expect("no node lock wait")
        .unwrap();
    assert_eq!((same.state, same.generation), (Availability::On, 2));
    let stale = mc::set_owner_availability(&mut tx, false, 1, who, Uuid::new_v4(), now());
    let stale = tokio::time::timeout(quick, stale)
        .await
        .expect("no node lock wait");
    assert!(
        matches!(stale, Err(Error::Conflict { generation: 2 })),
        "{stale:?}"
    );
    tx.commit().await.unwrap();
    assert_eq!(xmins().await, before, "no node row written");
    // Failure case: a real flip waits for the held node, then writes every live node.
    let flip = tokio::spawn({
        let pool = pool.clone();
        async move {
            let mut tx = pool.begin().await.unwrap();
            let r = mc::set_owner_availability(&mut tx, false, 2, who, Uuid::new_v4(), now()).await;
            tx.commit().await.unwrap();
            r
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!flip.is_finished(), "a flip locks the held node");
    holder.rollback().await.unwrap();
    let flipped = flip.await.unwrap().unwrap();
    assert_eq!((flipped.state, flipped.generation), (Availability::Off, 3));
    let after = xmins().await;
    assert!(
        after.iter().zip(&before).all(|(a, b)| a != b),
        "{after:?} {before:?}"
    );
}
