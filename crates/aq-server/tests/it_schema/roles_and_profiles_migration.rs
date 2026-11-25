//! Migration 0134 (spec 0014 §9, §10): from the format before it, the old
//! catalog's tables go, the routing document moves to the role store,
//! bindings lose their catalog fields and gain the two defaults, attempts
//! from before get a summary of what they ran with, and attempt history is
//! kept row for row.
use aq_server::{
    project_bindings::domain::ProjectBinding,
    test_support::{TestDb, migrate_profile},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const MIGRATION: &str = include_str!("../../migrations/0134_roles_and_profiles.sql");
const HISTORY: [&str; 4] = [
    "execution_attempts",
    "intelligence_decisions",
    "intelligence_attempt_selections",
    "profiled_launches",
];
const REMOVED: [&str; 7] = [
    "profile_catalog_artifacts",
    "profile_catalog_registry",
    "profile_catalog_operations",
    "profile_catalog_environment_templates",
    "context_qualification_renewals",
    "intelligence_structure",
    "intelligence_structure_operations",
];

async fn exists(pool: &PgPool, table: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// Every row of `table` as text, in a stable order.
async fn rows(pool: &PgPool, table: &str) -> Vec<String> {
    sqlx::query_scalar(&format!("SELECT t::text FROM {table} t ORDER BY t::text"))
        .fetch_all(pool)
        .await
        .unwrap()
}
fn old_binding(project: Uuid) -> Value {
    let hex = "a".repeat(64);
    json!({
        "schema_version": 1, "project_id": project, "name": "Old project", "enabled": true,
        "repository_id": Uuid::from_u128(7), "repository_binding_generation": 3,
        "repository_adapter_reference": "aq.git.configured",
        "graph_adapter_reference": "aq.graph.configured",
        "graph_workspace_id": Uuid::from_u128(8),
        "catalog_digest": hex,
        "allowed_agent_profiles": ["implementer@2"],
        "allowed_intelligence_profiles": ["engineering@2", "router@1"],
        "allowed_execution_profiles": ["confined-development@2"],
        "allowed_node_ids": [Uuid::from_u128(9)], "allowed_worker_groups": [],
        "allowed_environments": ["confined-worktree"],
        "policy": {"identity": "aq.project-policy", "revision": "1", "digest": hex},
        "context": [],
        "ceilings": {"permissions": [{"operation": "task.read", "scope_reference": "project"}],
            "context_bytes": 65536, "wall_time_ms": 3600000, "memory_bytes": 1073741824u64,
            "cpu_millis": 4000, "process_limit": 64, "cost_limit_microunits": null,
            "currency": null, "require_hard_cost_enforcement": false},
    })
}
/// A launch manifest reduced to what the summary is filled from.
fn manifest(agent: &str, level: &str, model: &str) -> Value {
    let reference = |id: &str| json!({"id": id, "version": "2", "digest": "b".repeat(64)});
    json!({
        "agent_profile": reference(agent),
        "intelligence_profile": reference(level),
        "execution_profile": reference("execution:confined-development@2"),
        "model": {"provider": "anthropic", "model": model, "reasoning": "high", "max_output_tokens": 128},
        "harness": {"harness": "claude", "cli_version": "1"},
    })
}

struct HistoricalAttempts {
    old_attempt: Uuid,
    supervisor: Uuid,
    renamed_supervisor: Uuid,
    stopped_supervisor: Uuid,
}

async fn seed_principals(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let (owner, node, project) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query(
        "INSERT INTO users(id,email,role,created_at) VALUES($1,'roles@localhost','owner',now())",
    )
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'roles-node',$2,'0.3.3',2,now())")
        .bind(node)
        .bind("c".repeat(64))
        .execute(pool)
        .await
        .unwrap();
    (owner, node, project)
}

async fn seed_catalog(pool: &PgPool, owner: Uuid, node: Uuid) {
    // The old catalog, its environment pins, a measurement and the structure.
    let digest = "a".repeat(64);
    sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) VALUES($1,'{}',$2,1)")
        .bind(&digest)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE profile_catalog_registry SET revision=1,active_digest=$1")
        .bind(&digest)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO profile_catalog_environment_templates(catalog_digest,environment,template_digest) VALUES($1,'confined-worktree',$1)")
        .bind(&digest)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO context_qualification_renewals(renderer_digest,base_renderer_digest,fact,observed_at_ms,expires_at_ms,node_id,submission,created_at) VALUES($1,$1,'{}',1,2,$2,'{}',now())")
        .bind(&digest)
        .bind(node)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE intelligence_structure SET revision=4,document=jsonb_set(document,'{routing_markdown}',to_jsonb($1::text))")
        .bind("# Routing\nReviews go to deep-high.\n")
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_bindings(pool: &PgPool, owner: Uuid, project: Uuid) -> Value {
    let digest = "a".repeat(64);
    // A binding with the old fields, in each place a read returns it.
    let binding = old_binding(project);
    let record = json!({"revision": 2, "binding": binding, "validated_at_ms": 5, "validation": {}});
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,2,$2,5,'{}')")
        .bind(project)
        .bind(&binding)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_execution_binding_history(project_id,revision,record,actor_id,created_at_ms) VALUES($1,2,$2,$3,5)")
        .bind(project)
        .bind(&record)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO project_execution_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)")
        .bind(owner)
        .bind(Uuid::new_v4())
        .bind(&digest)
        .bind(json!({"operation_id": Uuid::new_v4(), "record": record, "disposition": "bound"}))
        .execute(pool)
        .await
        .unwrap();
    binding
}

async fn seed_inventory(pool: &PgPool, node: Uuid) {
    // A node inventory that lists the models of a harness.
    let snapshot = json!({"revision": 1, "harnesses": [
        {"harness": "claude", "cli_version": "1", "models": [{"provider": "anthropic", "model": "m"}]},
        {"harness": "codex", "cli_version": "2"}]});
    sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)")
        .bind(node)
        .bind(Uuid::new_v4())
        .bind(&snapshot)
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_attempt_history(pool: &PgPool, owner: Uuid, node: Uuid) -> HistoricalAttempts {
    let digest = "a".repeat(64);
    // Attempt history: rows the foreign keys and seals of earlier formats
    // guard are written as a restore would write them.
    let old_attempt = Uuid::new_v4();
    let partial_attempt = Uuid::new_v4();
    let plain_attempt = Uuid::new_v4();
    let supervisor = Uuid::new_v4();
    let renamed_supervisor = Uuid::new_v4();
    let stopped_supervisor = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role='replica'")
        .execute(&mut *tx)
        .await
        .unwrap();
    for attempt in [old_attempt, partial_attempt, plain_attempt] {
        sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,gen_random_uuid(),$2,gen_random_uuid(),gen_random_uuid(),FALSE,'{\"label\":\"history\"}')")
            .bind(attempt)
            .bind(node)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let decision = Uuid::new_v4();
    let selection = json!({"catalog_digest": digest, "catalog_revision": 1,
        "choice": {"provider": "anthropic", "model": "claude-old", "composition_digest": digest,
            "reasoning_configuration_reference": "anthropic:high"}});
    sqlx::query("INSERT INTO intelligence_decisions(decision_id,actor_id,operation_id,payload_digest,decision) VALUES($1,$2,$3,$4,$5)")
        .bind(decision)
        .bind(owner)
        .bind(Uuid::new_v4())
        .bind(&digest)
        .bind(json!({"inputs": {"project": {"catalog_digest": digest, "agent_profile": "implementer@2"}},
            "outcome": {"resolved": selection}}))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO intelligence_attempt_selections(attempt_id,selection_digest,selection) VALUES($1,$2,$3)")
        .bind(old_attempt)
        .bind(&digest)
        .bind(&selection)
        .execute(&mut *tx)
        .await
        .unwrap();
    let mut without_model = manifest("agent:implementer@2", "intelligence:engineering@2", "x");
    without_model["model"]
        .as_object_mut()
        .unwrap()
        .remove("model");
    for (attempt, manifest) in [
        (
            old_attempt,
            manifest(
                "agent:aq-server-implementer@12",
                "intelligence:deep-high@3",
                "claude-old",
            ),
        ),
        (partial_attempt, without_model),
    ] {
        sqlx::query("INSERT INTO profiled_launches(attempt_id,decision_id,source_observation_id,launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms) VALUES($1,$2,gen_random_uuid(),'{}',$3,$4,'{}',$4,'{}','{}',1)")
            .bind(attempt)
            .bind(decision)
            .bind(manifest)
            .bind(&digest)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let attempts = HistoricalAttempts {
        old_attempt,
        supervisor,
        renamed_supervisor,
        stopped_supervisor,
    };
    seed_supervisors(&mut tx, owner, &digest, &attempts).await;
    tx.commit().await.unwrap();
    attempts
}

async fn seed_supervisors(
    connection: &mut sqlx::PgConnection,
    owner: Uuid,
    digest: &str,
    attempts: &HistoricalAttempts,
) {
    let HistoricalAttempts {
        supervisor,
        renamed_supervisor,
        stopped_supervisor,
        ..
    } = *attempts;
    // Supervisors with a choice from the old catalog: two switched on (one
    // on a renamed class), one switched off.
    for (id, enabled, profile_id) in [
        (supervisor, true, "standard-medium"),
        (renamed_supervisor, true, "fast-low"),
        (stopped_supervisor, false, "deep-high"),
    ] {
        sqlx::query("INSERT INTO supervisor_controls(conversation_id,enabled,revision,profile,approved_by) VALUES($1,$2,3,$3,$4)")
            .bind(id)
            .bind(enabled)
            .bind(json!({"catalog_digest": digest, "profile_id": profile_id, "profile_version": 2,
                "candidate_id": "v1-codex", "name": "Standard · Medium",
                "provider": "openai", "model": "gpt-6.1-sol", "harness": "codex",
                "reasoning": "medium", "reasoning_configuration_reference": "codex:medium",
                "options": {"service_tier": "fast"}}))
            .bind(owner)
            .execute(&mut *connection)
            .await
            .unwrap();
    }
}

async fn assert_bindings_migrated(pool: &PgPool, binding: &Value) {
    // Bindings: the removed fields are gone, the defaults are set, the rest
    // is kept, and each stored copy is a binding the server reads.
    let mut expected = binding.clone();
    for removed in [
        "catalog_digest",
        "allowed_agent_profiles",
        "allowed_intelligence_profiles",
        "allowed_execution_profiles",
    ] {
        expected.as_object_mut().unwrap().remove(removed);
    }
    expected["default_role"] = json!("developer");
    expected["default_intelligence"] = json!("standard-high");
    let copies: Vec<Value> = sqlx::query_scalar(
        "SELECT binding FROM project_execution_bindings
         UNION ALL SELECT record->'binding' FROM project_execution_binding_history
         UNION ALL SELECT response#>'{record,binding}' FROM project_execution_operations",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(copies.len(), 3);
    for copy in copies {
        assert_eq!(copy, expected);
        let read: ProjectBinding = serde_json::from_value(copy).unwrap();
        assert_eq!(read.default_role, "developer");
        assert_eq!(read.default_intelligence, "standard-high");
        assert_eq!(read.ceilings.wall_time_ms, 3_600_000);
        assert_eq!(read.allowed_environments, ["confined-worktree"]);
    }
}

async fn assert_attempt_summary(pool: &PgPool, old_attempt: Uuid) {
    // What the old attempt ran with comes from its launch manifest; its
    // snapshot digest is null and nothing is pinned. An attempt whose
    // manifest lacks a value, and one with no launch, get no summary.
    let mut conn = pool.acquire().await.unwrap();
    let summary = aq_role_settings::attempts::ran_with(&mut conn, old_attempt)
        .await
        .unwrap()
        .expect("a summary for the old attempt");
    assert_eq!(
        serde_json::to_value(&summary).unwrap(),
        json!({"role": "aq-server-implementer", "level": "deep-high", "provider": "anthropic",
            "model": "claude-old", "reasoning": "high", "harness": "claude",
            "execution_profile": "confined-development", "snapshot_digest": null})
    );
    drop(conn);
    let summaries: Vec<Uuid> = sqlx::query_scalar("SELECT attempt_id FROM attempt_ran_with")
        .fetch_all(pool)
        .await
        .unwrap();
    assert_eq!(summaries, [old_attempt]);
    let pins: i64 = sqlx::query_scalar("SELECT count(*) FROM role_snapshot_pins")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(pins, 0);
}

async fn assert_inventory_migrated(pool: &PgPool) {
    // The node inventory keeps everything but the model lists.
    let stored: Value = sqlx::query_scalar("SELECT snapshot FROM intelligence_node_snapshots")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        stored,
        json!({"revision": 1, "harnesses": [
            {"harness": "claude", "cli_version": "1"}, {"harness": "codex", "cli_version": "2"}]})
    );
}

async fn assert_supervisors_migrated(pool: &PgPool, attempts: &HistoricalAttempts) {
    let HistoricalAttempts {
        supervisor,
        renamed_supervisor,
        stopped_supervisor,
        ..
    } = *attempts;
    // Supervisors are carried: `enabled` and the revision as they were, the
    // choice as a level id and its candidate, with no snapshot digest.
    for (id, enabled, level) in [
        (supervisor, true, "standard-medium"),
        (renamed_supervisor, true, "simple-low"),
        (stopped_supervisor, false, "deep-high"),
    ] {
        let control: (bool, Option<Value>, i64) = sqlx::query_as(
            "SELECT enabled,profile,revision FROM supervisor_controls WHERE conversation_id=$1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
        let carried = json!({"profile_id": level, "candidate_id": "openai:gpt-6.1-sol:medium",
            "name": "Standard · Medium", "provider": "openai", "model": "gpt-6.1-sol",
            "reasoning": "medium", "harness": "codex"});
        assert_eq!(control, (enabled, Some(carried), 3));
    }
}

#[tokio::test]
async fn the_roles_migration_removes_the_catalog_and_keeps_bindings_and_attempt_history() {
    let number: u32 = MIGRATION
        .rsplit_once("SET value='")
        .and_then(|(_, tail)| tail.split('\'').next())
        .and_then(|n| n.parse().ok())
        .unwrap();
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, number - 1).await.unwrap();
    let (owner, node, project) = seed_principals(&pool).await;
    seed_catalog(&pool, owner, node).await;
    let binding = seed_bindings(&pool, owner, project).await;
    seed_inventory(&pool, node).await;
    let attempts = seed_attempt_history(&pool, owner, node).await;
    let mut before = Vec::new();
    for table in HISTORY {
        before.push(rows(&pool, table).await);
    }
    for table in REMOVED {
        assert!(exists(&pool, table).await, "{table} before");
    }

    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    for table in REMOVED {
        assert!(!exists(&pool, table).await, "{table} is dropped");
    }
    // Attempt history is kept row for row.
    for (table, before) in HISTORY.iter().zip(before) {
        assert_eq!(rows(&pool, table).await, before, "{table}");
    }
    // The routing document is the role store's first saved routing revision.
    let (markdown, revision) = aq_role_settings::routing_document(&pool).await.unwrap();
    assert_eq!(markdown, "# Routing\nReviews go to deep-high.\n");
    assert_eq!(revision, 1);
    assert_eq!(aq_role_settings::settings(&pool).await.unwrap().revision, 1);
    assert_bindings_migrated(&pool, &binding).await;
    assert_attempt_summary(&pool, attempts.old_attempt).await;
    assert_inventory_migrated(&pool).await;
    assert_supervisors_migrated(&pool, &attempts).await;
    // The swarm's occupancy count reads the old attempts without an error.
    let mut conn = pool.acquire().await.unwrap();
    aq_swarm_settings::admission::counts(&mut conn)
        .await
        .unwrap();
    let format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(format, number.to_string());
}

/// A database that never had a routing document or bindings upgrades to an
/// empty routing document at revision 0.
#[tokio::test]
async fn the_roles_migration_of_an_empty_store_carries_nothing() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 133).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        aq_role_settings::routing_document(&pool).await.unwrap(),
        (String::new(), 0)
    );
    assert_eq!(aq_role_settings::settings(&pool).await.unwrap().revision, 0);
    let summaries: i64 = sqlx::query_scalar("SELECT count(*) FROM attempt_ran_with")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(summaries, 0);
}
