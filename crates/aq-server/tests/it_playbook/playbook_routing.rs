//! Routing on the authoring path, end to end (spec 0013 §7; plan task D2):
//! the routing document saved in Settings is authored into a playbook, the
//! playbook is activated on `aq.routing` under the routing source's grant,
//! the switch turns on, and a new task is routed by a run of that playbook
//! and placed on the routed level's document.
//!
//! Real here: the project binding, the level documents, the worker poll
//! and placement, the routing document, the playbook service, driver
//! and engine on protected PostgreSQL, the server's own routing port
//! (`ServerRouting`), the save-time checks and the grant. Synthetic: the
//! Graph peer, the judgment provider, and the agent, whose outcome (the
//! engine's worked example `escalate-on-judgment`) is handed to the save
//! path without a model.
use crate::common::native_offer::{Case, poll, poll_offered, setup};
use crate::common::roles;
use aq_admission::control::{ControlReply, decode_reply};
use aq_playbook_authoring::{self as authoring, compiler::docs};
use aq_server::{
    InternalWork,
    playbooks::{
        driver::Driver,
        host_adapters::{
            AssessReply, AssessRequest, GatewayGraph, JudgmentProvider, JudgmentReply,
            JudgmentRequest, ProviderFailure,
        },
        review_domain::CatalogProfile,
        service::{Namespace, Service},
    },
    protected_storage::{KeyFile, Keyring, ProtectionConfig},
    routing,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Arc};
use uuid::Uuid;

const CLASS_PROFILE: &str = "intelligence:standard-high";
const CLASS_MODEL: &str = "model-c";

/// As `it_native` `native_offers::routed::on_the_router`: the level document
/// `standard-high` saved with its own model, as the owner would in Settings.
async fn on_the_router(c: &mut Case) {
    let level = roles::open_level("standard-high", &[("fixture-a", CLASS_MODEL, "high")]);
    let pool = &c.env.env.pool;
    let kind = aq_profiles::roles::Kind::Intelligence;
    roles::save(pool, c.env.actor, kind, "standard-high", &level).await;
    c.env.snapshot = roles::active(pool).await;
}

/// The owner's Settings: the routing document.
async fn save_structure(pool: &sqlx::PgPool, markdown: &str) {
    let actor: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users ORDER BY created_at LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap();
    let (_, revision) = aq_role_settings::routing_document(pool).await.unwrap();
    let id = uuid::Uuid::new_v4();
    aq_role_settings::save_routing_document(pool, actor, markdown, revision, id, 1)
        .await
        .unwrap();
}

/// Answers every field `medium` (or its first choice) and no flag.
struct MediumRisk;
#[async_trait::async_trait]
impl JudgmentProvider for MediumRisk {
    fn credential_profile(&self, _: Uuid, _: Uuid) -> Option<String> {
        Some("profile/judge".into())
    }
    async fn complete(&self, _: &JudgmentRequest) -> Result<JudgmentReply, ProviderFailure> {
        Err(ProviderFailure::Unavailable)
    }
    async fn assess(&self, r: &AssessRequest) -> Result<AssessReply, &'static str> {
        let properties = |part: &str| {
            let fields = r.schema["properties"][part]["properties"].as_object();
            fields.unwrap().clone()
        };
        let labels: serde_json::Map<String, Value> = (properties("labels").into_iter())
            .map(|(id, field)| {
                let choices = field["enum"].as_array().unwrap();
                let medium = choices.iter().find(|c| *c == "medium");
                (id, medium.unwrap_or(&choices[0]).clone())
            })
            .collect();
        let flags: serde_json::Map<String, Value> = (properties("flags").into_iter())
            .map(|(id, _)| (id, json!(false)))
            .collect();
        Ok(AssessReply {
            answer: json!({"labels": labels, "flags": flags, "rationale": "Shared code."}),
            provider: "fixture".into(),
            model: "model-a".into(),
            input_tokens: 1,
            output_tokens: 1,
        })
    }
}

fn example(path: &str) -> &'static str {
    let path = format!("escalate-on-judgment/{path}");
    let files = docs::example_files();
    files.into_iter().find(|(p, _)| *p == path).unwrap().1
}

/// What `aq-playbooks save` leaves in `out/outcome.json` for the example.
fn saved_outcome() -> aq_internal_work::Outcome {
    let cases: Vec<Value> = (docs::example_files().into_iter())
        .filter(|(path, _)| path.starts_with("escalate-on-judgment/cases/"))
        .map(|(_, text)| serde_json::from_str(text).unwrap())
        .collect();
    let digest = format!("sha256:{}", "0".repeat(64));
    aq_internal_work::Outcome::Answered {
        answer: json!({"kind": "saved", "cases": cases, "artifact": digest, "facts_digest": digest,
            "draft": serde_json::from_str::<Value>(example("draft.json")).unwrap()}),
        provider: "anthropic".into(),
        model: "claude-fixture".into(),
        way: aq_internal_work::Way::Login,
        elapsed_ms: 1,
        usage: None,
        attempts: vec![],
    }
}

fn offered(body: &Value) -> Vec<String> {
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap() else {
        panic!("{body}")
    };
    (page.offers.iter())
        .map(|o| {
            let profile = &o.offered.offer.manifest.intelligence_profile;
            profile.id.as_str().to_owned()
        })
        .collect()
}

#[tokio::test]
async fn a_saved_routing_document_routes_a_new_task_and_places_it_on_the_routed_level() {
    Box::pin(routes_with_saved_artifact(CatalogProfile::Host9)).await;
}

#[cfg(feature = "delivery_playbook")]
#[tokio::test]
async fn host10_reinstalls_and_runs_the_active_host9_routing_artifact() {
    Box::pin(routes_with_saved_artifact(CatalogProfile::Host10)).await;
}

async fn routes_with_saved_artifact(target: CatalogProfile) {
    let mut c = setup().await;
    let wired = InternalWork::new(
        c.env.env.pool.clone(),
        c.env.env.state.clock.clone(),
        None,
        None,
    );
    c.env.env.state.internal_work = wired.unwrap();
    on_the_router(&mut c).await;
    let (state, pool) = (c.env.env.state.clone(), c.env.env.pool.clone());
    let (actor, lineage) = (c.env.actor, c.env.lineage.clone());
    let task = lineage.task_id;

    // A host_v9 namespace with installation routing, on the server's own ports.
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
        name: format!("routing-{}", Uuid::new_v4()),
        workspace_id: lineage.workspace_id,
        project_id: lineage.project_id,
        owner_user_id: actor,
    };
    let graph = Arc::new(GatewayGraph(state.agent_access.clone().unwrap()));
    let judge = Arc::new(MediumRisk);
    let keys = Arc::new(keys);
    let mut service = Service::new_host2(
        &state,
        namespace.clone(),
        keys.clone(),
        CatalogProfile::Host9,
        graph.clone(),
        None,
        Some(judge.clone()),
        None,
    )
    .await
    .unwrap();
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
            "routing",
            BTreeMap::from([("routing".into(), [9; 32])]),
        );
        aq_playbook_host::protected::storage(
            client.map_err(|_| aq_playbook_host::Error::Unknown)?,
            keys.unwrap(),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(500),
        )
    };
    let limits = aq_playbook_host::Limits::default();
    let mut driver = Arc::new(
        Driver::start(service, limits.clone(), factory.clone())
            .await
            .unwrap(),
    );
    state.internal_work.test_engine(driver.clone());

    // (1) The built-in source exists; with no playbook the switch stays off.
    routing::ensure_source(&pool).await.unwrap();
    assert!(routing::set_enabled(&pool, true).await.is_err());

    // (2) The owner saves the routing document: one authoring run starts.
    save_structure(&pool, example("source.md")).await;
    routing::author(&state).await;
    let runs = authoring::in_flight(&pool).await.unwrap();
    assert_eq!(runs.len(), 1, "one authoring run for the routing source");
    let internal = runs[0].internal_task_id.unwrap();
    let submitted = aq_internal_work::store::get(&pool, internal)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        submitted.purpose,
        aq_internal_work::Purpose::PlaybookCompilation
    );
    let files = submitted.request.agent.unwrap().files;
    assert_eq!(files["source.md"], example("source.md"));
    let facts: Value = serde_json::from_str(&files["facts.json"]).unwrap();
    // Every level document of the fixture is a level the document may name.
    let levels = facts["levels"].as_array().unwrap();
    assert_eq!(levels.len(), roles::levels().len(), "{levels:?}");
    assert!(levels.contains(&json!("standard-high")), "{levels:?}");
    assert_eq!(
        facts["offers"]["standard-high"]["providers"],
        json!(["fixture-a"])
    );
    // Saving again with nothing changed starts no second run.
    routing::author(&state).await;
    assert_eq!(authoring::in_flight(&pool).await.unwrap().len(), 1);

    // (3) The agent saves; the server checks, installs and activates.
    authoring::completed(&state, internal, &saved_outcome())
        .await
        .unwrap();
    let active = (authoring::active(&pool, routing::ROUTING_SOURCE)
        .await
        .unwrap())
    .expect("the routing playbook is in effect");
    let bound = driver
        .inspect_binding(routing::BINDING.into())
        .await
        .unwrap();
    assert_eq!(bound["artifact"], active.artifact_digest);
    let grant: Uuid = sqlx::query_scalar(
        "SELECT grant_id FROM playbook_source_grants WHERE source_id='routing' AND revoked_at_ms IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        bound["authority"]["actor"]
            .as_str()
            .unwrap()
            .contains(&grant.to_string())
    );

    // (4) The switch turns on, and the read shows the source.
    let app = aq_server::build_admin_router(state.clone());
    let path = "/api/settings/routing";
    let on = Some(json!({"enabled": true}));
    let (status, view) = crate::common::selection::call(&app, path, on, "owner@localhost").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["enabled"], true);
    assert_eq!(view["source"]["id"], "routing");
    assert_eq!(view["source"]["active"]["artifact"], active.artifact_digest);
    assert_eq!(view["source"]["latest"]["status"], "saved");

    if target != CatalogProfile::Host9 {
        driver.stop().await.unwrap();
        let mut upgraded = Service::new_host2(
            &state,
            Namespace {
                name: format!("routing-{}", Uuid::new_v4()),
                namespace_id: Uuid::new_v4(),
                ..namespace
            },
            keys,
            target,
            graph,
            None,
            Some(judge),
            None,
        )
        .await
        .unwrap();
        upgraded.installation_routing = true;
        upgraded.initialize(actor).await.unwrap();
        driver = Arc::new(Driver::start(upgraded, limits, factory).await.unwrap());
        state.internal_work.test_engine(driver.clone());
        assert_eq!(authoring::reinstall_active(&state).await.unwrap(), 1);
        let binding = driver
            .inspect_binding(routing::BINDING.into())
            .await
            .unwrap();
        assert_eq!(binding["artifact"], active.artifact_digest);
        assert!(
            !binding["authority"]["actor"]
                .as_str()
                .unwrap()
                .contains(&grant.to_string())
        );
    }

    // (5) A ready task without a route waits, the server asks for a route,
    // and a run of the playbook reads the task, asks the judgment and routes it.
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        offered(&body).is_empty(),
        "an unrouted task is not offered: {body}"
    );
    for _ in 0..300 {
        driver.quantum().await.unwrap();
        if c.metadata.lock().unwrap().get("aq:route").is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let route = c.metadata.lock().unwrap().get("aq:route").cloned();
    let route = route.expect("the playbook routed the task");
    assert_eq!(
        (&route["c"], &route["s"]),
        (&json!("standard-high"), &json!("router"))
    );
    assert_eq!(route["d"], active.artifact_digest[..16]);
    let history = routing::history(&pool, task).await.unwrap();
    let decided = history[0]
        .route
        .as_ref()
        .expect("the route is in the history");
    assert_eq!(
        decided.judgment.as_ref().unwrap()["labels"],
        json!(["medium"])
    );
    assert!(!decided.reasons.is_empty());

    // The routed task is offered on the level document of its class.
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered(&body), [CLASS_PROFILE]);
    driver.stop().await.unwrap();
}
