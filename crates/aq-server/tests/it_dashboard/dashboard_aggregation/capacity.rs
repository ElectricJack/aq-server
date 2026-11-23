use super::*;
use aq_selection::{ArtifactRef, AuthorizedCredential, Text};
use aq_server::intelligence::config::{CredentialConfig, IntelligenceConfig};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

/// X1/X2/X3: slot load, per-node eligibility with placement's own refusal
/// reasons, provider availability from unexpired node credential reports,
/// and storage on the worker inventory.
#[tokio::test]
async fn capacity_reports_slots_eligibility_availability_and_storage() {
    let now = common::profiled::NOW;
    let mut f = common::profiled::fixture().await;
    let reported = configure_accounts(&mut f);
    let pool = f.db.pool().await;
    let boat = seed_nodes(&pool, f.node.node_id, reported, now).await;
    disk(&pool, boat, now, 50).await;
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
    assert_capacity(&body, boat, f.node.node_id);

    // Disk pressure is a placement refusal; revoked nodes leave the online total.
    disk(&pool, boat, now, 90).await;
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    let (_, body) = read(&app, "/api/v1/capacity").await;
    assert_revoked_capacity(&body, boat, f.node.node_id);
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

    assert_worker_storage(&app, boat, f.node.node_id).await;
}

fn configure_accounts(f: &mut common::profiled::Fixture) -> AuthorizedCredential {
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
                    allow_enrolled_nodes: false,
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
    reported
}

async fn seed_nodes(
    pool: &sqlx::PgPool,
    native_id: Uuid,
    reported: AuthorizedCredential,
    now: u64,
) -> Uuid {
    // Native and sandbox nodes both use their snapshotted server slot Setting.
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(native_id)
    .bind(now as i64)
    .execute(pool)
    .await
    .unwrap();
    let boat = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,last_seen_at) VALUES($1,'boat-1',$2,'t',1,now(),to_timestamp($3::double precision/1000))")
        .bind(boat).bind(boat.to_string()).bind(now as i64).execute(pool).await.unwrap();
    let registration = json!({"backend":{"kind":"boat","capabilities":["vm-isolation"],"max_concurrent_sandboxes":2,"spend_ceiling":null,"templates":[]}});
    sqlx::query("INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)")
        .bind(boat)
        .bind(uuid::Uuid::new_v4())
        .bind(registration)
        .execute(pool)
        .await
        .unwrap();
    let snapshot = json!({"observed_at_ms": now - 5, "expires_at_ms": now + 30_000, "credential_bindings": [
        {"credential_revision": 1, "credential_profile": reported.profile, "available": true, "observed_at_ms": now - 5, "expires_at_ms": now + 30_000}
    ]});
    sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)")
        .bind(boat).bind(uuid::Uuid::new_v4()).bind(snapshot).execute(pool).await.unwrap();
    boat
}

async fn disk(pool: &sqlx::PgPool, boat: Uuid, now: u64, used: u64) {
    let report = aq_server::node_storage::StorageReport {
        disk_total_bytes: 100,
        disk_used_bytes: used,
        disk_free_bytes: 100 - used,
        preserved_count: 3,
        preserved_bytes: 7,
        ..Default::default()
    };
    let mut conn = pool.acquire().await.unwrap();
    aq_server::node_storage::record(&mut conn, &boat.to_string(), Some(boat), &report, now - 1)
        .await
        .unwrap();
}

fn node(id: Uuid, body: &Value) -> Value {
    body["data"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["node"]["id"] == json!(id))
        .cloned()
        .unwrap()
}

fn assert_capacity(body: &Value, boat: Uuid, native_id: Uuid) {
    let data = &body["data"];
    assert_fields(
        &data["slots"],
        json!({
            "total":{"value":4,"accuracy":"exact"}, "idle":{"value":4,"accuracy":"exact"},
            "occupied":{"value":0,"accuracy":"exact"}
        }),
    );
    assert_eq!(data["slots"]["coverage"]["state"], "complete");
    assert_eq!(body["coverage"]["state"], "complete");
    assert_fields(
        &node(native_id, body),
        json!({"health":"online","slots":{"total":2,"occupied":0,"idle":2},"backend_kind":null}),
    );
    let b = node(boat, body);
    assert_fields(
        &b,
        json!({"backend_kind":"boat","slots":{"total":2,"occupied":0,"idle":2},
        "eligibility":{"state":"eligible","reasons":[]},"capability_observed_at":"1970-01-01T00:16:39.995Z",
        "capability_fresh_until":"1970-01-01T00:17:10.000Z"}),
    );
    assert_fields(
        &b["storage"],
        json!({"used_bps":5000,"preserved_count":3,"observed_at":"1970-01-01T00:16:39.999Z"}),
    );
    let providers = data["providers"].as_array().unwrap();
    let account = |name: &str| {
        providers
            .iter()
            .find(|p| p["account_ref"] == name)
            .cloned()
            .unwrap()
    };
    assert_fields(
        &account("acct-reported"),
        json!({"state":"available","reason":null,"available_nodes":1,"observed_at":"1970-01-01T00:16:39.995Z"}),
    );
    assert_fields(
        &account("acct-disabled"),
        json!({"state":"unavailable","reason":"account_disabled"}),
    );
    assert_fields(
        &account("acct-silent"),
        json!({"state":"unknown","reason":"no_fresh_node_report","observed_at":null}),
    );
    assert!(!body.to_string().contains("private-capacity-canary"));
    capacity_fixture(body);
}

async fn assert_worker_storage(app: &Router, boat: Uuid, native_id: Uuid) {
    // X3: the worker inventory carries flattened storage, absent when unreported.
    let (status, workers) = read(app, "/api/workers").await;
    assert_eq!(status, StatusCode::OK, "{workers}");
    let rows = workers.as_array().unwrap();
    let row = rows.iter().find(|w| w["id"] == json!(boat)).unwrap();
    assert_fields(
        row,
        json!({"disk_total_bytes":100,"disk_used_bytes":90,"disk_free_bytes":10,"preserved_bytes":7,"storage_used_bps":9000}),
    );
    assert!(row["storage_observed_at"].is_string());
    let native = rows.iter().find(|w| w["id"] == json!(native_id)).unwrap();
    assert!(native.get("disk_total_bytes").is_none());
    assert!(native.get("storage_observed_at").is_none());
}

fn assert_revoked_capacity(body: &Value, boat: Uuid, native_id: Uuid) {
    let b = node(boat, body);
    assert_eq!(
        b["eligibility"],
        json!({"state": "ineligible", "reasons": ["node_disk_pressure"]})
    );
    assert_eq!(node(native_id, body)["health"], "revoked");
    assert!(
        node(native_id, body)["eligibility"]["reasons"]
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
}

fn assert_fields(actual: &Value, expected: Value) {
    for (field, value) in expected.as_object().unwrap() {
        assert_eq!(&actual[field], value, "{field} in {actual}");
    }
}
