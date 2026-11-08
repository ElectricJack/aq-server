//! R6 M6: a lost attempt held before migration 0106 (#117 recorded `held_reason` and posted its
//! system comment) becomes a task hold with that comment, so the task is never commented twice;
//! a hold whose comment was never posted gets exactly one.
use aq_machine_control::holds::{self, Decision};
use aq_server::test_support::{TestDb, migrate_profile};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// A held lost attempt of `task` at format 105, with the #117 comment when `commented`.
async fn lost_hold(pool: &PgPool, project: Uuid, task: Uuid, commented: bool) -> Uuid {
    let (attempt, node) = (Uuid::new_v4(), Uuid::new_v4());
    let state = json!({"offer":{"manifest":{"bindings":{"project_id":project}}}});
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,$2,$3,$4,$5,TRUE,$6)")
        .bind(attempt).bind(task).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(state)
        .execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO lost_attempt_releases(attempt_id,node_id,proof_kind,proof,reason,charged,receipt_digest,recorded_at,held_reason) VALUES($1,$2,'cgroup_teardown','{}','lost_after_crash',TRUE,$3,now(),'lineage_retry_budget_exhausted')")
        .bind(attempt).bind(node).bind("a".repeat(64))
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    if commented {
        let body = format!(
            "AQ: this task is held. Its last attempt ({attempt}) was lost after a crash and its retry budget is exhausted after lost attempts (hold reason: lineage_retry_budget_exhausted). An owner can retire the task or release the hold."
        );
        let thread = aq_task_comments::Thread {
            project_id: project,
            task_id: task,
        };
        let author = aq_task_comments::Author::System {
            component: "aq.swarm".into(),
        };
        let id = aq_machine_control::lost::hold_comment_id(attempt);
        aq_task_comments::add(pool, id, thread, &author, &body, chrono::Utc::now())
            .await
            .unwrap();
    }
    attempt
}
async fn comments(pool: &PgPool, task: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM task_comments WHERE task_id=$1 AND author_kind='system'",
    )
    .bind(task)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn pre_0106_lost_holds_become_task_holds_without_a_second_comment() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 105).await.unwrap();
    let project = Uuid::new_v4();
    let (posted, unposted) = (Uuid::new_v4(), Uuid::new_v4());
    let attempt = lost_hold(&pool, project, posted, true).await;
    lost_hold(&pool, project, unposted, false).await;
    assert_eq!(comments(&pool, posted).await, 1);

    let _store = db.store().await;
    let rows: Vec<(Uuid, Uuid, String, bool)> = sqlx::query_as(
        "SELECT task_id,comment_id,reason,commented FROM task_holds_current ORDER BY commented DESC",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        rows[0],
        (
            posted,
            aq_machine_control::lost::hold_comment_id(attempt),
            "lineage_retry_budget_exhausted".to_owned(),
            true
        ),
        "the #117 comment is the hold's"
    );
    assert_eq!((rows[1].0, rows[1].3), (unposted, false));

    // The next poll's decision for either task posts nothing twice.
    let at = chrono::Utc::now();
    for task in [posted, unposted] {
        for _ in 0..2 {
            let d = Decision {
                project,
                task,
                reason: "lineage_retry_budget_exhausted",
                seen: 1,
                attempt: None,
            };
            holds::hold(&pool, Some(d), at).await;
        }
    }
    assert_eq!(comments(&pool, posted).await, 1, "no second comment");
    assert_eq!(
        comments(&pool, unposted).await,
        1,
        "the unposted hold gets one"
    );
    let mut conn = pool.acquire().await.unwrap();
    let counted = holds::counts(&mut conn, &[project]).await.unwrap();
    assert_eq!(
        counted,
        [(project, "lineage_retry_budget_exhausted".to_owned(), 2)]
    );
}
