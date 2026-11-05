//! H3 stall ladder against the real store: a session that never progresses
//! climbs nudge, notify, stop and replace with persisted, receipted actions;
//! the lineage ceiling quarantines the next stall; a crash between intent and
//! receipt re-runs that effect once with the same action id.
mod common;
use aq_server::{
    error::AppError,
    stall::{
        engine::{self, Ports, Target},
        evidence::{Evidence, InputState},
        ladder::{Delivery, Intent, Outcome},
        policy::ExecutionPolicy,
        ports,
    },
};
use async_trait::async_trait;
use common::profiled::{NOW, fixture, insert};
use std::{
    collections::HashSet,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

const MIN: u64 = 60_000;

#[derive(Default)]
struct Fake {
    effects: Mutex<Vec<(Uuid, Uuid, String)>>,
    stopped: Mutex<HashSet<Uuid>>,
    started: Mutex<std::collections::HashMap<Uuid, u64>>,
    crash_notify_once: AtomicBool,
}

#[async_trait]
impl Ports for Fake {
    async fn evidence(
        &self,
        t: &Target,
        _: &ExecutionPolicy,
        now: u64,
    ) -> Result<Evidence, AppError> {
        let start = *self
            .started
            .lock()
            .unwrap()
            .entry(t.attempt_id)
            .or_insert(now);
        let mut e = Evidence::quiet(start);
        e.observed_through_ms = Some(now);
        e.stop_confirmed = self.stopped.lock().unwrap().contains(&t.attempt_id);
        Ok(e)
    }
    async fn effect(
        &self,
        t: &Target,
        id: Uuid,
        intent: &Intent,
        _: &ExecutionPolicy,
        _: u64,
    ) -> Result<Outcome, AppError> {
        if matches!(intent, Intent::Notify) && self.crash_notify_once.swap(false, Ordering::SeqCst)
        {
            return Err(AppError::Internal);
        }
        self.effects
            .lock()
            .unwrap()
            .push((t.attempt_id, id, intent.kind().into()));
        Ok(match intent {
            Intent::Nudge { .. } => Outcome::Nudged {
                message_id: Uuid::new_v5(&id, b"m"),
                delivery: Delivery::Attempted,
            },
            Intent::Stop => {
                self.stopped.lock().unwrap().insert(t.attempt_id);
                Outcome::Done
            }
            Intent::Replace => Outcome::Replaced { replacement: None },
            _ => Outcome::Done,
        })
    }
}

fn target(f: &common::profiled::Fixture, attempt: Uuid) -> Target {
    Target {
        attempt_id: attempt,
        session_id: Uuid::new_v4(),
        task_id: f.selection.lineage.task_id,
        project_id: f.selection.lineage.project_id,
        workspace_id: f.selection.lineage.workspace_id,
        generation: 1,
    }
}

async fn walk(pool: &sqlx::PgPool, fake: &Fake, t: &Target, from: u64, minutes: u64) {
    let policy = ExecutionPolicy::v1();
    for m in 0..=minutes {
        // A crashed effect leaves its intent pending; the next tick resumes it.
        let _ = engine::tick(pool, fake, t, &policy, from + m * MIN).await;
    }
}

/// The replaced attempt releases its task and a successor attempt in the same
/// lineage (task) takes it over.
async fn successor(pool: &sqlx::PgPool, first: Uuid) -> Uuid {
    let next = Uuid::new_v4();
    sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
        .bind(first)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state,execution_deadline_ms) SELECT $2,task_id,node_id,$3,$4,kind,state,execution_deadline_ms FROM execution_attempts WHERE attempt_id=$1")
        .bind(first).bind(next).bind(Uuid::new_v4()).bind(Uuid::new_v4()).execute(pool).await.unwrap();
    next
}

fn kinds(fake: &Fake, attempt: Uuid) -> Vec<String> {
    fake.effects
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.0 == attempt)
        .map(|e| e.2.clone())
        .collect()
}

#[tokio::test]
async fn stalled_session_is_nudged_then_escalated() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let first = insert(&f).await;
    let fake = Fake::default();
    fake.crash_notify_once.store(true, Ordering::SeqCst);
    let t1 = target(&f, first);
    walk(&pool, &fake, &t1, NOW, 60).await;
    assert_eq!(
        kinds(&fake, first),
        ["nudge", "nudge", "notify", "stop", "replace"]
    );

    // Restart safety: every action id ran its effect exactly once, the crashed
    // notify included, and every persisted action carries a receipt.
    assert!(
        !fake.crash_notify_once.load(Ordering::SeqCst),
        "notify crash was exercised"
    );
    let ids: Vec<Uuid> = fake.effects.lock().unwrap().iter().map(|e| e.1).collect();
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
    let rows: Vec<(Uuid, String, Option<String>, bool)> = sqlx::query_as("SELECT a.action_id,a.kind,a.delivery,a.receipt IS NOT NULL FROM stall_actions a JOIN stall_episodes e USING(episode_id) WHERE e.attempt_id=$1 ORDER BY a.created_at_ms")
        .bind(first).fetch_all(&pool).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), ids);
    assert!(rows.iter().all(|r| r.3));
    assert_eq!(
        rows.iter()
            .filter(|r| r.1 == "nudge")
            .map(|r| r.2.as_deref())
            .collect::<Vec<_>>(),
        [Some("attempted"), Some("attempted")]
    );
    let (rung, revision): (String, i64) =
        sqlx::query_as("SELECT rung,revision FROM stall_episodes WHERE attempt_id=$1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rung, "replaced");
    assert!(revision > 0);
    let spent: i32 =
        sqlx::query_scalar("SELECT replacements FROM stall_lineage_budgets WHERE lineage_id=$1")
            .bind(f.selection.lineage.task_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(spent, 1);

    // A final rung is inert on later ticks.
    walk(&pool, &fake, &t1, NOW + 61 * MIN, 30).await;
    assert_eq!(kinds(&fake, first).len(), 5);

    // The replacement stalls too: the lineage ceiling (1) is spent, so after
    // a confirmed stop the ladder quarantines instead of replacing again.
    let second = successor(&pool, first).await;
    let t2 = target(&f, second);
    walk(&pool, &fake, &t2, NOW + 100 * MIN, 60).await;
    assert_eq!(
        kinds(&fake, second),
        ["nudge", "nudge", "notify", "stop", "quarantine"]
    );
    let state: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM stall_episodes WHERE attempt_id=$1")
            .bind(second)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state["rung"], "quarantined");
    assert_eq!(state["quarantine"], "replacement_ceiling_reached");
}

async fn bind_launch(f: &common::profiled::Fixture, pool: &sqlx::PgPool, t: &Target) {
    let binding = serde_json::json!({"workspace_id":t.workspace_id,"project_id":t.project_id,"task_id":t.task_id,"session_id":t.session_id,"attempt_id":t.attempt_id,"generation":t.generation});
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,'{}','{}') ON CONFLICT(attempt_id) DO UPDATE SET binding=EXCLUDED.binding")
        .bind(t.attempt_id).bind(f.node.node_id).bind(binding).execute(pool).await.unwrap();
}

async fn terminal(f: &common::profiled::Fixture, pool: &sqlx::PgPool, t: &Target) -> Uuid {
    let terminal = Uuid::new_v4();
    let digest = "0".repeat(64);
    sqlx::query("INSERT INTO terminal_sessions(terminal_id,attempt_id,node_id,boot_id,session_id,assignment_generation,terminal_epoch,identity,authority_revision,execution_spec_digest,launch_manifest_digest,input_mode,ledger_revision,runtime_id,protection_version,key_id,nonce,ciphertext,controller_generation,pending_bytes,operation_count,observed_at_ms) VALUES($1,$2,$3,$4,$5,1,1,'{}',1,$6,$6,'finite_v1',0,$7,1,'k',$8,$9,0,0,0,$10)")
        .bind(terminal).bind(t.attempt_id).bind(f.node.node_id).bind(Uuid::new_v4()).bind(t.session_id).bind(&digest).bind(Uuid::new_v4())
        .bind(vec![0u8; 12]).bind(vec![0u8; 16]).bind(NOW as i64).execute(pool).await.unwrap();
    terminal
}

async fn send_nudge(pool: &sqlx::PgPool, t: &Target) -> (Uuid, Delivery) {
    let mut tx = pool.begin().await.unwrap();
    let out = ports::nudge(&mut tx, t, Uuid::new_v4(), "AQ stall check", false, NOW)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let Outcome::Nudged {
        message_id,
        delivery,
    } = out
    else {
        panic!("{out:?}")
    };
    (message_id, delivery)
}

#[tokio::test]
async fn nudge_never_injects_during_unknown_input_state() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let attempt = insert(&f).await;
    let mut t = target(&f, attempt);
    t.session_id =
        sqlx::query_scalar("SELECT session_id FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    bind_launch(&f, &pool, &t).await;
    let term = terminal(&f, &pool, &t).await;
    let operation = Uuid::new_v4();
    sqlx::query("INSERT INTO terminal_input_receipts(terminal_id,operation_id,principal_id,request_digest,command_digest,disposition,deadline_ms,dispatched,byte_count,admission_revision,updated_at_ms) VALUES($1,$2,$3,$4,$4,'delivery_unknown',$5,true,1,1,$5)")
        .bind(term).bind(operation).bind(Uuid::new_v4()).bind("0".repeat(64)).bind(NOW as i64).execute(&pool).await.unwrap();

    // Unknown input: the nudge is held in the inbox and never injected.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        ports::input_state(&mut tx, attempt).await.unwrap(),
        InputState::Unknown
    );
    tx.rollback().await.unwrap();
    let (deferred, delivery) = send_nudge(&pool, &t).await;
    assert_eq!(delivery, Delivery::Deferred);
    let inbox_only: bool =
        sqlx::query_scalar("SELECT inbox_only FROM stall_nudges WHERE message_id=$1")
            .bind(deferred)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(inbox_only);
    let readable: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mailbox_messages WHERE message_id=$1)")
            .bind(deferred)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(readable, "deferred nudge stays readable in the inbox");
    assert!(ports::injectable(&pool, attempt).await.unwrap().is_empty());

    // Input settles: a new nudge is eligible; the deferred one stays inbox-only.
    sqlx::query("UPDATE terminal_input_receipts SET disposition='applied' WHERE terminal_id=$1")
        .bind(term)
        .execute(&pool)
        .await
        .unwrap();
    let (eligible, delivery) = send_nudge(&pool, &t).await;
    assert_eq!(delivery, Delivery::Attempted);
    assert_eq!(
        ports::injectable(&pool, attempt).await.unwrap(),
        vec![eligible]
    );

    // A human takes the pane: the already-queued eligible nudge is withheld too.
    sqlx::query("UPDATE terminal_sessions SET controller_principal=$2,controller_generation=1 WHERE terminal_id=$1").bind(term).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    assert!(ports::injectable(&pool, attempt).await.unwrap().is_empty());
    // And input going unknown again withholds it after the human leaves.
    sqlx::query("UPDATE terminal_sessions SET controller_principal=NULL WHERE terminal_id=$1")
        .bind(term)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE terminal_input_receipts SET disposition='injecting' WHERE terminal_id=$1")
        .bind(term)
        .execute(&pool)
        .await
        .unwrap();
    assert!(ports::injectable(&pool, attempt).await.unwrap().is_empty());
}
