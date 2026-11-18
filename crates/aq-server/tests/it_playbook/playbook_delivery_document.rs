//! The delivery document on the authoring path (spec 0016 §8; Epic 0 task S11):
//! saving `/api/settings/delivery-document` starts an authoring run of the
//! built-in `delivery` source, against catalog 2.3.0; an artifact activated on
//! `aq.delivery` receives `aq.work.result_ready`.
//!
//! Real here: the Settings route, the delivery document, the playbook service,
//! driver and engine on protected PostgreSQL, the source's grant, the server's
//! save-time seal, and the engine's own announcement of a result. Synthetic:
//! the Graph peer, and the server's record of results (a port with one pending
//! result). The agent's work is not run: the artifact it would save is sealed
//! here from a draft that reads the task, and installed through the server's
//! own deployer, as `authoring::completed` does after the checks.
use crate::common::native_offer::setup;
use aq_playbook_authoring::{
    self as authoring, Deployer,
    builtin::{DELIVERY, DELIVERY_BINDING},
    compiler::draft,
};
use aq_server::{
    playbooks::{
        domain::{
            Result as DomainResult,
            delivery_store::{DeliveryPort, Latest, Pending},
        },
        driver::Driver,
        host_adapters::GatewayGraph,
        review_domain::CatalogProfile,
        service::{Namespace, Service},
    },
    protected_storage::{KeyFile, Keyring, ProtectionConfig},
};
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;

const DOCUMENT: &str = "When a task has a result, read the task and settle its result.\n";
const PATH: &str = "/api/settings/delivery-document";
/// A playbook that reads the task its trigger names and ends.
const READS_THE_TASK: &str = include_str!("fixtures/delivery_reads_the_task.draft.json");

/// What the server records of results: one task stopped with a result.
struct Port(Pending, Latest);
#[async_trait::async_trait]
impl DeliveryPort for Port {
    async fn pending(&self, _: Option<Uuid>, _: u32) -> DomainResult<Vec<Pending>> {
        Ok(vec![self.0])
    }
    async fn latest(&self, _: Uuid) -> DomainResult<Option<Latest>> {
        Ok(Some(self.1.clone()))
    }
    async fn bound(&self, _: Uuid) -> DomainResult<bool> {
        Ok(true)
    }
}

/// `PUT` of the owner's delivery document, as the Settings page sends it.
async fn put(app: &axum::Router, markdown: &str, revision: u64) -> (u16, Value) {
    let body = json!({"markdown": markdown, "expected_revision": revision.to_string(),
        "operation_id": Uuid::new_v4()});
    let request = Request::builder()
        .method("PUT")
        .uri(PATH)
        .header("content-type", "application/json")
        .header("X-AQ-Request", "1")
        .header("x-aq-dev-email", "owner@localhost")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_saved_delivery_document_starts_authoring_and_an_activated_artifact_receives_result_ready()
 {
    let c = setup().await;
    let wired = aq_server::InternalWork::new(
        c.env.env.pool.clone(),
        c.env.env.state.clock.clone(),
        None,
        None,
    );
    let mut state = c.env.env.state.clone();
    state.internal_work = wired.unwrap();
    let (pool, actor, lineage) = (c.env.env.pool.clone(), c.env.actor, c.env.lineage.clone());

    // A host_v10 namespace on the server's own ports, announcing one result.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("key");
    std::fs::write(&path, [9_u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let keys = Keyring::load(&ProtectionConfig {
        active_key_id: "test".into(),
        keys: vec![KeyFile {
            id: "test".into(),
            file: path,
        }],
    })
    .unwrap();
    let namespace = Namespace {
        namespace_id: Uuid::new_v4(),
        generation: 1,
        name: format!("delivery-{}", Uuid::new_v4()),
        workspace_id: lineage.workspace_id,
        project_id: lineage.project_id,
        owner_user_id: actor,
    };
    let pending = Pending {
        task_id: lineage.task_id,
        workspace_id: lineage.workspace_id,
        project_id: lineage.project_id,
        attempt_id: Uuid::new_v4(),
        result_id: Uuid::new_v4(),
    };
    let latest = Latest {
        workspace_id: pending.workspace_id,
        project_id: pending.project_id,
        attempt_id: pending.attempt_id,
        result_id: pending.result_id,
        outcome: "success".into(),
        changed: true,
        head: "a".repeat(40),
        base: "b".repeat(40),
    };
    let graph = Arc::new(GatewayGraph(state.agent_access.clone().unwrap()));
    let mut service = Service::new_host2(
        &state,
        namespace,
        Arc::new(keys),
        CatalogProfile::Host10,
        graph,
        None,
        None,
        None,
    )
    .await
    .unwrap()
    .with_delivery(Arc::new(Port(pending, latest)));
    service.installation_routing = true;
    service.initialize(actor).await.unwrap();
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    url.set_path(c.env.env._db.options().get_database().unwrap());
    let factory = move || {
        let config = url
            .as_str()
            .parse::<aq_playbook_host::postgres_client::Config>();
        let client = (config.unwrap()).connect(aq_playbook_host::postgres_client::NoTls);
        let keys = aq_playbook_host::postgres_storage::ProtectionKeyring::new(
            "delivery",
            BTreeMap::from([("delivery".into(), [9; 32])]),
        );
        aq_playbook_host::protected::storage(
            client.map_err(|_| aq_playbook_host::Error::Unknown)?,
            keys.unwrap(),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(500),
        )
    };
    let limits = aq_playbook_host::Limits::default();
    let driver = Arc::new(Driver::start(service, limits, factory).await.unwrap());
    state.internal_work.test_engine(driver.clone());

    // (1) The built-in source exists, with the delivery record; no document, no run.
    authoring::author_document(&state, &authoring::builtin::delivery()).await;
    let record = authoring::get(&pool, DELIVERY).await.unwrap().unwrap();
    assert_eq!(record, {
        let mut expected = authoring::builtin::delivery();
        expected.revision = record.revision;
        expected
    });
    assert_eq!(record.binding, DELIVERY_BINDING);
    assert_eq!(
        DELIVERY_BINDING,
        aq_server::playbooks::domain::host10::DELIVERY_BINDING
    );
    assert!(authoring::in_flight(&pool).await.unwrap().is_empty());

    // (2) The owner saves the document in Settings: one authoring run starts,
    // with catalog 2.3.0 and the delivery record in its workspace.
    let app = aq_server::build_admin_router(state.clone());
    let (status, saved) = put(&app, DOCUMENT, 0).await;
    assert_eq!(status, 200, "{saved}");
    let runs = authoring::in_flight(&pool).await.unwrap();
    assert_eq!(runs.len(), 1, "one authoring run for the delivery source");
    let internal = runs[0].internal_task_id.unwrap();
    let submitted = aq_internal_work::store::get(&pool, internal)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        submitted.purpose,
        aq_internal_work::Purpose::PlaybookCompilation
    );
    assert_eq!(submitted.request.affected.id, DELIVERY);
    let files = submitted.request.agent.unwrap().files;
    assert_eq!(files["source.md"], DOCUMENT);
    let record: Value = serde_json::from_str(&files["record.json"]).unwrap();
    assert_eq!(record["id"], DELIVERY);
    assert_eq!(record["binding"], DELIVERY_BINDING);
    assert_eq!(record["events"], json!(["aq.work.result_ready"]));
    let catalog: Value = serde_json::from_str(&files["catalog.json"]).unwrap();
    assert_eq!(catalog["commands"]["work.read"]["version"], "2.3.0");
    for command in [
        "work.complete",
        "work.return",
        "work.hold",
        "branch.publish",
    ] {
        assert!(catalog["commands"].get(command).is_some(), "{command}");
    }
    assert!(catalog["events"].get("aq.work.result_ready").is_some());
    assert!(files["CATALOG.md"].contains("aq.work.result_ready"));
    // Saving the same document again, or other settings, starts no second run.
    authoring::author_document(&state, &authoring::builtin::delivery()).await;
    assert_eq!(authoring::in_flight(&pool).await.unwrap().len(), 1);
    // The document is the Settings value; nothing else needs saving.
    let (read, revision) = aq_role_settings::delivery_document(&pool).await.unwrap();
    // A document's revision is the store revision of its save (as for routing).
    let saved_revision: i64 = saved["revision"].as_str().unwrap().parse().unwrap();
    assert_eq!((read.as_str(), revision), (DOCUMENT, saved_revision));

    // (3) The artifact the agent saves is put into effect on `aq.delivery`
    // under the source's grant: the server's deployer installs and activates it.
    let delivery = authoring::builtin::delivery();
    let catalog = authoring::request::pinned_catalog(&delivery).unwrap();
    let workspace = draft::Workspace {
        markdown: DOCUMENT.into(),
        record: authoring::request::engine_record(&delivery, "settings-1"),
        catalog: authoring::request::catalog_for(&delivery, &catalog).unwrap(),
        facts: BTreeMap::new(),
        draft: serde_json::from_str(READS_THE_TASK).unwrap(),
    };
    let artifact = draft::seal(&workspace).unwrap_or_else(|faults| panic!("{faults:?}"));
    state
        .ensure_grant(DELIVERY, &delivery.capabilities)
        .await
        .unwrap();
    state.install(&artifact).await.unwrap();
    state
        .activate(DELIVERY, DELIVERY_BINDING, &artifact.digest)
        .await
        .unwrap();
    let bound = driver
        .inspect_binding(DELIVERY_BINDING.into())
        .await
        .unwrap();
    assert_eq!(bound["artifact"], artifact.digest);

    // (4) The engine announces the stopped result; a run of the artifact reads
    // the task.
    let sql = "SELECT count(*) FROM playbook_effects WHERE command_name='work.read' AND receipt->'subject'->>'id'=$1";
    let mut reads = 0_i64;
    for _ in 0..300 {
        driver.quantum().await.unwrap();
        reads = sqlx::query_scalar(sql)
            .bind(lineage.task_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        if reads > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        reads > 0,
        "the activated playbook received aq.work.result_ready"
    );
    driver.stop().await.unwrap();
}
