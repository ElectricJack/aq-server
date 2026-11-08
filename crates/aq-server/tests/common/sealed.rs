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

/// A source export as `export`, written at `created_at_ms` (the pending window).
pub async fn export_created(
    c: &mut PgConnection,
    project: Uuid,
    state: &str,
    charged: i64,
    created_at_ms: i64,
) -> Uuid {
    let id = export(c, project, state, charged).await;
    sqlx::query("UPDATE profiled_source_exports SET created_at_ms=$2 WHERE operation_id=$1")
        .bind(id)
        .bind(created_at_ms)
        .execute(&mut *c)
        .await
        .unwrap();
    id
}

/// An enrolled node row, for inserts that keep their foreign keys.
pub async fn node(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,last_seen_at) VALUES($1,$2,$3,'t',1,now(),now())")
        .bind(id).bind(id.to_string()).bind(id.to_string()).execute(pool).await.unwrap();
    id
}

/// `n` inventory reports for `node` at `at_ms`, written with triggers off.
pub async fn inventories(pool: &PgPool, node: Uuid, n: i64, at_ms: i64) {
    let mut tx = replica(pool).await;
    sqlx::query("INSERT INTO profiled_presence_operations(node_id,operation_id,kind,boot_id,sequence,inventory_revision,compatibility_epoch,snapshot_digest,request_bytes,request_digest,receipt_bytes,receipt_digest,recorded_at_ms,expires_at_ms) SELECT $1,gen_random_uuid(),'inventory_v2',gen_random_uuid(),g,g,1,repeat('a',64),convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),$3,$3+29000 FROM generate_series(1,$2) g")
        .bind(node).bind(n).bind(at_ms).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// One inventory report through the real `profiled_presence_operation_guard`.
/// Returns the SQLSTATE of a refusal.
pub async fn report(pool: &PgPool, node: Uuid, sequence: i64, at_ms: i64) -> Result<(), String> {
    let (op, boot) = (Uuid::new_v4(), Uuid::new_v4());
    let request = serde_json::json!({"schema_version":"1","operation_id":op,"identity":{"node_id":node,"boot_id":boot},"sequence":sequence});
    let request_bytes = serde_json::to_vec(&request).unwrap();
    let digest: String = sqlx::query_scalar("SELECT encode(sha256($1),'hex')")
        .bind(&request_bytes)
        .fetch_one(pool)
        .await
        .unwrap();
    let receipt = serde_json::json!({"schema_version":"1","inventory":{"request_digest":digest,"revision":sequence,"snapshot_digest":"a".repeat(64)},"identity":{"node_id":node,"boot_id":boot},"recorded_at_ms":at_ms,"expires_at_ms":at_ms+29000});
    let receipt_bytes = serde_json::to_vec(&receipt).unwrap();
    sqlx::query("INSERT INTO profiled_presence_operations(node_id,operation_id,kind,boot_id,sequence,inventory_revision,compatibility_epoch,snapshot_digest,request_bytes,request_digest,receipt_bytes,receipt_digest,recorded_at_ms,expires_at_ms) VALUES($1,$2,'inventory_v2',$3,$4,$4,1,repeat('a',64),$5,$6,$7,encode(sha256($7),'hex'),$8,$8+29000)")
        .bind(node).bind(op).bind(boot).bind(sequence).bind(&request_bytes).bind(&digest).bind(&receipt_bytes).bind(at_ms)
        .execute(pool).await.map(|_| ()).map_err(|e| e.as_database_error().and_then(|d| d.code().map(|c| c.to_string())).unwrap_or_else(|| e.to_string()))
}

/// The SQLSTATE of a launch insert for `node`/`project` at `at_ms` with triggers
/// on: `54000` is the offer-rate fence, `23503` means the fence passed and the
/// insert went on to fail on the decision foreign key.
pub async fn try_offer(pool: &PgPool, node: Uuid, project: Uuid, at_ms: i64) -> String {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let observation = Uuid::new_v4();
    sqlx::query("INSERT INTO admission_source_observations(observation_id,project_id,source_policy_revision,observation,observed_at_ms,expires_at_ms) VALUES($1,$2,1,'{}'::jsonb,$3,$3+30000)")
        .bind(observation).bind(project).bind(at_ms).execute(&mut *tx).await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='origin'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query("INSERT INTO profiled_launches(attempt_id,decision_id,source_observation_id,launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms) VALUES(gen_random_uuid(),gen_random_uuid(),$1,jsonb_build_object('session',jsonb_build_object('node_id',$2::text)),'{}'::jsonb,repeat('0',64),'{}'::jsonb,repeat('0',64),'{}'::jsonb,'{}'::jsonb,$3+60000)")
        .bind(observation).bind(node).bind(at_ms).execute(&mut *tx).await.unwrap_err();
    err.as_database_error().unwrap().code().unwrap().to_string()
}

/// `n` offers over distinct nodes of `project` (or distinct projects when
/// `project` is None) at `at_ms`: server-wide and per-project rate fixtures.
pub async fn spread_offers(pool: &PgPool, project: Option<Uuid>, n: i64, at_ms: i64) {
    let mut tx = replica(pool).await;
    sqlx::query("WITH s AS (SELECT gen_random_uuid() AS observation_id,gen_random_uuid() AS attempt_id,gen_random_uuid() AS node_id,COALESCE($1,gen_random_uuid()) AS project_id FROM generate_series(1,$2)), o AS (INSERT INTO admission_source_observations(observation_id,project_id,source_policy_revision,observation,observed_at_ms,expires_at_ms) SELECT observation_id,project_id,1,'{}'::jsonb,$3,$3+30000 FROM s) INSERT INTO profiled_launches(attempt_id,decision_id,source_observation_id,launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms) SELECT attempt_id,gen_random_uuid(),observation_id,jsonb_build_object('session',jsonb_build_object('node_id',node_id::text)),'{}'::jsonb,repeat('0',64),'{}'::jsonb,repeat('0',64),'{}'::jsonb,'{}'::jsonb,$3+60000 FROM s")
        .bind(project).bind(n).bind(at_ms).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// Settled history, written with triggers off: `attempts` finished runs (released,
/// hot status `finished`, a stale `offered` snapshot) each with an offer, a decision,
/// an artifact, an export, a registration, a terminal reservation, a proposal and an
/// expired lease, all `age_ms` old, plus `presence` old inventory reports.
pub async fn settled_history(
    pool: &PgPool,
    attempts: i64,
    presence: i64,
    now_ms: i64,
    age_ms: i64,
) {
    let old = now_ms - age_ms;
    let mut tx = replica(pool).await;
    sqlx::query(&format!("CREATE TEMP TABLE s ON COMMIT DROP AS SELECT gen_random_uuid() AS attempt_id,gen_random_uuid() AS node_id,gen_random_uuid() AS project_id,gen_random_uuid() AS decision_id,gen_random_uuid() AS observation_id,gen_random_uuid() AS artifact_id,gen_random_uuid() AS operation_id,gen_random_uuid() AS lease_id,g FROM generate_series(1,{attempts}) g"))
        .execute(&mut *tx).await.unwrap();
    for sql in [
        "INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,kind,state) SELECT attempt_id,gen_random_uuid(),node_id,gen_random_uuid(),gen_random_uuid(),FALSE,'profiled_v1',jsonb_build_object('schema_version','1','status','offered') FROM s",
        "INSERT INTO intelligence_decisions(decision_id,actor_id,operation_id,payload_digest,decision) SELECT decision_id,gen_random_uuid(),gen_random_uuid(),repeat('0',64),jsonb_build_object('inputs',jsonb_build_object('now_ms',$1::bigint)) FROM s",
        "INSERT INTO admission_source_observations(observation_id,project_id,source_policy_revision,observation,observed_at_ms,expires_at_ms) SELECT observation_id,project_id,1,'{}'::jsonb,$1,$1+30000 FROM s",
        "INSERT INTO profiled_launches(attempt_id,decision_id,source_observation_id,launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms) SELECT attempt_id,decision_id,observation_id,jsonb_build_object('session',jsonb_build_object('node_id',node_id::text)),'{}'::jsonb,repeat('0',64),'{}'::jsonb,repeat('0',64),'{}'::jsonb,'{}'::jsonb,$1+60000 FROM s",
        "INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) SELECT attempt_id,jsonb_build_object('schema_version','1','status','finished'),1,1,0 FROM s",
        "INSERT INTO admission_artifacts(artifact_id,descriptor,content_digest,byte_length,owner_kind,owner_key,body,created_at_ms,retain_until_ms) SELECT artifact_id,'{}'::jsonb,encode(sha256(convert_to(artifact_id::text,'UTF8')),'hex'),1,'graph_snapshot',project_id::text,decode('78','hex'),$1,$1+86400000 FROM s",
        "INSERT INTO profiled_launch_artifacts(attempt_id,artifact_id) SELECT attempt_id,artifact_id FROM s",
        "INSERT INTO profiled_source_exports(operation_id,project_id,request_bytes,request_digest,reserved_bytes,charged_bytes,state,verified_bytes,verified_digest,created_at_ms) SELECT operation_id,project_id,convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),1048577,5000,'verified',convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),$1 FROM s",
        "INSERT INTO profiled_launch_source_exports(attempt_id,operation_id) SELECT attempt_id,operation_id FROM s",
        "INSERT INTO profiled_launch_registrations(attempt_id,registration_id,node_id,boot_id) SELECT attempt_id,gen_random_uuid(),node_id,gen_random_uuid() FROM s",
        "INSERT INTO profiled_phase_terminal_capacity(attempt_id,node_id) SELECT attempt_id,node_id FROM s",
        "INSERT INTO profiled_v2_proposals(attempt_id,node_id,boot_id,policy,proposal,proposal_digest,published_at_ms) SELECT attempt_id,node_id,gen_random_uuid(),'{}'::jsonb,'{}'::jsonb,repeat('0',64),$1 FROM s",
        "INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,admitted_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) SELECT lease_id,gen_random_uuid(),repeat('0',64),node_id,gen_random_uuid(),attempt_id,NULL,gen_random_uuid(),'settled',1,'settled','{}'::jsonb,md5(g::text)||md5((g+7)::text),'expired',1,2 FROM s",
    ] {
        let mut q = sqlx::query(sql);
        if sql.contains("$1") {
            q = q.bind(old);
        }
        q.execute(&mut *tx).await.unwrap();
    }
    sqlx::query("INSERT INTO profiled_presence_operations(node_id,operation_id,kind,boot_id,sequence,inventory_revision,compatibility_epoch,snapshot_digest,request_bytes,request_digest,receipt_bytes,receipt_digest,recorded_at_ms,expires_at_ms) SELECT gen_random_uuid(),gen_random_uuid(),'inventory_v2',gen_random_uuid(),g,g,1,repeat('a',64),convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),convert_to('{}','UTF8'),encode(sha256(convert_to('{}','UTF8')),'hex'),$2,$2+29000 FROM generate_series(1,$1) g")
        .bind(presence).bind(old).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    for t in [
        "execution_attempts",
        "profiled_authority_hot",
        "profiled_launches",
        "admission_source_observations",
        "intelligence_decisions",
        "admission_artifacts",
        "profiled_launch_artifacts",
        "profiled_source_exports",
        "profiled_launch_source_exports",
        "profiled_launch_registrations",
        "profiled_phase_terminal_capacity",
        "profiled_v2_proposals",
        "admission_material_leases",
        "profiled_presence_operations",
    ] {
        sqlx::query(&format!("VACUUM ANALYZE {t}"))
            .execute(pool)
            .await
            .unwrap();
    }
}

pub async fn proposal(c: &mut PgConnection, attempt: Uuid, node: Uuid) {
    sqlx::query("INSERT INTO profiled_v2_proposals(attempt_id,node_id,boot_id,policy,proposal,proposal_digest,published_at_ms) VALUES($1,$2,gen_random_uuid(),'{}'::jsonb,'{}'::jsonb,repeat('0',64),1)")
        .bind(attempt).bind(node).execute(&mut *c).await.unwrap();
}
