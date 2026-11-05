//! Dashboard aggregate HTTP behavior with disposable owner fixtures.
mod common;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

async fn read(app: &Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn quota_unknown_is_not_zero() {
    use aq_selection::{ArtifactRef, AuthorizedCredential, Text};
    use aq_server::intelligence::config::{CredentialConfig, IntelligenceConfig};
    use std::sync::Arc;
    let mut f = common::profiled::fixture().await;
    let mut config = (*f.state.config).clone();
    let credential = AuthorizedCredential {
        allowed_backends: aq_selection::default_allowed_backends(),
        profile: ArtifactRef {
            id: Text::new("profile").unwrap(),
            version: Text::new("1").unwrap(),
            digest: aq_admission::Digest::of(b"public"),
        },
        provider: "unsupported".into(),
        account_reference: "account".into(),
        endpoint_reference: "private-search-canary".into(),
        revision: 1,
        enabled: true,
        provisionable: false,
        material_owner: None,
    };
    let mut second = credential.clone();
    second.profile.id = Text::new("other-profile").unwrap();
    let credentials = [credential, second]
        .into_iter()
        .map(|credential| CredentialConfig {
            kind: aq_selection::CredentialKind::ApiKey,
            allowed_backends: aq_selection::default_allowed_backends(),
            credential,
            allowed_node_ids: vec![],
            secret_file: None,
            delivery_mode: None,
        })
        .collect();
    config.intelligence = Some(Arc::new(
        IntelligenceConfig {
            revision: 1,
            credentials,
            ..Default::default()
        }
        .load()
        .unwrap(),
    ));
    f.state.config = Arc::new(config);
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'quota-viewer@localhost','viewer','dev','quota-viewer@localhost',now())")
        .bind(uuid::Uuid::new_v4()).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let viewer_response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/provider-usage")
                .header("x-aq-dev-email", "quota-viewer@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer_response.status(), StatusCode::FORBIDDEN);
    let (status, body) = read(&app, "/api/v1/provider-usage").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let accounts = body["data"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    let usage = &accounts[0];
    assert_eq!(usage["scope"], "provider_account");
    let window = &usage["windows"][0];
    assert_eq!(window["state"], "unknown");
    for field in ["used_percent", "used", "limit"] {
        assert!(window[field].is_null(), "{field}: {window}");
    }
    assert!(!body.to_string().contains("private-search-canary"));
    let (_, other) = read(
        &app,
        &format!(
            "/api/v1/provider-usage?scope=project:{}",
            uuid::Uuid::new_v4()
        ),
    )
    .await;
    assert_eq!(body["data"], other["data"]);
    let (_, bootstrap) = read(&app, "/api/v1/bootstrap").await;
    assert!(
        bootstrap["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "provider.quota" && v["enabled"] == true)
    );
}

#[tokio::test]
async fn overview_empty_and_bounded_project_sources_are_truthful() {
    use serde_json::json;
    use uuid::Uuid;
    let f = common::profiled::fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, empty) = read(&app, "/api/v1/overview").await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["data"]["counts"]["running"]["value"], 0);
    assert_eq!(empty["data"]["counts"]["running"]["accuracy"], "exact");
    assert!(empty["data"]["counts"]["ready"]["value"].is_null());
    assert!(empty["data"]["counts"]["needs_you"]["value"].is_null());
    assert!(empty["data"]["attention"].as_array().unwrap().is_empty());
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    for _ in 0..9 {
        let project = Uuid::new_v4();
        sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) SELECT $1,revision,jsonb_set(binding,'{project_id}',$2),validated_at_ms,binding_observation FROM project_execution_bindings WHERE project_id=$3").bind(project).bind(json!(project)).bind(f.selection.lineage.project_id).execute(&pool).await.unwrap();
    }
    let (_, bounded) = read(&app, "/api/v1/overview").await;
    assert_eq!(bounded["data"]["in_motion"].as_array().unwrap().len(), 8);
    assert_eq!(
        bounded["data"]["counts"]["running"]["accuracy"],
        "estimated"
    );
    assert!(
        bounded["data"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["source"] == "projects"
                && s["coverage"]["reason"] == "project_sample_truncated")
    );
}

#[tokio::test]
async fn overview_ready_owner_preserves_samples_failures_and_binding_fence() {
    use serde_json::json;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let mode = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let workspace = f.profiled.graph.workspace_id;
    let mut node: Value = serde_json::from_str::<Value>(include_str!(
        "../../../protocol/human-graph/v1/examples/task.json"
    ))
    .unwrap()["data"]
        .clone();
    node["workspace_id"] = json!(workspace);
    node["metadata"] = json!({});
    node["version"] = json!(1);
    node["content_revision"] = json!(1);
    let server_mode = mode.clone();
    let reached = entered.clone();
    let proceed = release.clone();
    let graph = Router::new().route(
        "/v1/workspaces/{workspace}/ready",
        axum::routing::get(move || {
            let node = node.clone();
            let mode = server_mode.clone();
            let reached = reached.clone();
            let proceed = proceed.clone();
            async move {
                let mode = mode.load(Ordering::SeqCst);
                if mode == 3 {
                    reached.notify_one();
                    proceed.notified().await;
                }
                (
                    if mode == 2 {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    },
                    [("X-AQ-Graph-API-Version", "1")],
                    axum::Json(
                        json!({"items":[node],"next_cursor":if mode==1 {Some("next")} else {None}}),
                    ),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, graph).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "s".repeat(32)).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: origin,
            graph_token_file: token,
            mutation_actor_id: None,
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let app = aq_server::build_admin_router(f.state.clone());
    for (value, accuracy) in [(0, "exact"), (1, "estimated")] {
        mode.store(value, Ordering::SeqCst);
        let (status, body) = read(&app, "/api/v1/overview").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["counts"]["ready"]["value"], 1, "{body}");
        assert_eq!(body["data"]["counts"]["ready"]["accuracy"], accuracy);
        // The active Graph task is ready; it does not become a running attempt.
        assert_eq!(body["data"]["counts"]["running"]["value"], 0);
        assert!(
            !body["data"]["attention"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["kind"] == "blocked")
        );
        assert!(
            body["data"]["sources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["source"] == "graph_ready"
                    && v["observation"]["observed_at"] == "1970-01-01T00:16:40.000Z")
        );
    }
    mode.store(2, Ordering::SeqCst);
    let (_, body) = read(&app, "/api/v1/overview").await;
    assert!(body["data"]["counts"]["ready"]["value"].is_null());
    assert_eq!(body["data"]["counts"]["ready"]["accuracy"], "unknown");
    mode.store(3, Ordering::SeqCst);
    let pending_app = app.clone();
    let pending = tokio::spawn(async move { read(&pending_app, "/api/v1/overview").await });
    entered.notified().await;
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1")
        .bind(f.selection.lineage.project_id)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    release.notify_one();
    assert_eq!(pending.await.unwrap().0, StatusCode::FORBIDDEN);
    server.abort();
}

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

fn capacity_fixture(value: &Value) {
    if std::env::var_os("AQ_DASHBOARD_WRITE_FIXTURES").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../protocol/dashboard-read/v1/fixtures/capacity.json");
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }
}

/// X1/X2/X3: slot load, per-node eligibility with placement's own refusal
/// reasons, provider availability from unexpired node credential reports,
/// and storage on the worker inventory.
#[tokio::test]
async fn capacity_reports_slots_eligibility_availability_and_storage() {
    use aq_selection::{ArtifactRef, AuthorizedCredential, Text};
    use aq_server::intelligence::config::{CredentialConfig, IntelligenceConfig};
    use serde_json::json;
    use std::sync::Arc;
    let now = common::profiled::NOW;
    let mut f = common::profiled::fixture().await;
    let credential = |profile: &str, account: &str, enabled: bool| AuthorizedCredential {
        allowed_backends: aq_selection::default_allowed_backends(),
        profile: ArtifactRef {
            id: Text::new(profile).unwrap(),
            version: Text::new("1").unwrap(),
            digest: aq_admission::Digest::of(profile.as_bytes()),
        },
        provider: "openai".into(),
        account_reference: account.into(),
        endpoint_reference: "private-capacity-canary".into(),
        revision: 1,
        enabled,
        provisionable: false,
        material_owner: None,
    };
    let reported = credential("reported", "acct-reported", true);
    let credentials = [
        reported.clone(),
        credential("disabled", "acct-disabled", false),
        credential("silent", "acct-silent", true),
    ];
    let mut config = (*f.state.config).clone();
    config.intelligence = Some(Arc::new(
        IntelligenceConfig {
            revision: 1,
            credentials: credentials
                .into_iter()
                .map(|credential| CredentialConfig {
                    kind: aq_selection::CredentialKind::ApiKey,
                    allowed_backends: aq_selection::default_allowed_backends(),
                    credential,
                    allowed_node_ids: vec![],
                    secret_file: None,
                    delivery_mode: None,
                })
                .collect(),
            ..Default::default()
        }
        .load()
        .unwrap(),
    ));
    f.state.config = Arc::new(config);
    let pool = f.db.pool().await;
    // The fixture node is host-native: online, but advertises no slot total.
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(f.node.node_id)
    .bind(now as i64)
    .execute(&pool)
    .await
    .unwrap();
    let boat = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,last_seen_at) VALUES($1,'boat-1',$2,'t',1,now(),to_timestamp($3::double precision/1000))")
        .bind(boat).bind(boat.to_string()).bind(now as i64).execute(&pool).await.unwrap();
    let registration = json!({"backend":{"kind":"boat","capabilities":["vm-isolation"],"max_concurrent_sandboxes":2,"spend_ceiling":null,"templates":[]}});
    sqlx::query("INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)")
        .bind(boat)
        .bind(uuid::Uuid::new_v4())
        .bind(registration)
        .execute(&pool)
        .await
        .unwrap();
    let snapshot = json!({"observed_at_ms": now - 5, "expires_at_ms": now + 30_000, "credential_bindings": [
        {"credential_revision": 1, "credential_profile": reported.profile, "available": true, "observed_at_ms": now - 5, "expires_at_ms": now + 30_000}
    ]});
    sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)")
        .bind(boat).bind(uuid::Uuid::new_v4()).bind(snapshot).execute(&pool).await.unwrap();
    let disk = |used: u64| {
        let pool = pool.clone();
        async move {
            let report = aq_server::node_storage::StorageReport {
                disk_total_bytes: 100,
                disk_used_bytes: used,
                disk_free_bytes: 100 - used,
                preserved_count: 3,
                preserved_bytes: 7,
                ..Default::default()
            };
            let mut conn = pool.acquire().await.unwrap();
            aq_server::node_storage::record(
                &mut conn,
                &boat.to_string(),
                Some(boat),
                &report,
                now - 1,
            )
            .await
            .unwrap();
        }
    };
    disk(50).await;
    let app = aq_server::build_admin_router(f.state.clone());

    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'capacity-viewer@localhost','viewer','dev','capacity-viewer@localhost',now())")
        .bind(uuid::Uuid::new_v4()).execute(&pool).await.unwrap();
    let viewer = app
        .clone()
        .oneshot(
            Request::get("/api/v1/capacity")
                .header("x-aq-dev-email", "capacity-viewer@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer.status(), StatusCode::FORBIDDEN);

    let (status, body) = read(&app, "/api/v1/capacity").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data = &body["data"];
    // Host-native online worker: slot total and idle are unknown, not zero.
    assert_eq!(
        data["slots"]["total"],
        json!({"value": null, "accuracy": "unknown"})
    );
    assert_eq!(
        data["slots"]["idle"],
        json!({"value": null, "accuracy": "unknown"})
    );
    assert_eq!(
        data["slots"]["occupied"],
        json!({"value": 0, "accuracy": "exact"})
    );
    assert_eq!(data["slots"]["coverage"]["state"], "partial");
    assert_eq!(body["coverage"]["state"], "partial");
    let node = |id: uuid::Uuid, body: &Value| {
        body["data"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["node"]["id"] == json!(id))
            .cloned()
            .unwrap()
    };
    let native = node(f.node.node_id, &body);
    assert_eq!(native["health"], "online");
    assert!(native["slots"]["total"].is_null() && native["slots"]["idle"].is_null());
    assert!(native["backend_kind"].is_null());
    let b = node(boat, &body);
    assert_eq!(b["backend_kind"], "boat");
    assert_eq!(b["slots"], json!({"total": 2, "occupied": 0, "idle": 2}));
    assert_eq!(
        b["eligibility"],
        json!({"state": "eligible", "reasons": []})
    );
    assert_eq!(b["capability_observed_at"], "1970-01-01T00:16:39.995Z");
    assert_eq!(b["capability_fresh_until"], "1970-01-01T00:17:10.000Z");
    assert_eq!(b["storage"]["used_bps"], 5000);
    assert_eq!(b["storage"]["preserved_count"], 3);
    assert_eq!(b["storage"]["observed_at"], "1970-01-01T00:16:39.999Z");
    let providers = data["providers"].as_array().unwrap();
    let account = |name: &str| {
        providers
            .iter()
            .find(|p| p["account_ref"] == name)
            .cloned()
            .unwrap()
    };
    let available = account("acct-reported");
    assert_eq!(available["state"], "available");
    assert!(available["reason"].is_null());
    assert_eq!(available["available_nodes"], 1);
    assert_eq!(available["observed_at"], "1970-01-01T00:16:39.995Z");
    assert_eq!(account("acct-disabled")["state"], "unavailable");
    assert_eq!(account("acct-disabled")["reason"], "account_disabled");
    assert_eq!(account("acct-silent")["state"], "unknown");
    assert_eq!(account("acct-silent")["reason"], "no_fresh_node_report");
    assert!(account("acct-silent")["observed_at"].is_null());
    assert!(!body.to_string().contains("private-capacity-canary"));
    capacity_fixture(&body);

    // Placement's own refusal: above 85% used is node_disk_pressure. Revoking
    // the host-native worker makes the slot totals exact.
    disk(90).await;
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    let (_, body) = read(&app, "/api/v1/capacity").await;
    let b = node(boat, &body);
    assert_eq!(
        b["eligibility"],
        json!({"state": "ineligible", "reasons": ["node_disk_pressure"]})
    );
    assert_eq!(node(f.node.node_id, &body)["health"], "revoked");
    assert!(
        node(f.node.node_id, &body)["eligibility"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("worker_revoked"))
    );
    assert_eq!(
        body["data"]["slots"]["total"],
        json!({"value": 2, "accuracy": "exact"})
    );
    assert_eq!(
        body["data"]["slots"]["idle"],
        json!({"value": 2, "accuracy": "exact"})
    );
    assert_eq!(body["coverage"]["state"], "complete");
    // An expired capability report leaves availability unknown.
    f.time
        .store((now + 60_000) as i64, std::sync::atomic::Ordering::SeqCst);
    let (_, body) = read(&app, "/api/v1/capacity").await;
    let b = node(boat, &body);
    assert!(
        b["eligibility"]["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("capability_expired"))
    );
    assert_eq!(
        body["data"]["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["account_ref"] == "acct-reported")
            .unwrap()["state"],
        "unknown"
    );

    let (_, bootstrap) = read(&app, "/api/v1/bootstrap").await;
    assert!(
        bootstrap["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "capacity.read" && v["enabled"] == true)
    );

    // X3: the worker inventory carries flattened storage, absent when unreported.
    let (status, workers) = read(&app, "/api/workers").await;
    assert_eq!(status, StatusCode::OK, "{workers}");
    let rows = workers.as_array().unwrap();
    let row = rows.iter().find(|w| w["id"] == json!(boat)).unwrap();
    assert_eq!(row["disk_total_bytes"], 100);
    assert_eq!(row["disk_used_bytes"], 90);
    assert_eq!(row["disk_free_bytes"], 10);
    assert_eq!(row["preserved_bytes"], 7);
    assert_eq!(row["storage_used_bps"], 9000);
    assert!(row["storage_observed_at"].is_string());
    let native = rows
        .iter()
        .find(|w| w["id"] == json!(f.node.node_id))
        .unwrap();
    assert!(native.get("disk_total_bytes").is_none());
    assert!(native.get("storage_observed_at").is_none());
}
