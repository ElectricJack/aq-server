//! Actual PostgreSQL qualification of the state34 journal, independent of Graph.
use aq_server::store::StoreError;
use aq_server::test_support::{TestDb, migrate_profile};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

// Qualify the historical state34 reader even after newer migrations are added.
async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    migrate_profile(pool, 34).await
}

async fn database(version: u32) -> (TestDb, PgPool) {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, version)
        .await
        .expect("apply actual migration prefix");
    (db, pool)
}

async fn rows(pool: &PgPool, table: &str) -> Value {
    sqlx::query_scalar(&format!(
        "SELECT COALESCE(jsonb_agg(row ORDER BY row::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) AS row FROM {table} t) s"
    ))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn catalog(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object('table',to_regclass('human_graph_operations')::text, \
         'index',to_regclass('human_graph_operation_actor')::text, \
         'function',to_regprocedure('guard_human_graph_operation_identity()')::text, \
         'triggers',(SELECT jsonb_agg(jsonb_build_array(tgname,tgtype,tgenabled) ORDER BY tgname) \
         FROM pg_trigger WHERE tgrelid=to_regclass('human_graph_operations') AND NOT tgisinternal))",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn principals(pool: &PgPool) -> (Uuid, Uuid) {
    let actor = Uuid::new_v4();
    let project = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,'owner','schema34',$2,now())")
        .bind(actor)
        .bind(format!("{actor}@schema34.invalid"))
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{}',1,'{}')")
        .bind(project).execute(pool).await.unwrap();
    (actor, project)
}

async fn intent(pool: &PgPool, actor: Uuid, project: Uuid) -> Uuid {
    let operation = Uuid::new_v4();
    sqlx::query("INSERT INTO human_graph_operations(operation_id,actor_id,project_id,project_revision,project_binding,workspace_id,task_id,request_bytes,request_digest,payload_bytes,payload_digest,service_identity,status,created_at_ms,expires_at_ms,retained_bytes) VALUES($1,$2,$3,1,'{}',$4,$1,$5,$6,$5,$6,'{}','{\"state\":\"pending\"}',1000,2000,2)")
        .bind(operation).bind(actor).bind(project).bind(Uuid::new_v4())
        .bind(b"{}".as_slice()).bind(format!("sha256:{}", "a".repeat(64)))
        .execute(pool).await.unwrap();
    operation
}

fn rejected(error: sqlx::Error, code: &str, reason: &str) {
    let database = error.as_database_error().expect("PostgreSQL error");
    assert_eq!(
        database.code().as_deref(),
        Some(code),
        "{reason}: {database}"
    );
    assert!(database.message().contains(reason), "{database}");
}

async fn reject_assignment(
    pool: &PgPool,
    operation: Uuid,
    assignment: &str,
    code: &str,
    reason: &str,
) {
    let error = sqlx::query(&format!(
        "UPDATE human_graph_operations SET {assignment} WHERE operation_id=$1"
    ))
    .bind(operation)
    .execute(pool)
    .await
    .unwrap_err();
    rejected(error, code, reason);
}

async fn assert_terminal_rows_immutable(pool: &PgPool, actor: Uuid, project: Uuid) {
    for state in ["confirmed", "conflict", "rejected"] {
        let id = intent(pool, actor, project).await;
        sqlx::query(
            "UPDATE human_graph_operations SET status=$2,raw_receipt=$3 WHERE operation_id=$1",
        )
        .bind(id)
        .bind(json!({"state":state}))
        .bind((state == "confirmed").then(|| json!({"receipt":"actual fixture"})))
        .execute(pool)
        .await
        .unwrap();
        let before = rows(pool, "human_graph_operations").await;
        // Exact replay remains harmless, but every durable terminal fact is frozen.
        sqlx::query("UPDATE human_graph_operations SET status=status WHERE operation_id=$1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        for assignment in [
            "status='{\"state\":\"pending\"}',raw_receipt=NULL",
            "stop_reason='changed'",
            "dispatch_count=1",
        ] {
            reject_assignment(
                pool,
                id,
                assignment,
                "P0001",
                "terminal operation is immutable",
            )
            .await;
        }
        assert_eq!(rows(pool, "human_graph_operations").await, before);
    }
}

#[tokio::test]
async fn fresh34_is_complete_and_idempotent() {
    let (_db, pool) = database(34).await;
    let before = catalog(&pool).await;
    assert_eq!(before["table"], "human_graph_operations");
    assert_eq!(before["index"], "human_graph_operation_actor");
    assert_eq!(before["triggers"].as_array().unwrap().len(), 3);
    let ledger = rows(&pool, "schema_migrations").await;
    assert_eq!(ledger.as_array().unwrap().len(), 34);
    migrate(&pool).await.unwrap();
    assert_eq!(catalog(&pool).await, before);
    assert_eq!(rows(&pool, "schema_migrations").await, ledger);
}

#[tokio::test]
async fn actual33_upgrade_preserves_legacy_source_and_old_reader_refuses34() {
    let (_db, pool) = database(33).await;
    let (actor, project) = principals(&pool).await;
    let node = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'schema34-legacy',$2,'fixture',1,now())")
        .bind(node).bind(node.to_string()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state) VALUES($1,$2,$3,$4,$5,'{\"label\":\"retained legacy fixture\"}')")
        .bind(attempt).bind(Uuid::new_v4()).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,artifact_id,import_operation_id,manifest,state,receipt) VALUES($1,$2,$3,$4,$5,$6,'{\"label\":\"retained source fixture\"}','retained','{\"retained\":true}')")
        .bind(Uuid::new_v4()).bind(attempt).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&pool).await.unwrap();
    let preserved = [
        "users",
        "nodes",
        "project_execution_bindings",
        "execution_attempts",
        "source_transfers",
    ];
    let mut before = Vec::new();
    for table in preserved {
        before.push(rows(&pool, table).await);
    }
    migrate(&pool).await.unwrap();
    for (table, expected) in preserved.into_iter().zip(before) {
        assert_eq!(
            rows(&pool, table).await,
            expected,
            "upgrade changed {table}"
        );
    }
    intent(&pool, actor, project).await;
    let mut before = Vec::new();
    let protected = [
        "meta",
        "schema_migrations",
        "human_graph_operations",
        "execution_attempts",
        "source_transfers",
    ];
    for table in protected {
        before.push(rows(&pool, table).await);
    }
    assert!(matches!(
        migrate_profile(&pool, 33).await,
        Err(StoreError::IncompatibleState {
            found: 34,
            supported: 33
        })
    ));
    for (table, expected) in protected.into_iter().zip(before) {
        assert_eq!(
            rows(&pool, table).await,
            expected,
            "old reader changed {table}"
        );
    }
}

async fn corruption_refused(sql: &str) {
    let (_db, pool) = database(34).await;
    sqlx::query(sql).execute(&pool).await.unwrap();
    let before = catalog(&pool).await;
    let ledger = rows(&pool, "schema_migrations").await;
    let meta = rows(&pool, "meta").await;
    assert!(
        matches!(migrate(&pool).await, Err(StoreError::Corrupt(message))
        if message.contains("human Graph intent journal")),
        "startup must reject {sql}"
    );
    assert_eq!(catalog(&pool).await, before, "startup repaired {sql}");
    assert_eq!(rows(&pool, "schema_migrations").await, ledger);
    assert_eq!(rows(&pool, "meta").await, meta);
}

#[tokio::test]
async fn missing_table_index_or_function_is_corrupt_without_repair() {
    for sql in [
        "DROP TABLE human_graph_operations",
        "DROP INDEX human_graph_operation_actor",
        "DROP FUNCTION guard_human_graph_operation_identity() CASCADE",
    ] {
        corruption_refused(sql).await;
    }
}

#[tokio::test]
async fn each_missing_history_guard_is_corrupt_without_repair() {
    for guard in ["identity", "delete", "truncate"] {
        corruption_refused(&format!(
            "DROP TRIGGER human_graph_operation_{guard}_guard ON human_graph_operations"
        ))
        .await;
    }
}

#[tokio::test]
async fn each_disabled_history_guard_is_corrupt_without_repair() {
    for guard in ["identity", "delete", "truncate"] {
        corruption_refused(&format!(
            "ALTER TABLE human_graph_operations DISABLE TRIGGER human_graph_operation_{guard}_guard"
        ))
        .await;
    }
}

#[tokio::test]
async fn intent_identity_is_immutable_and_dispatch_count_cannot_decrease() {
    let (_db, pool) = database(34).await;
    let (actor, project) = principals(&pool).await;
    let (other_actor, other_project) = principals(&pool).await;
    let operation = intent(&pool, actor, project).await;
    let before = rows(&pool, "human_graph_operations").await;
    let assignments = [
        format!("operation_id='{}',task_id='{}'", Uuid::nil(), Uuid::nil()),
        format!("actor_id='{other_actor}'"),
        format!("project_id='{other_project}'"),
        "project_revision=2".into(),
        "project_binding='{\"changed\":true}'".into(),
        format!("workspace_id='{}'", Uuid::new_v4()),
        format!("task_id='{}'", Uuid::new_v4()),
        "request_bytes='changed'::bytea".into(),
        format!("request_digest='sha256:{}'", "b".repeat(64)),
        "payload_bytes='changed'::bytea".into(),
        format!("payload_digest='sha256:{}'", "b".repeat(64)),
        "service_identity='{\"changed\":true}'".into(),
        "created_at_ms=1001".into(),
        "expires_at_ms=2001".into(),
        "retained_bytes=3".into(),
    ];
    for assignment in assignments {
        reject_assignment(
            &pool,
            operation,
            &assignment,
            "P0001",
            "identity is immutable",
        )
        .await;
        assert_eq!(
            rows(&pool, "human_graph_operations").await,
            before,
            "{assignment}"
        );
    }
    sqlx::query("UPDATE human_graph_operations SET status='{\"state\":\"unknown\"}',dispatch_count=1 WHERE operation_id=$1")
        .bind(operation).execute(&pool).await.unwrap();
    let error =
        sqlx::query("UPDATE human_graph_operations SET dispatch_count=0 WHERE operation_id=$1")
            .bind(operation)
            .execute(&pool)
            .await
            .unwrap_err();
    rejected(error, "P0001", "dispatch history cannot decrease");
}

#[tokio::test]
async fn status_shape_terminal_rows_and_destructive_history_guards() {
    let (_db, pool) = database(34).await;
    let (actor, project) = principals(&pool).await;
    let operation = intent(&pool, actor, project).await;
    for assignment in [
        "status='null'",
        "status='{}'",
        "status='{\"state\":null}'",
        "status='{\"state\":3}'",
        "status='{\"state\":\"invented\"}'",
        "status='{\"state\":\"confirmed\"}'",
        "status='{\"state\":\"confirmed\"}',raw_receipt='null'",
        "status='{\"state\":\"confirmed\"}',raw_receipt='[]'",
        "raw_receipt='{}'",
    ] {
        reject_assignment(&pool, operation, assignment, "23514", "check constraint").await;
    }
    assert_terminal_rows_immutable(&pool, actor, project).await;
    let before = rows(&pool, "human_graph_operations").await;
    for sql in [
        "DELETE FROM human_graph_operations",
        "TRUNCATE human_graph_operations",
    ] {
        rejected(
            sqlx::query(sql).execute(&pool).await.unwrap_err(),
            "P0001",
            "history cannot be deleted",
        );
        assert_eq!(rows(&pool, "human_graph_operations").await, before);
    }
}
