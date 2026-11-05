//! F5 agent questions: a typed `question` decision wakes the waiting attempt
//! once through H4, or reaches an ended attempt's continuation as task mail.
use super::*;
use aq_server::agent_access::{questions, waits};

struct Asked {
    env: ReviewEnv,
    decision_id: Uuid,
    wait_id: Uuid,
}
impl Asked {
    fn node(&self) -> Uuid {
        self.env.f.node.node_id
    }
    fn attempt(&self) -> Uuid {
        self.env.report.binding.attempt_id
    }
}
async fn ask(env: ReviewEnv, operation: Uuid) -> (Asked, serde_json::Value) {
    let asked = questions::ask(
        &env.f.state,
        env.f.node.node_id,
        env.report.binding.attempt_id,
        operation,
        "Which database should the migration target?",
    )
    .await
    .unwrap();
    let id = |k: &str| Uuid::parse_str(asked[k].as_str().unwrap()).unwrap();
    (
        Asked {
            decision_id: id("decision_id"),
            wait_id: id("wait_id"),
            env,
        },
        asked,
    )
}

#[tokio::test]
async fn question_answer_resumes_waiting_attempt() {
    let env = Box::pin(review_env(1)).await;
    let operation = Uuid::new_v4();
    let (q, asked) = ask(env, operation).await;
    let f = &q.env.f;
    let pool = f.db.pool().await;
    assert_eq!(asked["kind"], "question");
    assert_eq!(asked["state"], "pending");
    assert_eq!(asked["decision_revision"], "1");
    assert_eq!(asked["subject_id"], json!(f.selection.lineage.task_id));

    // Replay returns the same decision; a changed question under the same
    // operation is a conflict and creates nothing.
    let replay = questions::ask(
        &f.state,
        q.node(),
        q.attempt(),
        operation,
        "Which database should the migration target?",
    )
    .await
    .unwrap();
    assert_eq!(replay, asked);
    let changed = questions::ask(
        &f.state,
        q.node(),
        q.attempt(),
        operation,
        "Something else?",
    )
    .await
    .unwrap_err();
    assert_eq!(changed.0, "id_conflict");
    let decisions: i64 = sqlx::query_scalar("SELECT count(*) FROM question_decisions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(decisions, 1);
    let phase: String = sqlx::query_scalar("SELECT phase FROM agent_waits WHERE wait_id=$1")
        .bind(q.wait_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(phase, "pending");

    // A stale revision changes nothing; the exact revision answers once.
    let answer = Uuid::new_v4();
    let stale = questions::answer(&f.state, f.actor, q.decision_id, 2, answer, "Postgres")
        .await
        .unwrap_err();
    assert_eq!(stale.0, "stale_revision");
    let receipt = questions::answer(&f.state, f.actor, q.decision_id, 1, answer, "Postgres")
        .await
        .unwrap();
    assert_eq!(receipt["decision_revision"], "2");
    assert_eq!(receipt["delivery"], json!({"kind":"wait"}));
    let again = questions::answer(&f.state, f.actor, q.decision_id, 1, answer, "Postgres")
        .await
        .unwrap();
    assert_eq!(again, receipt);
    let second = questions::answer(
        &f.state,
        f.actor,
        q.decision_id,
        2,
        Uuid::new_v4(),
        "SQLite",
    )
    .await
    .unwrap_err();
    assert_eq!(second.0, "id_conflict");

    // Duplicate reconciliation records exactly one outcome and one intent.
    f.time.store((NOW + 61_000) as i64, Ordering::SeqCst);
    waits::reconcile_due(&f.state, 64).await.unwrap();
    f.time.store((NOW + 125_000) as i64, Ordering::SeqCst);
    waits::reconcile_due(&f.state, 64).await.unwrap();
    let (outcomes, kind): (i64, Option<String>) =
        sqlx::query_as("SELECT count(*),max(kind) FROM agent_wait_outcomes WHERE wait_id=$1")
            .bind(q.wait_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((outcomes, kind.as_deref()), (1, Some("satisfied")));
    let intents: Vec<String> =
        sqlx::query_scalar("SELECT state FROM agent_wait_resume_intents WHERE wait_id=$1")
            .bind(q.wait_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(intents, ["ready"]);

    let shown = questions::show(&f.state, q.node(), q.attempt(), q.decision_id)
        .await
        .unwrap();
    assert_eq!(shown["state"], "answered");
    assert_eq!(shown["answer"], "Postgres");
    // A live waiting attempt is woken by the wait, never by mail.
    let mail: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_messages WHERE thread_id=$1")
        .bind(q.decision_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mail, 0);
}

#[tokio::test]
async fn question_after_attempt_ended_routes_to_continuation() {
    let env = Box::pin(review_env(1)).await;
    let (q, _) = ask(env, Uuid::new_v4()).await;
    let f = &q.env.f;
    let pool = f.db.pool().await;
    // The asking attempt ends: its launch binding is revoked.
    let l = &f.selection.lineage;
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context,revoked) VALUES($1,$2,$3,'{}','{}',TRUE)")
        .bind(q.attempt())
        .bind(q.node())
        .bind(json!({"task_id":l.task_id,"project_id":l.project_id,"workspace_id":l.workspace_id,"attempt_id":q.attempt(),"session_id":Uuid::new_v4(),"generation":1}))
        .execute(&pool)
        .await
        .unwrap();

    let receipt = questions::answer(
        &f.state,
        f.actor,
        q.decision_id,
        1,
        Uuid::new_v4(),
        "Use Postgres 17",
    )
    .await
    .unwrap();
    assert_eq!(receipt["delivery"]["kind"], "task_mail");
    let rows = sqlx::query(
        "SELECT message_id,recipient,sender,body FROM mailbox_messages WHERE thread_id=$1",
    )
    .bind(q.decision_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    let recipient: serde_json::Value = sqlx::Row::get(&rows[0], "recipient");
    let sender: serde_json::Value = sqlx::Row::get(&rows[0], "sender");
    let body: String = sqlx::Row::get(&rows[0], "body");
    assert_eq!(recipient["kind"], "task");
    assert_eq!(recipient["task_id"], json!(l.task_id));
    assert_eq!(sender["component"], "question_answer");
    assert!(body.contains("Use Postgres 17"));
    assert_eq!(
        json!(sqlx::Row::get::<Uuid, _>(&rows[0], "message_id")),
        receipt["delivery"]["message_id"]
    );
    // No session-addressed mail was created or retargeted.
    let session_mail: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mailbox_messages WHERE recipient->>'kind'='session'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(session_mail, 0);
}
