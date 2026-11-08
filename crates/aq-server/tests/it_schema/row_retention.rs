//! DEP6 uses disposable PostgreSQL, fake Graph state and caller-supplied times.
use aq_server::{
    row_retention::{self, TaskState},
    test_support::TestDb,
};
use serde_json::json;
use sqlx::PgPool;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;
const DAY: i64 = 86_400_000;
const START: i64 = 100 * DAY;
struct Tasks(AtomicBool);
#[async_trait::async_trait]
impl TaskState for Tasks {
    async fn node(&self, _workspace: Uuid, _task: Uuid) -> anyhow::Result<serde_json::Value> {
        let state = if self.0.load(Ordering::SeqCst) {
            "completed"
        } else {
            "active"
        };
        Ok(json!({"task_state":state,"deleted":false,"retired":false}))
    }
}
struct Env {
    _db: TestDb,
    pool: PgPool,
    node: Uuid,
    task: Uuid,
    workspace: Uuid,
    attempt: Uuid,
    tasks: Tasks,
}
impl Env {
    async fn new() -> Self {
        let db = TestDb::create().await;
        let _store = db.store().await;
        let pool = db.pool().await;
        let node = Uuid::new_v4();
        let task = Uuid::new_v4();
        let workspace = Uuid::new_v4();
        let attempt = Uuid::new_v4();
        sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'retention','retention-hash','0.1.0',1,now())").bind(node).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,$2,$3,$4,$5,false,$6)")
            .bind(attempt).bind(task).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(json!({"status":"finished"})).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,'{}','{}')")
            .bind(attempt).bind(node).bind(json!({"workspace_id":workspace})).execute(&pool).await.unwrap();
        Self {
            _db: db,
            pool,
            node,
            task,
            workspace,
            attempt,
            tasks: Tasks(AtomicBool::new(true)),
        }
    }
    async fn collect(&self, now: i64) -> row_retention::Report {
        row_retention::collect(&self.pool, &self.tasks, now, 2)
            .await
            .unwrap()
    }
    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn observation_retention_batches_history_and_preserves_authority() {
    let e = Env::new().await;
    let lineage = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO observation_nodes(node_id,event_count,retained_bytes) VALUES($1,6,12000)",
    )
    .bind(e.node)
    .execute(&e.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO observation_call_heads(call_key,fact,owner_attempt_id,lineage_id) VALUES($1,'{}',$2,$3)").bind("a".repeat(64)).bind(e.attempt).bind(lineage).execute(&e.pool).await.unwrap();
    for seq in 1..=6i64 {
        let event = Uuid::new_v4();
        sqlx::query("INSERT INTO observation_events(event_id,node_id,sequence,lineage_id,attempt_id,recorded_at_ms,record,decision) VALUES($1,$2,$3,$4,$5,$6,'{}','{}')")
            .bind(event).bind(e.node).bind(seq).bind(lineage).bind(e.attempt).bind(if seq==6 {START+60*DAY} else {START-DAY}).execute(&e.pool).await.unwrap();
        if seq < 6 {
            sqlx::query("INSERT INTO observation_facts(event_id,call_key,fact) VALUES($1,$2,'{}')")
                .bind(event)
                .bind("a".repeat(64))
                .execute(&e.pool)
                .await
                .unwrap();
        }
    }
    let packet = Uuid::new_v4();
    sqlx::query("INSERT INTO observation_packets(node_id,message_id,body_digest,receipt) VALUES($1,$2,$3,'{\"through_sequence\":5}')").bind(e.node).bind(packet).bind("a".repeat(64)).execute(&e.pool).await.unwrap();
    sqlx::query("INSERT INTO observation_packet_retention(node_id,message_id,first_sequence,last_sequence,attempt_ids,recorded_at_ms,charged_bytes) VALUES($1,$2,1,5,$3,$4,12000)")
        .bind(e.node).bind(packet).bind(vec![e.attempt]).bind(START-DAY).execute(&e.pool).await.unwrap();
    assert_eq!(e.collect(START).await["observation_events"], 0); // starts the clock
    e.tasks.0.store(false, Ordering::SeqCst);
    assert_eq!(e.collect(START + 15 * DAY).await["observation_events"], 0); // open Graph task
    e.tasks.0.store(true, Ordering::SeqCst);
    e.collect(START + 16 * DAY).await;
    sqlx::query("UPDATE execution_attempts SET holds_task=true WHERE attempt_id=$1")
        .bind(e.attempt)
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(e.collect(START + 31 * DAY).await["observation_events"], 0); // unsettled attempt
    sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
        .bind(e.attempt)
        .execute(&e.pool)
        .await
        .unwrap();
    e.collect(START + 32 * DAY).await;
    for expected in [2, 2, 1] {
        let r = e.collect(START + 47 * DAY).await;
        assert_eq!(r["observation_events"], expected);
        assert_eq!(r["observation_facts"], expected);
    }
    assert_eq!(e.count("observation_events").await, 1); // recent record
    assert_eq!(e.count("observation_packets").await, 0);
    assert_eq!(e.count("observation_call_heads").await, 1); // still referenced by remaining history
    let counters: (i64, i64) =
        sqlx::query_as("SELECT event_count,retained_bytes FROM observation_nodes WHERE node_id=$1")
            .bind(e.node)
            .fetch_one(&e.pool)
            .await
            .unwrap();
    assert_eq!(counters, (1, 0));
    assert_eq!(e.count("retention_closed_attempts").await, 1);
    assert!(
        sqlx::query("DELETE FROM observation_events")
            .execute(&e.pool)
            .await
            .is_err()
    ); // normal writes remain immutable
}

#[tokio::test]
async fn outbox_retention_ages_completed_fanout_and_returns_capacity_in_batches() {
    let e = Env::new().await;
    let owner = Uuid::new_v4();
    let namespace = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,'retention@localhost','owner',now())").bind(owner).execute(&e.pool).await.unwrap();
    sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest,outbox_count,retained_bytes) VALUES($1,1,'retention',$2,$3,$4,$5,$5,7,28784)")
        .bind(namespace).bind(e.workspace).bind(Uuid::new_v4()).bind(owner).bind("a".repeat(64)).execute(&e.pool).await.unwrap();
    for i in 0..7 {
        sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status,fanout_complete) VALUES($1,1,'aq.host.bridge@1',0,$2,$3,1,'retention',$4,$5,$6,$7,$8)")
            .bind(namespace).bind(Uuid::new_v4()).bind("a".repeat(64)).bind(vec![0u8;12]).bind(vec![0u8;16]).bind(START-20*DAY)
            .bind(if i==5 {"pending"} else {"engine_accepted"}).bind(i!=6).execute(&e.pool).await.unwrap();
    }
    // Metadata is bounded too: three passes observe all five completed rows.
    for _ in 0..3 {
        assert_eq!(e.collect(START).await["playbook_outbox"], 0);
    }
    for expected in [2, 2, 1] {
        assert_eq!(
            e.collect(START + 8 * DAY).await["playbook_outbox"],
            expected
        );
    }
    assert_eq!(e.count("playbook_outbox").await, 2); // pending and unfinished fanout
    let counters: (i64, i64) = sqlx::query_as(
        "SELECT outbox_count,retained_bytes FROM playbook_namespaces WHERE namespace_id=$1",
    )
    .bind(namespace)
    .fetch_one(&e.pool)
    .await
    .unwrap();
    assert_eq!(counters, (2, 8224));
}

#[tokio::test]
async fn mailbox_retention_keeps_unread_fresh_and_open_task_mail() {
    let e = Env::new().await;
    sqlx::query("INSERT INTO mailbox_heads(mailbox_key) VALUES('retention')")
        .execute(&e.pool)
        .await
        .unwrap();
    for i in 1..=7i64 {
        let message = Uuid::new_v4();
        sqlx::query("INSERT INTO mailbox_messages(message_id,mailbox_key,cursor,thread_id,sender,recipient,payload_digest,body,created_at_ms) VALUES($1,'retention',$2,$3,'{}',$4,'digest','body',$5)")
            .bind(message).bind(i).bind(Uuid::new_v4()).bind(json!({"kind":"task","workspace_id":e.workspace,"task_id":e.task}))
            .bind(if i==7 {START+60*DAY} else {START-DAY}).execute(&e.pool).await.unwrap();
        if i != 6 {
            sqlx::query("INSERT INTO mailbox_receipts(message_id,attempt_id,disposition,acknowledged_at_ms) VALUES($1,$2,'read',$3)")
            .bind(message).bind(e.attempt).bind(if i==7 {START+60*DAY} else {START-DAY}).execute(&e.pool).await.unwrap();
        }
    }
    e.collect(START).await;
    e.tasks.0.store(false, Ordering::SeqCst);
    assert_eq!(e.collect(START + 31 * DAY).await["mailbox_messages"], 0);
    e.tasks.0.store(true, Ordering::SeqCst);
    e.collect(START + 32 * DAY).await;
    for expected in [2, 2, 1] {
        assert_eq!(
            e.collect(START + 63 * DAY).await["mailbox_messages"],
            expected
        );
    }
    assert_eq!(e.count("mailbox_messages").await, 2); // unread and recent-read
    assert_eq!(e.count("mailbox_receipts").await, 1);
    assert_eq!(e.count("mailbox_heads").await, 1); // cursors never reset
}
