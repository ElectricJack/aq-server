//! Spec 0008 fixture seeding: relational-only sealed history rows written with
//! triggers and FKs disabled (`session_replication_role='replica'`). Never runnable
//! authority; only for counting tests.
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

pub async fn replica(pool: &PgPool) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

/// `status` None stores `{}`; Some("finished") stores `{"status":"finished"}`.
pub async fn attempt(c: &mut PgConnection, holds_task: bool, status: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),gen_random_uuid(),$2,CASE WHEN $3::text IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('status',$3::text) END)")
        .bind(id).bind(holds_task).bind(status).execute(&mut *c).await.unwrap();
    id
}

/// One offered launch: its source observation at `at_ms` and a launch binding on `node`.
pub async fn offer(
    c: &mut PgConnection,
    attempt: Uuid,
    decision: Uuid,
    node: Uuid,
    project: Uuid,
    at_ms: i64,
) {
    let observation = Uuid::new_v4();
    sqlx::query("INSERT INTO admission_source_observations(observation_id,project_id,source_policy_revision,observation,observed_at_ms,expires_at_ms) VALUES($1,$2,1,'{}'::jsonb,$3,$3+30000)")
        .bind(observation).bind(project).bind(at_ms).execute(&mut *c).await.unwrap();
    sqlx::query("INSERT INTO profiled_launches(attempt_id,decision_id,source_observation_id,launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms) VALUES($1,$2,$3,jsonb_build_object('session',jsonb_build_object('node_id',$4::text)),'{}'::jsonb,repeat('0',64),'{}'::jsonb,repeat('0',64),'{}'::jsonb,'{}'::jsonb,$5+60000)")
        .bind(attempt).bind(decision).bind(observation).bind(node).bind(at_ms).execute(&mut *c).await.unwrap();
}

/// `n` offers on `node`/`project` at `at_ms`, with fresh attempt and decision ids.
pub async fn offers(pool: &PgPool, node: Uuid, project: Uuid, at_ms: i64, n: usize) {
    let mut tx = replica(pool).await;
    for _ in 0..n {
        offer(
            &mut tx,
            Uuid::new_v4(),
            Uuid::new_v4(),
            node,
            project,
            at_ms,
        )
        .await;
    }
    tx.commit().await.unwrap();
}

/// A one-byte (`x`) admission artifact of `project` written at `created_at_ms`.
pub async fn artifact(c: &mut PgConnection, project: Uuid, created_at_ms: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO admission_artifacts(artifact_id,descriptor,content_digest,byte_length,owner_kind,owner_key,body,created_at_ms,retain_until_ms) VALUES($1,'{}'::jsonb,encode(sha256(convert_to($1::text,'UTF8')),'hex'),1,'graph_snapshot',$2,decode('78','hex'),$3,$3+86400000)")
        .bind(id).bind(project.to_string()).bind(created_at_ms).execute(&mut *c).await.unwrap();
    id
}

pub async fn link_artifact(c: &mut PgConnection, attempt: Uuid, artifact: Uuid) {
    sqlx::query("INSERT INTO profiled_launch_artifacts(attempt_id,artifact_id) VALUES($1,$2)")
        .bind(attempt)
        .bind(artifact)
        .execute(&mut *c)
        .await
        .unwrap();
}

/// A human-owner decision whose `inputs.now_ms` is `now_ms`.
pub async fn decision(c: &mut PgConnection, now_ms: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO intelligence_decisions(decision_id,actor_id,operation_id,payload_digest,decision) VALUES($1,gen_random_uuid(),gen_random_uuid(),repeat('0',64),jsonb_build_object('inputs',jsonb_build_object('now_ms',$2::bigint)))")
        .bind(id).bind(now_ms).execute(&mut *c).await.unwrap();
    id
}

/// A source export: `pending` (charged 1048577) or `verified` (charged `charged`).
pub async fn export(c: &mut PgConnection, project: Uuid, state: &str, charged: i64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO profiled_source_exports(operation_id,project_id,request_bytes,request_digest,reserved_bytes,charged_bytes,state,verified_bytes,verified_digest) VALUES($1,$2,convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),1048577,$3,$4::text,CASE WHEN $4::text='verified' THEN convert_to('{}','UTF8') END,CASE WHEN $4::text='verified' THEN encode(sha256(convert_to('{}','UTF8')),'hex') END)")
        .bind(id).bind(project).bind(charged).bind(state).execute(&mut *c).await.unwrap();
    id
}

pub async fn link_export(c: &mut PgConnection, attempt: Uuid, export: Uuid) {
    sqlx::query(
        "INSERT INTO profiled_launch_source_exports(attempt_id,operation_id) VALUES($1,$2)",
    )
    .bind(attempt)
    .bind(export)
    .execute(&mut *c)
    .await
    .unwrap();
}

pub async fn phase_capacity(c: &mut PgConnection, attempt: Uuid, node: Uuid) {
    sqlx::query("INSERT INTO profiled_phase_terminal_capacity(attempt_id,node_id) VALUES($1,$2)")
        .bind(attempt)
        .bind(node)
        .execute(&mut *c)
        .await
        .unwrap();
}

pub async fn registration(c: &mut PgConnection, attempt: Uuid, node: Uuid) {
    sqlx::query("INSERT INTO profiled_launch_registrations(attempt_id,registration_id,node_id,boot_id) VALUES($1,gen_random_uuid(),$2,gen_random_uuid())")
        .bind(attempt).bind(node).execute(&mut *c).await.unwrap();
}

/// An inventory report of `node` recorded at `at_ms` (expires 29 s later). Each
/// call needs a distinct `sequence`.
pub async fn inventory(c: &mut PgConnection, node: Uuid, boot: Uuid, sequence: i64, at_ms: i64) {
    sqlx::query("INSERT INTO profiled_presence_operations(node_id,operation_id,kind,boot_id,sequence,inventory_revision,compatibility_epoch,snapshot_digest,request_bytes,request_digest,receipt_bytes,receipt_digest,recorded_at_ms,expires_at_ms) VALUES($1,gen_random_uuid(),'inventory_v2',$2,$3,$3,1,repeat('a',64),convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),$4,$4+29000)")
        .bind(node).bind(boot).bind(sequence).bind(at_ms).execute(&mut *c).await.unwrap();
}

/// `sealed_history_usage(kind, scope, now)` as (total_rows, total_bytes, scope_rows, scope_bytes).
pub async fn usage(
    pool: &PgPool,
    kind: &str,
    scope: Option<Uuid>,
    now_ms: i64,
    exclude: Option<Uuid>,
) -> (i64, i64, i64, i64) {
    sqlx::query_as("SELECT total_rows,total_bytes,scope_rows,scope_bytes FROM sealed_history_usage($1,$2,$3,$4)")
        .bind(kind).bind(scope).bind(now_ms).bind(exclude).fetch_one(pool).await.unwrap()
}
