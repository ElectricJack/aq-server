//! C5b write-back endpoint: real SQL/router fences with synthetic login bytes.
use crate::common;
use aq_admission::{Digest, Label, MaterialLease, transport::*};
use aq_selection::{AuthorizedCredential, CredentialKind, DeliveryCapability};
use aq_server::{
    execution::{
        authority::hot::{HotState, LaunchOwner},
        profiled::Status,
    },
    intelligence::config::{CredentialConfig, IntelligenceConfig, MaterialDeliveryMode},
    profiled_execution::{
        config::{ProfiledConfig, QualifiedRuntime},
        login_refresh::api::{HEADER, MaterialRefresh, PATH},
        registration::{self, NativeRegistration},
    },
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use common::profiled::{Fixture, NOW, artifact, fixture, insert};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
const SECRET: &[u8] = b"synthetic-private-provider-material";
const REFRESHED: &[u8] = b"synthetic-refreshed-login-material";
struct Case {
    f: Fixture,
    request: MaterialFetch,
    #[allow(dead_code)]
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}
async fn post(
    app: Router,
    path: &str,
    value: Value,
    credential: Option<&str>,
) -> axum::response::Response {
    let mut req = Request::post(path).header("content-type", "application/json");
    if let Some(credential) = credential {
        req = req.header("authorization", format!("Bearer {credential}"));
    } else {
        req = req
            .header("x-aq-dev-email", "owner@localhost")
            .header("x-aq-request", "1");
    }
    app.oneshot(
        req.body(Body::from(serde_json::to_vec(&value).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
}
async fn setup(kind: CredentialKind) -> Case {
    let mut f = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("provider");
    std::fs::write(&path, SECRET).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let choice = &f.selection.choice;
    let mut cfg = (*f.state.config).clone();
    cfg.intelligence = Some(Arc::new(
        IntelligenceConfig {
            accounting: Default::default(),
            material_store: Some(dir.path().join("store")),
            revision: 1,
            credentials: vec![CredentialConfig {
                kind,
                allowed_backends: aq_selection::default_allowed_backends(),
                credential: AuthorizedCredential {
                    profile: choice.credential_profile.clone(),
                    provider: choice.provider.clone(),
                    account_reference: choice.account_reference.clone(),
                    endpoint_reference: choice.endpoint_reference.clone(),
                    revision: f.selection.credential_revision,
                    enabled: true,
                    provisionable: false,
                    material_owner: None,
                    allowed_backends: aq_selection::default_allowed_backends(),
                },
                allowed_node_ids: vec![f.node.node_id],
                allow_enrolled_nodes: false,
                secret_file: Some(path.clone()),
                delivery_mode: Some(MaterialDeliveryMode::ServerMaterialV1),
            }],
            qualified_harnesses: vec![],
        }
        .load()
        .unwrap(),
    ));
    let registration = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime: artifact("synthetic-runtime", b"code"),
        capabilities: vec![],
        features: vec![Feature::ServerMaterialV1],
    };
    let qualification = artifact("synthetic-qualification", b"qualified-synthetic-runtime");
    cfg.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        context_qualifications: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: registration.runtime.clone(),
            qualification: qualification.clone(),
            capabilities: vec![],
            features: registration.features.clone(),
            material_delivery: Some(DeliveryCapability {
                adapter: artifact("synthetic-material-adapter", b"code"),
                version: 1,
            }),
        }],
        projects: vec![],
    }));
    f.state.config = Arc::new(cfg);
    aq_server::profiled_execution::policies::install(&f.state)
        .await
        .unwrap();
    let native = NativeRegistration {
        schema_version: 1,
        semantic_digest: registration::semantic_digest(&registration).unwrap(),
        request: registration,
        qualification,
        configuration_revision: 1,
        registered_at_ms: NOW,
    };
    let pool = f.db.pool().await;
    sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{native}',$2) WHERE node_id=$1")
        .bind(f.node.node_id)
        .bind(json!(native))
        .execute(&pool)
        .await
        .unwrap();
    let installed = post(
        aq_server::build_admin_router(f.state.clone()),
        "/api/intelligence/register",
        json!({}),
        None,
    )
    .await;
    assert_eq!(installed.status(), StatusCode::OK);
    let attempt = insert(&f).await;
    let lease = MaterialLease {
        schema_version: 1,
        lease_id: Uuid::new_v4(),
        binding: f.profiled.offer.binding.clone(),
        credential_profile: f.selection.choice.credential_profile.clone(),
        credential_revision: f.selection.credential_revision,
        provider: Label::new(&f.selection.choice.provider).unwrap(),
        account_reference: Label::new(&f.selection.choice.account_reference).unwrap(),
        endpoint_reference: Label::new(&f.selection.choice.endpoint_reference).unwrap(),
        purpose: Label::new("model-authentication").unwrap(),
        material_digest: Digest::of(SECRET),
        material_bytes: SECRET.len() as u64,
        issued_at_ms: NOW,
        expires_at_ms: NOW + 1000,
    };
    let owner = LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    };
    let op = Uuid::new_v4();
    let plan = PreparationPlan {
        schema_version: 1,
        operation_id: op,
        binding: lease.binding.clone(),
        reservation: NoLaunchReservation {
            reservation_id: owner.reservation_id,
            reservation_operation_id: Uuid::new_v4(),
            reservation_journal_digest: Digest::of(b"synthetic-reservation"),
        },
        guardian_id: owner.guardian_id,
        launch_id: owner.launch_id,
        clock: DeliveryClock::new(NOW, NOW + 1000).unwrap(),
        materials: vec![lease.clone()],
    };
    let doc = TransportDocument::PreparationPlan(plan);
    doc.validate().unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
        .bind(attempt)
        .execute(&mut *tx)
        .await
        .unwrap();
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(hot)).execute(&mut *tx).await.unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(owner);
    hot.preparation_operation_id = Some(op);
    sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'accept',$3,$4)").bind(f.node.node_id).bind(op).bind(Digest::of(b"synthetic-accept").as_str()).bind(json!(doc)).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&mut *tx)
        .await
        .unwrap();
    let identity = format!(
        "{}@{}",
        lease.credential_profile.id.as_str(),
        lease.credential_profile.version.as_str()
    );
    sqlx::query("INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,admitted_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$6,$7,$8,$9,$10,$11,$12,'admitted',$13,$14)").bind(lease.lease_id).bind(Uuid::new_v4()).bind(Digest::of(b"synthetic-material-issuance").as_str()).bind(f.node.node_id).bind(f.node.boot_id).bind(attempt).bind(lease.binding.offer_id).bind(identity).bind(lease.credential_revision as i64).bind(lease.purpose.as_str()).bind(json!(lease)).bind(Digest::of(b"private-verifier-not-a-response").as_str()).bind(NOW as i64).bind((NOW+1000)as i64).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO profiled_launch_materials(attempt_id,lease_id) VALUES($1,$2)")
        .bind(attempt)
        .bind(lease.lease_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let request = MaterialFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        binding: lease.binding,
        lease_id: lease.lease_id,
    };
    Case {
        f,
        request,
        path,
        _dir: dir,
    }
}
fn doc(c: &Case, bytes: &[u8]) -> MaterialRefresh {
    based(c, 0, bytes)
}
fn based(c: &Case, base_version: i64, bytes: &[u8]) -> MaterialRefresh {
    MaterialRefresh {
        base_version,
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: c.request.binding.clone(),
        lease_id: c.request.lease_id,
        content_digest: Digest::of(bytes),
    }
}
async fn refresh(c: &Case, doc: &MaterialRefresh, bytes: &[u8]) -> (StatusCode, Vec<u8>) {
    let response = aq_server::build_worker_router(c.f.state.clone())
        .oneshot(
            Request::post(PATH)
                .header("content-type", "application/octet-stream")
                .header("authorization", format!("Bearer {}", c.f.credential))
                .header(
                    HEADER,
                    URL_SAFE_NO_PAD.encode(serde_json::to_vec(doc).unwrap()),
                )
                .body(Body::from(bytes.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}
async fn rows(c: &Case) -> Vec<(i64, String, Option<Uuid>, String, Uuid)> {
    sqlx::query_as("SELECT sequence,origin,lease_id,content_digest,revision_id FROM credential_material_revisions ORDER BY sequence")
        .fetch_all(&c.f.db.pool().await)
        .await
        .unwrap()
}
fn accepted(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap()
}
fn ok(version: i64, replayed: bool) -> Value {
    json!({"accepted": true, "version": version, "replayed": replayed})
}
async fn newest(c: &Case) -> Option<String> {
    aq_server::profiled_execution::login_refresh::newest_source(
        &c.f.db.pool().await,
        &c.f.state.config.intelligence.clone().unwrap(),
        &c.f.selection.choice.credential_profile,
        c.f.selection.credential_revision,
    )
    .await
    .unwrap()
    .map(|h| h.digest)
}
#[tokio::test]
async fn writeback_is_idempotent_on_lease_and_digest() {
    let c = setup(CredentialKind::SubscriptionLogin).await;
    let first = doc(&c, REFRESHED);
    let (status, body) = refresh(&c, &first, REFRESHED).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(accepted(&body), ok(1, false));
    let mut replay = first.clone();
    replay.operation_id = Uuid::new_v4();
    let (status, body) = refresh(&c, &replay, REFRESHED).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accepted(&body), ok(1, true));
    let next: &[u8] = b"synthetic-second-refresh";
    let (status, body) = refresh(&c, &based(&c, 1, next), next).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accepted(&body), ok(2, false));
    let written = rows(&c).await;
    assert_eq!(written.len(), 2);
    assert!(
        written
            .iter()
            .all(|r| r.1 == "refresh" && r.2 == Some(c.request.lease_id))
    );
    assert_eq!(written[0].3, Digest::of(REFRESHED).as_str());
    // The next attempt is issued from the newest lineage revision.
    let registry = c.f.state.config.intelligence.clone().unwrap();
    let head = aq_server::profiled_execution::login_refresh::newest_source(
        &c.f.db.pool().await,
        &registry,
        &c.f.selection.choice.credential_profile,
        c.f.selection.credential_revision,
    )
    .await
    .unwrap()
    .expect("lineage head");
    assert_eq!(head.digest, Digest::of(next).as_str());
    assert_eq!(head.revision_id, written[1].4);
    // A digest that does not match the body is refused and writes nothing.
    let (status, _) = refresh(&c, &doc(&c, b"other"), b"different").await;
    assert_eq!(status, StatusCode::CONFLICT);
    // A revoked (closed) lease cannot write back.
    sqlx::query("UPDATE admission_material_leases SET state='revoked' WHERE lease_id=$1")
        .bind(c.request.lease_id)
        .execute(&c.f.db.pool().await)
        .await
        .unwrap();
    let late: &[u8] = b"synthetic-late-refresh";
    let (status, _) = refresh(&c, &based(&c, 2, late), late).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(rows(&c).await.len(), 2);
    assert_eq!(
        aq_server::admission::classify(PATH),
        aq_server::admission::Class::Bulk
    );
}
#[tokio::test]
async fn writeback_refuses_oversize_and_api_key() {
    let c = setup(CredentialKind::SubscriptionLogin).await;
    let big = vec![b'x'; 65_537];
    let (status, _) = refresh(&c, &doc(&c, &big), &big).await;
    assert_ne!(status, StatusCode::OK);
    let (status, _) = refresh(&c, &doc(&c, b""), b"").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(rows(&c).await.is_empty());
    let api = setup(CredentialKind::ApiKey).await;
    let (status, _) = refresh(&api, &doc(&api, REFRESHED), REFRESHED).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(rows(&api).await.is_empty());
}
#[tokio::test]
async fn stored_revision_file_is_owner_only() {
    let c = setup(CredentialKind::SubscriptionLogin).await;
    let (status, body) = refresh(&c, &doc(&c, REFRESHED), REFRESHED).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.windows(REFRESHED.len()).any(|w| w == REFRESHED));
    let store = c._dir.path().join("store");
    assert_eq!(
        std::fs::metadata(&store).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let revision = rows(&c).await[0].4;
    let file = store.join(format!("{revision}.material"));
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(std::fs::read(&file).unwrap(), REFRESHED);
}

#[tokio::test]
async fn writeback_accepts_newer_version() {
    let c = setup(CredentialKind::SubscriptionLogin).await;
    assert_eq!(newest(&c).await, None, "version 0 is the configured file");
    // "Version 1 based on 0", then "version 2 based on 1".
    let (status, body) = refresh(&c, &based(&c, 0, REFRESHED), REFRESHED).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(accepted(&body), ok(1, false));
    let next: &[u8] = b"synthetic-second-refresh";
    let (status, body) = refresh(&c, &based(&c, 1, next), next).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accepted(&body), ok(2, false));
    let versions: Vec<i64> = rows(&c).await.iter().map(|r| r.0).collect();
    assert_eq!(versions, vec![1, 2]);
    // The next attempt fetches the newest version.
    assert_eq!(newest(&c).await, Some(Digest::of(next).as_str().to_owned()));
}

#[tokio::test]
async fn writeback_rejects_stale_base_and_returns_current() {
    let c = setup(CredentialKind::SubscriptionLogin).await;
    let (status, _) = refresh(&c, &based(&c, 0, REFRESHED), REFRESHED).await;
    assert_eq!(status, StatusCode::OK);
    // A second "version 1 based on 0" lost the race: the server keeps its
    // current version and answers with it.
    let stale: &[u8] = b"synthetic-stale-refresh";
    for base in [0, 7] {
        let (status, body) = refresh(&c, &based(&c, base, stale), stale).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            accepted(&body),
            json!({"accepted": false, "version": 1, "replayed": false})
        );
        assert!(!body.windows(REFRESHED.len()).any(|w| w == REFRESHED));
    }
    assert_eq!(rows(&c).await.len(), 1);
    assert_eq!(
        newest(&c).await,
        Some(Digest::of(REFRESHED).as_str().to_owned())
    );
}

#[tokio::test]
async fn operator_replace_wins_over_agent_writeback() {
    let c = setup(CredentialKind::SubscriptionLogin).await;
    let (status, _) = refresh(&c, &based(&c, 0, REFRESHED), REFRESHED).await;
    assert_eq!(status, StatusCode::OK);
    // The operator import path (`aq-server import-login`) reads a plain-text
    // token file and stores it as the newest version.
    let operator: &[u8] = b"synthetic-operator-long-lived-token";
    let source = c._dir.path().join("operator-upload");
    std::fs::write(&source, operator).unwrap();
    let registry = c.f.state.config.intelligence.clone().unwrap();
    let identity = aq_server::profiled_execution::login_refresh::identity(
        &c.f.selection.choice.credential_profile,
    );
    let version = aq_server::intelligence::login_import::import(
        &c.f.state.store,
        &registry,
        &identity,
        c.f.selection.credential_revision,
        &source,
        NOW as i64,
    )
    .await
    .unwrap();
    assert_eq!(version, 2);
    let written = rows(&c).await;
    assert_eq!((written[1].1.as_str(), written[1].2), ("operator", None));
    assert_eq!(
        newest(&c).await,
        Some(Digest::of(operator).as_str().to_owned())
    );
    // The agent's later write-back, even based on the current version, is
    // refused: its lease was issued before the operator upload.
    let late: &[u8] = b"synthetic-late-refresh";
    for base in [1, 2] {
        let (status, body) = refresh(&c, &based(&c, base, late), late).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(!body.windows(operator.len()).any(|w| w == operator));
    }
    assert_eq!(rows(&c).await.len(), 2);
    // A second operator upload always wins too, even of identical bytes.
    let again = aq_server::intelligence::login_import::import(
        &c.f.state.store,
        &registry,
        &identity,
        c.f.selection.credential_revision,
        &source,
        NOW as i64,
    )
    .await
    .unwrap();
    assert_eq!(again, 3);
    // Unknown credential is refused without touching the store.
    assert!(
        aq_server::intelligence::login_import::import(
            &c.f.state.store,
            &registry,
            "missing@1",
            c.f.selection.credential_revision,
            &source,
            NOW as i64,
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn material_fetch_reports_login_version() {
    use aq_server::profiled_execution::{login_refresh, material_api::VERSION_HEADER};
    let c = setup(CredentialKind::SubscriptionLogin).await;
    let response = post(
        aq_server::build_worker_router(c.f.state.clone()),
        "/v1/worker/profiled/material",
        json!(c.request),
        Some(&c.f.credential),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // The configured file is version 0; the node echoes it as base_version.
    assert_eq!(response.headers()[VERSION_HEADER], "0");
    let (status, _) = refresh(&c, &based(&c, 0, REFRESHED), REFRESHED).await;
    assert_eq!(status, StatusCode::OK);
    // A lease issued from the refreshed bytes reports version 1.
    let raw: Value = sqlx::query_scalar("SELECT lease FROM admission_material_leases")
        .fetch_one(&c.f.db.pool().await)
        .await
        .unwrap();
    let mut lease: MaterialLease = serde_json::from_value(raw).unwrap();
    let registry = c.f.state.config.intelligence.clone().unwrap();
    let pool = c.f.db.pool().await;
    assert_eq!(
        login_refresh::lease_versioned(&pool, &registry, &lease)
            .await
            .unwrap()
            .1,
        Some(0)
    );
    lease.material_digest = Digest::of(REFRESHED);
    let (source, version) = login_refresh::lease_versioned(&pool, &registry, &lease)
        .await
        .unwrap();
    assert_eq!(version, Some(1));
    assert_eq!(source.unwrap().digest, Digest::of(REFRESHED).as_str());
}
