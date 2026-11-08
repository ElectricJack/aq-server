//! Actual schema32 accepted/issued authority; synthetic Graph/Git/worker evidence.
//! Manual draft33 migration tests do not qualify terminal producer or activation.
use crate::common;
#[allow(dead_code)] // Shared actual-owner fixture also serves its full14-case target.
#[path = "../common/phase_execution.rs"]
mod fixture;
use aq_server::profiled_execution::phase_execution;
use fixture::{Prepared, auth, prepared};
use sqlx::{Connection, Row};
use uuid::Uuid;
const SQL: &str = include_str!("../../migrations/0033_phase_terminal.sql");
async fn issue(f: &Prepared) {
    phase_execution::prepare(
        &f.c.env.env.state,
        &auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
}
async fn migrate(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(SQL).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO schema_migrations(version) VALUES(33)")
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}
async fn history(pool: &sqlx::PgPool) -> Vec<u8> {
    sqlx::query_scalar(
        "SELECT convert_to(row_to_json(g)::text,'UTF8') FROM profiled_phase_execution_grants g",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}
#[tokio::test]
async fn actual32_history_survives33_and_reserves_cleanup_without_terminal_effects() {
    let f = aq_server::test_support::with_state32_store(Box::pin(prepared(true))).await;
    issue(&f).await;
    let pool = &f.c.env.env.pool;
    let before = history(pool).await;
    let mut old = pool.acquire().await.unwrap(); // Genuine pre-migration connection.
    let old_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *old)
        .await
        .unwrap();
    migrate(pool).await.unwrap();
    assert_eq!(history(pool).await, before);
    let row = sqlx::query("SELECT reserved_bytes,node_id FROM profiled_phase_terminal_capacity")
        .fetch_one(&mut *old)
        .await
        .unwrap();
    assert_eq!(row.get::<i64, _>("reserved_bytes"), 1048576);
    let new_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *old)
        .await
        .unwrap();
    assert_eq!(old_pid, new_pid);
    let counts:(i64,i64,i64,bool)=sqlx::query_as("SELECT (SELECT count(*) FROM profiled_started_receipts),(SELECT count(*) FROM profiled_stopped_receipts),(SELECT count(*) FROM profiled_launch_closures),(SELECT holds_task FROM execution_attempts LIMIT 1)").fetch_one(pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0, true));
    // Maximum declared variable bytes, conservatively charging all three events
    // even though Started and suppressed closure cannot both own a launch.
    let extra:i64=sqlx::query_scalar("SELECT profiled_phase_terminal_extra_bytes('phase_execution_v2',repeat('a',64),repeat('b',64),convert_to(repeat('e',65536),'UTF8'),convert_to(repeat('a',65536),'UTF8'),to_jsonb(repeat('q',4094)))").fetch_one(pool).await.unwrap();
    let expected_extra = "phase_execution_v2".len() as i64 + 64 + 64 + 65536 + 65536 + 4096 + 128;
    assert_eq!(
        extra, expected_extra,
        "include both generated raw digest columns"
    );
    let started = 16384_i64 + 16384 + 64 + extra;
    let stopped = 16384_i64 + 16384 + 16384 + 128 + extra;
    let closure = 32768_i64 + 65536 + 16384 + 8192 + 4096 + 256 + extra;
    assert!(
        started + stopped + closure < 1048576,
        "all declared variable byte maxima must fit each reservation"
    );
}
#[tokio::test]
async fn collision_rolls_back33_without_changing_actual32_history() {
    let f = aq_server::test_support::with_state32_store(Box::pin(prepared(true))).await;
    issue(&f).await;
    let pool = &f.c.env.env.pool;
    let before = history(pool).await;
    sqlx::query("CREATE FUNCTION profiled_phase_terminal_used_bytes(attempt UUID) RETURNS BIGINT LANGUAGE SQL AS $$ SELECT 7::BIGINT $$")
        .execute(pool)
        .await
        .unwrap();
    assert!(migrate(pool).await.is_err());
    assert_eq!(history(pool).await, before);
    let (version,columns,applied):(String,i64,i64)=sqlx::query_as("SELECT (SELECT value FROM meta WHERE key='state_format_version'),(SELECT count(*) FROM information_schema.columns WHERE table_name='profiled_started_receipts' AND column_name='initial_authority_kind'),(SELECT count(*) FROM schema_migrations WHERE version=33)").fetch_one(pool).await.unwrap();
    assert_eq!((version, columns, applied), ("32".into(), 0, 0));
    let capacity: bool =
        sqlx::query_scalar("SELECT to_regclass('profiled_phase_terminal_capacity') IS NOT NULL")
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(
        !capacity,
        "late function collision rolls back earlier table, backfill and columns"
    );
}
#[tokio::test]
async fn already_open_writer_cannot_insert_legacy_started_for_actual_phase_grant() {
    let f = aq_server::test_support::with_state32_store(Box::pin(prepared(true))).await;
    issue(&f).await;
    let pool = &f.c.env.env.pool;
    let mut old = pool.acquire().await.unwrap();
    migrate(pool).await.unwrap();
    let attempt = f.prepared.original.binding.attempt_id;
    let node = f.prepared.original.identity.node_id;
    let before:Vec<u8>=sqlx::query_scalar("SELECT convert_to(jsonb_build_object('hot',to_jsonb(h),'journal',(SELECT to_jsonb(j) FROM profiled_owner_journals j WHERE j.attempt_id=h.attempt_id))::text,'UTF8') FROM profiled_authority_hot h WHERE attempt_id=$1").bind(attempt).fetch_one(pool).await.unwrap();
    let mut old_tx = old.begin().await.unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{effects}','{\"kind\":\"pending\",\"count\":1}'::jsonb) WHERE attempt_id=$1").bind(attempt).execute(&mut *old_tx).await.unwrap();
    let changed:Vec<u8>=sqlx::query_scalar("SELECT convert_to(jsonb_build_object('hot',to_jsonb(h),'journal',(SELECT to_jsonb(j) FROM profiled_owner_journals j WHERE j.attempt_id=h.attempt_id))::text,'UTF8') FROM profiled_authority_hot h WHERE attempt_id=$1").bind(attempt).fetch_one(&mut *old_tx).await.unwrap();
    assert_ne!(
        before, changed,
        "the old transaction must actually mutate hot state before its insert is fenced"
    );
    let error=sqlx::query("INSERT INTO profiled_started_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,receipt_bytes,before_record_revision) VALUES($1,$2,$3,'{}',encode(sha256('{}'::bytea),'hex'),'{}',1)").bind(attempt).bind(node).bind(Uuid::new_v4()).execute(&mut *old_tx).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("qualified transaction-local phase terminal writer required"),
        "{error:?}"
    );
    old_tx.rollback().await.unwrap();
    let after:Vec<u8>=sqlx::query_scalar("SELECT convert_to(jsonb_build_object('hot',to_jsonb(h),'journal',(SELECT to_jsonb(j) FROM profiled_owner_journals j WHERE j.attempt_id=h.attempt_id))::text,'UTF8') FROM profiled_authority_hot h WHERE attempt_id=$1").bind(attempt).fetch_one(pool).await.unwrap();
    assert_eq!(
        before, after,
        "origin refusal rolls back the old writer's prior hot update and derived revisions, preserving its actual journal"
    );
    let stopped = sqlx::query("INSERT INTO profiled_stopped_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,proof_bytes,proof_digest,receipt_bytes,before_record_revision,before_grant_revision) VALUES($1,$2,$3,'{}',encode(sha256('{}'::bytea),'hex'),'{}',encode(sha256('{}'::bytea),'hex'),'{}',1,1)").bind(attempt).bind(node).bind(Uuid::new_v4()).execute(&mut *old).await.unwrap_err();
    assert!(
        stopped
            .to_string()
            .contains("qualified transaction-local phase terminal writer required"),
        "{stopped:?}"
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_started_receipts")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}
#[tokio::test]
async fn old_snapshot_classifies_actual030_origin_even_without_visible032_grant() {
    let f = aq_server::test_support::with_state32_store(Box::pin(prepared(true))).await;
    let pool = &f.c.env.env.pool;
    let mut old = pool.acquire().await.unwrap();
    let mut snapshot = old.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *snapshot)
        .await
        .unwrap();
    let before:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM profiled_v2_accepts),(SELECT count(*) FROM profiled_phase_execution_grants)").fetch_one(&mut *snapshot).await.unwrap();
    assert_eq!(before, (1, 0));
    issue(&f).await;
    migrate(pool).await.unwrap();
    let still: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_phase_execution_grants")
        .fetch_one(&mut *snapshot)
        .await
        .unwrap();
    assert_eq!(still, 0);
    let error=sqlx::query("INSERT INTO profiled_started_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,receipt_bytes,before_record_revision) VALUES($1,$2,$3,'{}',encode(sha256('{}'::bytea),'hex'),'{}',1)").bind(f.prepared.original.binding.attempt_id).bind(f.prepared.original.identity.node_id).bind(Uuid::new_v4()).execute(&mut *snapshot).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("P0002")
    );
    assert!(
        format!("{error:?}").contains("profiled_phase_terminal_origin_guard"),
        "must fail inside new origin classifier: {error:?}"
    );
    snapshot.rollback().await.unwrap();
}

#[tokio::test]
async fn relational_only_over_budget_backfill_rolls_back_entire_migration() {
    let f = aq_server::test_support::with_state32_store(Box::pin(prepared(true))).await;
    issue(&f).await;
    let pool = &f.c.env.env.pool;
    // Capacity boundary only: these additional rows deliberately are NOT runnable
    // owner-issued authority. Disable trigger/FK execution only in this disposable
    // transaction; never manufacture legacy history or change the actual seed.
    let mut seed = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *seed)
        .await
        .unwrap();
    sqlx::query("INSERT INTO profiled_phase_execution_grants(attempt_id,node_id,boot_id,operation_id,grant_id,registration_id,accepted_receipt_digest,request_digest,receipt_digest,request_bytes,receipt_bytes,provenance_bytes,issued_at_ms,expires_at_ms,execution_deadline_ms) SELECT gen_random_uuid(),node_id,boot_id,gen_random_uuid(),gen_random_uuid(),registration_id,accepted_receipt_digest,request_digest,receipt_digest,request_bytes,receipt_bytes,provenance_bytes,issued_at_ms,expires_at_ms,execution_deadline_ms FROM profiled_phase_execution_grants CROSS JOIN generate_series(1,16)").execute(&mut *seed).await.unwrap();
    seed.commit().await.unwrap();
    let before: String = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(g) ORDER BY attempt_id)::text FROM profiled_phase_execution_grants g").fetch_one(pool).await.unwrap();
    let error = migrate(pool).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("phase terminal capacity exhausted before grant issuance"),
        "{error:?}"
    );
    let after: String = sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(g) ORDER BY attempt_id)::text FROM profiled_phase_execution_grants g").fetch_one(pool).await.unwrap();
    assert_eq!(before, after);
    let (format, capacity, column):(String,bool,bool)=sqlx::query_as("SELECT (SELECT value FROM meta WHERE key='state_format_version'),to_regclass('profiled_phase_terminal_capacity') IS NOT NULL,EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='profiled_started_receipts' AND column_name='phase_ack_bytes')").fetch_one(pool).await.unwrap();
    assert_eq!((format, capacity, column), ("32".into(), false, false));
}

#[tokio::test]
async fn actual_grant_cannot_commit_without_reserved_terminal_capacity() {
    let f = aq_server::test_support::with_state32_store(Box::pin(prepared(true))).await;
    let pool = &f.c.env.env.pool;
    migrate(pool).await.unwrap();
    // Relational-only occupied capacity, not executable grants or scheduler slots.
    // The subsequently attempted grant below is through the actual phase owner.
    let mut occupied = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *occupied)
        .await
        .unwrap();
    // Spec 0008: capacity counts live attempts only, so the occupants hold their tasks.
    sqlx::query("WITH a AS (INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) SELECT gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),TRUE,'{}'::jsonb FROM generate_series(1,16) RETURNING attempt_id) INSERT INTO profiled_phase_terminal_capacity(attempt_id,node_id) SELECT attempt_id,$1 FROM a").bind(f.prepared.original.identity.node_id).execute(&mut *occupied).await.unwrap();
    occupied.commit().await.unwrap();
    let result = phase_execution::prepare(
        &f.c.env.env.state,
        &auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await;
    assert!(
        result.is_err(),
        "actual grant must roll back rather than outrun retained cleanup capacity"
    );
    fixture::assert_unissued(&f.c).await;
    let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM profiled_phase_terminal_capacity),(SELECT count(*) FROM profiled_owner_journals WHERE phase<>'reserved')").fetch_one(pool).await.unwrap();
    assert_eq!(counts, (16, 0));
}

/// Occupies `n` terminal reservations of the node, with attempts that hold their task
/// (`live`) or are released and finished, then asks the actual phase owner for a grant
/// on the current store (the frozen 32 fixture lacks tables the grant path reads today).
/// Returns whether the grant was issued and how many reservations exist afterwards.
async fn grant_with_occupants(n: i64, live: bool) -> (bool, i64) {
    let f = prepared(true).await;
    let pool = &f.c.env.env.pool;
    let (holds, state) = if live {
        ("TRUE", "{}")
    } else {
        ("FALSE", r#"{"status":"finished"}"#)
    };
    let mut occupied = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *occupied)
        .await
        .unwrap();
    sqlx::query(&format!("WITH a AS (INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) SELECT gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),{holds},'{state}'::jsonb FROM generate_series(1,{n}) RETURNING attempt_id) INSERT INTO profiled_phase_terminal_capacity(attempt_id,node_id) SELECT attempt_id,$1 FROM a")).bind(f.prepared.original.identity.node_id).execute(&mut *occupied).await.unwrap();
    occupied.commit().await.unwrap();
    let result = phase_execution::prepare(
        &f.c.env.env.state,
        &auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await;
    let count = sqlx::query_scalar("SELECT count(*) FROM profiled_phase_terminal_capacity")
        .fetch_one(pool)
        .await
        .unwrap();
    (result.is_ok(), count)
}

#[tokio::test]
async fn live_terminal_capacity_allows_the_sixteenth_reservation() {
    assert_eq!(grant_with_occupants(15, true).await, (true, 16));
}

#[tokio::test]
async fn live_terminal_capacity_refuses_the_seventeenth_reservation() {
    assert_eq!(grant_with_occupants(16, true).await, (false, 16));
}

#[tokio::test]
async fn settled_terminal_capacity_does_not_block_a_new_grant() {
    assert_eq!(grant_with_occupants(16, false).await, (true, 17));
}

#[path = "schema33_phase_terminal/legacy_parent.rs"]
mod legacy_parent;

#[path = "schema33_phase_terminal/ordered.rs"]
mod ordered;
