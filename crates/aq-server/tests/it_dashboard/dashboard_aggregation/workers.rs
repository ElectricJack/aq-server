use super::*;

#[tokio::test]
async fn overview_workers_use_configured_threshold_and_project_association() {
    use serde_json::json;
    use std::sync::Arc;
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(f.node.node_id)
    .bind((common::profiled::NOW - 61_000) as i64)
    .execute(&pool)
    .await
    .unwrap();
    let mut config = (*f.state.config).clone();
    config.workers.offline_after_secs = 120;
    f.state.config = Arc::new(config.clone());
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, body) = read(&app, "/api/v1/overview").await;
    assert!(
        !body["data"]["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "worker_offline")
    );
    config.workers.offline_after_secs = 60;
    f.state.config = Arc::new(config);
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, body) = read(&app, "/api/v1/overview").await;
    let worker = body["data"]["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["kind"] == "worker_offline")
        .expect("scoped stale worker");
    let revision: String = sqlx::query_scalar("SELECT xmin::text FROM nodes WHERE id=$1")
        .bind(f.node.node_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(worker["subject"]["id"], json!(f.node.node_id));
    // A node the owner removed in worker settings is not listed, though stale.
    let removed = |removed: bool| {
        sqlx::query("WITH absent AS (INSERT INTO worker_settings(kind,id,revision) SELECT 'node',$1,1 WHERE NOT EXISTS(SELECT 1 FROM worker_settings WHERE kind='node' AND id=$1)) UPDATE worker_settings SET removed=$2 WHERE kind='node' AND id=$1")
            .bind(f.node.node_id.to_string())
            .bind(removed)
            .execute(&pool)
    };
    removed(true).await.unwrap();
    removed(true).await.unwrap();
    let (_, hidden) = read(&app, "/api/v1/overview").await;
    let attention = hidden["data"]["attention"].as_array().unwrap();
    assert!(!attention.iter().any(|v| v["kind"] == "worker_offline"));
    removed(false).await.unwrap();
    assert_eq!(worker["subject"]["revision"], revision);
    assert_eq!(
        worker["project"]["id"],
        json!(f.selection.lineage.project_id)
    );
    assert_eq!(worker["since"], "1970-01-01T00:16:39.000Z");
    assert!(worker["action"].is_null());
    let (_, excluded) = read(
        &app,
        &format!("/api/v1/overview?scope=project:{}", uuid::Uuid::new_v4()),
    )
    .await;
    assert!(!excluded.to_string().contains(&f.node.node_id.to_string()));
    assert_unassociated_worker(&f, &pool, &app).await;
}

#[tokio::test]
async fn overview_uncertain_operations_are_principal_and_scope_bound() {
    use serde_json::json;
    use uuid::Uuid;
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now())").bind(other).execute(&pool).await.unwrap();
    let own = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    let scope = json!({"kind":"project","project_id":f.selection.lineage.project_id});
    let digest = format!("sha256:{}", "a".repeat(64));
    // Synthetic retained journal fixtures, read through the journal owner. These
    // do not claim a dispatched domain action or manufacture domain receipts.
    for (id, principal) in [(own, f.actor), (foreign, other)] {
        let response = json!({"operation_id":id,"disposition":"unknown","request_digest":digest,"scope":scope,"created_at":"1970-01-01T00:16:40.000Z","updated_at":"1970-01-01T00:16:40.000Z","subject_refs":[],"receipt_refs":[],"affected_entities":[],"recovery":"lookup_original"});
        sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at) VALUES($1,$2,'task.create',$3,$4,'unknown',$5,to_timestamp(1000),to_timestamp(1000))").bind(id).bind(principal).bind(&scope).bind(&digest).bind(response).execute(&pool).await.unwrap();
    }
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, body) = read(&app, "/api/v1/overview").await;
    let operation = body["data"]["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["kind"] == "uncertain_operation")
        .unwrap();
    assert_eq!(operation["subject"]["id"], json!(own));
    assert_eq!(operation["subject"]["revision"], digest);
    assert_eq!(body["pending_operations"], json!([own]));
    assert!(!body.to_string().contains(&foreign.to_string()));
    let (_, excluded) = read(
        &app,
        &format!("/api/v1/overview?scope=project:{}", Uuid::new_v4()),
    )
    .await;
    assert!(!excluded.to_string().contains(&own.to_string()));
}

async fn assert_unassociated_worker(
    f: &common::profiled::Fixture,
    pool: &sqlx::PgPool,
    app: &Router,
) {
    use serde_json::json;
    // QA SO-06: an offline worker with no recorded work is installation-wide
    // attention in the all-projects view only.
    let idle = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,last_seen_at) VALUES($1,'idle',$2,'1',1,to_timestamp(0),to_timestamp(0))")
        .bind(idle)
        .bind(idle.to_string())
        .execute(pool)
        .await
        .unwrap();
    let (_, body) = read(app, "/api/v1/overview").await;
    let attention = body["data"]["attention"].as_array().unwrap();
    let item = attention
        .iter()
        .find(|v| v["subject"]["id"] == json!(idle))
        .expect("unassociated offline worker");
    assert_eq!(item["kind"], "worker_offline");
    assert!(item["project"].is_null());
    assert_eq!(
        attention
            .iter()
            .filter(|v| v["subject"]["id"] == json!(f.node.node_id))
            .count(),
        1,
        "an associated worker is listed once, with its project"
    );
    // No review owner is mounted here, so the count stays unknown, never zero.
    assert!(body["data"]["counts"]["needs_you"]["value"].is_null());
    let (_, scoped) = read(
        app,
        &format!(
            "/api/v1/overview?scope=project:{}",
            f.selection.lineage.project_id
        ),
    )
    .await;
    assert!(!scoped.to_string().contains(&idle.to_string()));
}

/// A worker whose machine defers sandbox admission is ineligible with no idle slots at once,
/// and attention once the deferral is older than the node-limits Setting allows.
#[tokio::test]
async fn a_deferring_worker_is_ineligible_then_attention_after_the_setting() {
    use serde_json::json;
    let now = common::profiled::NOW;
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let node = f.node.node_id;
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(node)
    .bind(now as i64)
    .execute(&pool)
    .await
    .unwrap();
    let deferral = json!({"code": "sandbox_disk_headroom_unavailable", "since_ms": now - 600_000,
        "available_bytes": 24_905_654_272_u64, "required_bytes": 34_359_738_368_u64});
    let report = json!({"schema_version": 1, "machine": {"kind": "local"}, "observed_at_ms": now,
        "source": "linux", "host_probe": "not_applicable", "logical_cpus": 8,
        "memory_total_bytes": 100, "memory_used_bytes": 10, "cpu_busy_bps": 0,
        "sandbox_admission": deferral});
    sqlx::query("INSERT INTO node_machine_reports(node_id,report,received_at_ms) VALUES($1,$2,$3)")
        .bind(node)
        .bind(report)
        .bind(now as i64)
        .execute(&pool)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, capacity) = read(&app, "/api/v1/capacity").await;
    assert_eq!(status, StatusCode::OK, "{capacity}");
    let entry = capacity["data"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["node"]["id"] == json!(node))
        .cloned()
        .unwrap();
    assert_eq!(entry["eligibility"]["state"], "ineligible");
    assert!(
        entry["eligibility"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("sandbox_admission_deferred"))
    );
    assert_eq!(entry["slots"]["idle"], 0, "{entry}");
    assert_eq!(
        entry["sandbox_admission"]["code"],
        "sandbox_disk_headroom_unavailable"
    );
    assert_eq!(
        entry["sandbox_admission"]["required_bytes"],
        34_359_738_368_u64
    );
    let attention = |body: &serde_json::Value| {
        let items = body["data"]["attention"].as_array().unwrap();
        items
            .iter()
            .find(|v| v["kind"] == "sandbox_admission_deferred")
            .cloned()
    };
    // Ten minutes is under the default thirty.
    let (_, overview) = read(&app, "/api/v1/overview").await;
    assert_eq!(attention(&overview), None, "{overview}");
    sqlx::query("UPDATE role_settings SET node_limits='{\"sandbox_headroom_bytes\":1,\"build_cache_keys_kept\":2,\"admission_deferral_attention_ms\":300000}'")
        .execute(&pool)
        .await
        .unwrap();
    let (_, overview) = read(&app, "/api/v1/overview").await;
    let item = attention(&overview).expect("a worker deferred past the Setting is attention");
    assert_eq!(item["subject"]["id"], json!(node));
    assert!(
        item["reason"]
            .as_str()
            .unwrap()
            .contains("34359738368 required"),
        "{item}"
    );
    sqlx::query(
        "UPDATE node_machine_reports SET report=report-'sandbox_admission' WHERE node_id=$1",
    )
    .bind(node)
    .execute(&pool)
    .await
    .unwrap();
    let (_, overview) = read(&app, "/api/v1/overview").await;
    assert_eq!(attention(&overview), None);
    let (_, capacity) = read(&app, "/api/v1/capacity").await;
    assert!(!capacity.to_string().contains("sandbox_admission_deferred"));
}
