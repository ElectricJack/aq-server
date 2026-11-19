//! Production assembly over synthetic Graph/model fixture data. This proves
//! exact server compile/count/retention, never actual Codex/provider eligibility.
use crate::common;
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest, Text,
    control::{ControlReply, decode_reply},
};
use aq_server::profiled_execution::{definitions::Definition, policies};
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::Arc;
const STARTUP: &str = include_str!("../fixtures/agent-result-startup-v1.md");
fn reference(id: &str, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    }
}
/// The recipe the docker-local Codex profile pins (aq-admission `DOCKER_LOCAL_PROFILES`).
fn pinned_codex_recipe() -> ArtifactRef {
    let codex = &aq_admission::DOCKER_LOCAL_PROFILES[1];
    ArtifactRef {
        id: Text::new(codex.recipe_id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: serde_json::from_value(json!(codex.recipe_digest.unwrap())).unwrap(),
    }
}
fn reporting() -> ArtifactRef {
    reference("aq.agent-result-startup", STARTUP.as_bytes())
}
async fn enable(case: &mut common::native_offer::Case, include_startup: bool) {
    let recipe = reference("aq.codex-result-local-recipe", b"fixture recipe");
    let profile = "codex-0.166-trusted-node-named-readonly-result-local-v1";
    enable_as(
        case,
        include_startup,
        (profile, "aq.codex-result-local-adapter", recipe),
    )
    .await;
}
/// `enable` for another result-local `(profile, adapter id, recipe)`.
async fn enable_as(
    case: &mut common::native_offer::Case,
    include_startup: bool,
    (profile, adapter_id, recipe): (&str, &str, ArtifactRef),
) {
    let mut config = (*case.env.env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    // Data-only test definition; no installation/runtime success is asserted.
    let body=json!({"schema_version":1,"profile":profile,
        "runtime":profiled.qualified_runtimes[0].runtime,"environment":case.env.input.node.harnesses[0].environment,
        "client_manifest":reference("aq.agent-result-client-installation",b"fixture"),"client_executable":reference("client",b"fixture"),
        "contract":{"id":"aq.agent-result-local","version":"1","digest":"4fe711c405ead85b55a54de2d7446b2c106c4b8809b16bc57497ba2af1d35ef9"},
        "recipe":recipe,
        "permission_digest":Digest::of(b"fixture whole permission metadata"),"reserved_client_bytes":67108864}).to_string();
    let adapter = reference(adapter_id, body.as_bytes());
    profiled
        .definition_bodies
        .push(Definition::new(adapter.clone(), ArtifactKind::Definition, body).unwrap());
    if include_startup {
        profiled
            .definition_bodies
            .push(Definition::new(reporting(), ArtifactKind::Definition, STARTUP.into()).unwrap());
    }
    profiled.revision += 1;
    profiled.validate().unwrap();
    let mut registry = config.intelligence.as_ref().unwrap().config.clone();
    registry.revision += 1;
    registry.qualified_harnesses[0].adapter = adapter.clone();
    config.intelligence = Some(Arc::new(registry.load().unwrap()));
    config.profiled_execution = Some(Arc::new(profiled));
    case.env.env.state.config = Arc::new(config);
    let app = aq_server::build_admin_router(case.env.env.state.clone());
    assert_eq!(
        common::selection::call(
            &app,
            "/api/intelligence/register",
            Some(json!({})),
            "owner@localhost"
        )
        .await
        .0,
        200
    );
    let row: Value = sqlx::query_scalar(
        "SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1 AND boot_id=$2",
    )
    .bind(case.request.identity.node_id)
    .bind(case.request.identity.boot_id)
    .fetch_one(&case.env.env.pool)
    .await
    .unwrap();
    let mut snapshot: aq_selection::CapabilitySnapshot = serde_json::from_value(row).unwrap();
    snapshot.harnesses[0].adapter = adapter;
    // This is deliberately synthetic fixture input, matching the existing
    // helper's node data. It is not used by GATE04's actual-owner constructor.
    sqlx::query(
        "UPDATE intelligence_node_snapshots SET snapshot=$3 WHERE node_id=$1 AND boot_id=$2",
    )
    .bind(case.request.identity.node_id)
    .bind(case.request.identity.boot_id)
    .bind(json!(snapshot))
    .execute(&case.env.env.pool)
    .await
    .unwrap();
    policies::install(&case.env.env.state).await.unwrap();
}
async fn published(case: &common::native_offer::Case) -> aq_admission::startup::NativeOffer {
    let (status, body) = common::native_offer::poll_offered(case).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("Poll required")
    };
    page.validate_for(&case.request).unwrap();
    assert_eq!(page.offers.len(), 1);
    page.offers.remove(0)
}
#[tokio::test]
async fn production_compile_seal_count_retain_uses_exact_public_reporting_instruction_once() {
    let mut case = common::native_offer::setup().await;
    enable(&mut case, true).await;
    let offered = published(&case).await;
    let reference = reporting();
    let manifest = &offered.offered.offer.manifest;
    assert_eq!(
        manifest.modules.iter().filter(|r| **r == reference).count(),
        1
    );
    let contexts: Vec<_> = manifest
        .context
        .references
        .iter()
        .filter(|r| r.artifact == reference)
        .collect();
    assert_eq!(contexts.len(), 1);
    assert!(contexts[0].required);
    assert_eq!(
        contexts[0].delivery,
        aq_session_protocol::ContextDelivery::Startup
    );
    assert_eq!(contexts[0].bytes, 1929);
    assert!(contexts[0].tokens >= 1929);
    let spans: Vec<_> = offered
        .startup
        .startup_inputs
        .iter()
        .filter(|r| r.artifact == reference)
        .collect();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].bytes, 1929);
    let body: Vec<u8> =
        sqlx::query_scalar("SELECT body FROM admission_artifacts WHERE descriptor->'artifact'=$1")
            .bind(json!(offered.startup.payload))
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    offered.startup.verify_payload(&body).unwrap();
    let begin = spans[0].offset as usize;
    assert_eq!(&body[begin..begin + 1929], STARTUP.as_bytes());
    assert_eq!(
        body.windows(1929)
            .filter(|b| *b == STARTUP.as_bytes())
            .count(),
        1
    );
    let retained=sqlx::query("SELECT body,descriptor,content_digest,byte_length FROM admission_artifacts WHERE descriptor->'artifact'=$1")
        .bind(json!(reference)).fetch_one(&case.env.env.pool).await.unwrap();
    assert_eq!(retained.get::<Vec<u8>, _>("body"), STARTUP.as_bytes());
    assert_eq!(retained.get::<i64, _>("byte_length"), 1929);
    assert_eq!(
        retained.get::<String, _>("content_digest"),
        reference.digest.as_str()
    );
    let descriptor: aq_admission::RetainedArtifact =
        serde_json::from_value(retained.get("descriptor")).unwrap();
    assert_eq!(descriptor.kind, ArtifactKind::Context);
    assert!(manifest.context.startup_byte_budget >= body.len() as u64);
    assert!(
        manifest
            .permissions
            .effective
            .iter()
            .all(|p| p.operation.as_str() != "result.submit")
    );
}
#[tokio::test]
async fn selected_result_profile_without_exact_owned_reporting_bytes_publishes_no_offer() {
    let mut case = common::native_offer::setup().await;
    enable(&mut case, false).await;
    let (status, body) = common::native_offer::poll(&case).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_launches")
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn ordinary_plain_profile_keeps_original_instruction_and_artifact_path() {
    let case = common::native_offer::setup().await;
    let offered = published(&case).await;
    let reference = reporting();
    let manifest = &offered.offered.offer.manifest;
    assert!(!manifest.modules.contains(&reference));
    assert!(
        manifest
            .context
            .references
            .iter()
            .all(|r| r.artifact != reference)
    );
    assert!(
        offered
            .startup
            .startup_inputs
            .iter()
            .all(|r| r.artifact != reference)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM admission_artifacts WHERE descriptor->'artifact'=$1"
        )
        .bind(json!(reference))
        .fetch_one(&case.env.env.pool)
        .await
        .unwrap(),
        0
    );
}

/// aq-worker W1: the docker-local Codex adapter and profile are admitted like the
/// Claude one, and publish the reporting instruction once.
#[tokio::test]
async fn docker_local_codex_adapter_publishes_the_reporting_instruction_once() {
    let mut case = common::native_offer::setup().await;
    let recipe = pinned_codex_recipe();
    let selected = (
        "codex-0.166-docker-local-result-local-v1",
        "aq.codex-docker-result-local-adapter",
        recipe,
    );
    enable_as(&mut case, true, selected).await;
    let offered = published(&case).await;
    let reference = reporting();
    let manifest = &offered.offered.offer.manifest;
    assert_eq!(
        manifest.modules.iter().filter(|r| **r == reference).count(),
        1
    );
}
/// The docker-local Codex profile under another adapter identity (here the
/// interactive Codex one) is refused: no offer is published.
#[tokio::test]
async fn docker_local_codex_profile_under_another_adapter_publishes_no_offer() {
    let mut case = common::native_offer::setup().await;
    let recipe = reference(
        "aq.codex-docker-local-result-local-recipe",
        b"fixture recipe",
    );
    let selected = (
        "codex-0.166-docker-local-result-local-v1",
        "aq.codex-result-local-adapter",
        recipe,
    );
    enable_as(&mut case, true, selected).await;
    let (status, body) = common::native_offer::poll(&case).await;
    assert_eq!(status, 409, "{body}");
}
