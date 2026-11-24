//! Synthetic owner registry qualification. No model/provider calls or real keys.
use aq_selection::{ArtifactRef, AuthorizedCredential, Digest, Text};
use aq_server::{
    AppState,
    config::Config,
    identity::verifier_from_config,
    intelligence::config::{CredentialConfig, IntelligenceConfig},
    test_support::TestDb,
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
fn credential() -> AuthorizedCredential {
    AuthorizedCredential {
        profile: ArtifactRef {
            id: Text::new("fixture-account").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"fixture-credential-definition"),
        },
        provider: "synthetic-provider".into(),
        account_reference: "account-a".into(),
        endpoint_reference: "endpoint-a".into(),
        revision: 1,
        enabled: true,
        provisionable: false,
        material_owner: None,
        allowed_backends: aq_selection::default_allowed_backends(),
    }
}
fn registry(path: Option<std::path::PathBuf>) -> IntelligenceConfig {
    IntelligenceConfig {
        accounting: Default::default(),
        material_store: None,
        revision: 1,
        credentials: vec![CredentialConfig {
            kind: Default::default(),
            allowed_backends: aq_selection::default_allowed_backends(),
            credential: credential(),
            allowed_node_ids: vec![Uuid::from_u128(10)],
            allow_enrolled_nodes: false,
            secret_file: path,
            delivery_mode: None,
        }],
        qualified_harnesses: vec![],
    }
}
fn config() -> Config {
    toml::from_str(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#,
    )
    .unwrap()
}
async fn setup(reg: IntelligenceConfig) -> (TestDb, AppState, Router) {
    let db = TestDb::create().await;
    let store = db.store().await;
    let time = Utc.timestamp_millis_opt(1_000_000).unwrap();
    store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", time)
        .await
        .unwrap();
    store
        .invite_user("viewer@localhost", aq_server::store::Role::Viewer, time)
        .await
        .unwrap();
    let mut cfg = config();
    cfg.intelligence = Some(Arc::new(reg.load().unwrap()));
    let state = AppState {
        internal_work: Default::default(),
        identity: verifier_from_config(&cfg.auth, reqwest::Client::new()),
        config: Arc::new(cfg),
        store,
        clock: Arc::new(|| Utc.timestamp_millis_opt(1_000_000).unwrap()),
        agent_access: None,
        source_retention: None,
    };
    (db, state.clone(), aq_server::build_admin_router(state))
}
mod enrollment;
mod observations;
mod requests;
use enrollment::{Node, enrolled_inventory, inventory_of};
use requests::{Session, call, configured, machine, register};

#[tokio::test]
async fn absent_registry_inspection_is_a_conflict() {
    let (_db, _state, app) = setup(registry(None)).await;
    let (status, body) = call(
        &app,
        "/api/intelligence/registry",
        None,
        Session::OwnerWithoutCsrf,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}
#[test]
fn credential_loading_is_private_bounded_regular_and_never_claims_missing_keys() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("key");
    assert!(!registry(Some(file.clone())).load().unwrap().credentials[0].provisionable);
    std::fs::write(&file, b"synthetic-only-key").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let loaded = registry(Some(file.clone())).load().unwrap();
    assert!(loaded.credentials[0].provisionable);
    assert!(!format!("{loaded:?}").contains("synthetic-only-key"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(registry(Some(file.clone())).load().is_err());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&file, vec![b'x'; 65_537]).unwrap();
    assert!(registry(Some(file.clone())).load().is_err());
    std::fs::remove_file(&file).unwrap();
    nix::unistd::mkfifo(
        &file,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    assert!(registry(Some(file.clone())).load().is_err());
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink("missing", &file).unwrap();
    assert!(registry(Some(file)).load().is_err());
}
#[tokio::test]
async fn manual_observation_has_authenticated_provenance_exact_replay_and_expiry_tombstones() {
    let (_db, state, app) = setup(registry(None)).await;

    assert_eq!(register(&app, Session::Viewer).await, StatusCode::FORBIDDEN);
    assert_eq!(
        register(&app, Session::OwnerWithoutCsrf).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(register(&app, Session::Owner).await, StatusCode::OK);
    let body = observations::observation();
    let first = observations::verify_receipt(&app, &body).await;
    observations::verify_refusals(&app, &body).await;
    observations::verify_removed_scope(&state, body, first).await;
    observations::verify_rotation(&state).await;
}

#[tokio::test]
async fn installed_inventory_is_current_boot_bound_does_not_authorize_keys_and_legacy_feedback_refuses()
 {
    let (db, state, _) = setup(registry(None)).await;
    let node = Node::enroll(&db, &state, "intelligence-fixture", "fixture").await;
    node.identify(&state).await;
    let mut reg = registry(None);
    reg.credentials[0].allowed_node_ids = vec![node.identity.node_id];
    let state = configured(&state, &reg);
    let admin = aq_server::build_admin_router(state.clone());
    assert_eq!(register(&admin, Session::Owner).await, StatusCode::OK);
    let worker = aq_server::build_worker_router(state);
    let body = node.inventory_body();
    let first = node.verify_inventory(&worker, &body).await;
    // A node's credential presence does not grant owner provisioning authority.
    let (_, view) = call(&admin, "/api/intelligence/registry", None, Session::Owner).await;
    assert_eq!(view["credentials"][0]["provisionable"], false);
    node.verify_replay(&worker, &body, first).await;
    node.verify_legacy_feedback(&worker).await;
    assert_eq!(
        machine(
            &worker,
            "/v1/worker/intelligence/inventory",
            "aqn_foreign",
            body
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}
#[test]
fn server_material_owner_is_derived_only_for_explicit_safe_configured_delivery() {
    use aq_server::intelligence::config::MaterialDeliveryMode;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("private-provider-material");
    let mut configured = registry(Some(file.clone()));
    configured.credentials[0].delivery_mode = Some(MaterialDeliveryMode::ServerMaterialV1);
    let missing = configured.load().unwrap();
    assert!(!missing.credentials[0].provisionable);
    assert!(missing.credentials[0].material_owner.is_none());
    std::fs::write(&file, b"synthetic-only-key").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let installed = registry(Some(file.clone())).load().unwrap();
    assert!(installed.credentials[0].material_owner.is_none());
    let delivery = configured.load().unwrap();
    let owner = delivery.credentials[0].material_owner.clone().unwrap();
    let public = serde_json::to_string(&delivery.credentials).unwrap();
    assert!(!public.contains("synthetic-only-key"));
    assert!(!public.contains(file.to_str().unwrap()));
    assert!(!format!("{configured:?}").contains("synthetic-only-key"));
    configured.credentials[0].credential.revision += 1;
    assert_ne!(
        configured.load().unwrap().credentials[0]
            .material_owner
            .as_ref()
            .unwrap()
            .digest,
        owner.digest
    );
    configured.credentials[0].credential.material_owner = Some(owner);
    assert!(configured.load().is_err());
    configured.credentials[0].credential.material_owner = None;
    configured.credentials[0].secret_file = None;
    assert!(configured.load().is_err());
}

#[test]
fn credential_backend_configuration_is_closed_bounded_and_defaults_unrestricted() {
    use aq_selection::{BackendKind, CredentialKind, default_allowed_backends};
    let value = json!({
        "revision": 1,
        "credentials": [{"credential": credential(), "allowed_node_ids": []}]
    });
    let config: IntelligenceConfig = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(config.credentials[0].kind, CredentialKind::ApiKey);
    assert_eq!(
        config.credentials[0].allowed_backends,
        default_allowed_backends()
    );
    assert!(config.validate().is_ok());
    for kind in ["api-key", "subscription-login"] {
        let mut v = value.clone();
        v["credentials"][0]["kind"] = json!(kind);
        v["credentials"][0]["allowed_backends"] = json!(["boat"]);
        let config: IntelligenceConfig = serde_json::from_value(v).unwrap();
        let loaded = config.load().unwrap();
        assert_eq!(
            loaded.credentials[0].allowed_backends,
            vec![BackendKind::Boat]
        );
    }
    for (field, invalid) in [
        ("kind", json!("unknown")),
        ("kind", json!("x".repeat(513))),
        ("allowed_backends", json!(["unknown"])),
        ("allowed_backends", json!(["x".repeat(513)])),
    ] {
        let mut v = value.clone();
        v["credentials"][0][field] = invalid;
        assert!(serde_json::from_value::<IntelligenceConfig>(v).is_err());
    }
    for backends in [vec![BackendKind::Boat; 2], vec![BackendKind::Boat; 4]] {
        let mut config = config.clone();
        config.credentials[0].allowed_backends = backends;
        assert!(config.validate().is_err());
    }
    let mut forged = config.clone();
    forged.credentials[0].credential.allowed_backends = vec![BackendKind::Boat];
    assert!(forged.validate().is_err());
    let mut config = config;
    config.credentials[0].allowed_backends.clear();
    assert!(config.validate().is_ok());
    assert!(
        config.load().unwrap().credentials[0]
            .allowed_backends
            .is_empty()
    );
}

#[tokio::test]
async fn credential_policy_changes_require_a_new_registry_revision() {
    use aq_selection::{BackendKind, CredentialKind};
    let (_db, state, app) = setup(registry(None)).await;

    assert_eq!(register(&app, Session::Owner).await, StatusCode::OK);
    let mut updated = registry(None);
    updated.credentials[0].kind = CredentialKind::SubscriptionLogin;
    verify_revision_change(&state, &mut updated).await;
    updated.credentials[0].allowed_backends = vec![BackendKind::Boat];
    verify_revision_change(&state, &mut updated).await;
}

async fn verify_revision_change(state: &AppState, updated: &mut IntelligenceConfig) {
    let app = aq_server::build_admin_router(configured(state, updated));
    assert_eq!(register(&app, Session::Owner).await, StatusCode::CONFLICT);
    updated.revision += 1;
    let app = aq_server::build_admin_router(configured(state, updated));
    assert_eq!(register(&app, Session::Owner).await, StatusCode::OK);
}

#[tokio::test]
async fn enrolled_node_inventory_accepted_with_flag() {
    let (status, answer, _) = enrolled_inventory(true, false).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["credential_bindings"][0]["available"], true);
    let (status, _, _) = enrolled_inventory(false, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Review I6: allow_enrolled_nodes admits only nodes enrolled with a code.
#[tokio::test]
async fn unenrolled_node_is_refused_even_with_flag() {
    let (status, v, _) = inventory_of(true, false, false).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
}

#[tokio::test]
async fn revoked_node_is_refused_even_with_flag() {
    let (status, _, revoked) = enrolled_inventory(true, true).await;
    assert_eq!(revoked, StatusCode::NO_CONTENT);
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
