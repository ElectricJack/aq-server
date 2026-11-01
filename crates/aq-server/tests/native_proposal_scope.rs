//! Cross-node protocol authorization with two genuinely authenticated registered
//! nodes. Graph/runtime/source qualification remains synthetic; no process launches.
mod common;
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest, Text,
    control::{ControlReply, decode_reply},
    preparation_v2::{self as phase, FactKind, OwnerAuthority, PhasePolicy},
    transport::{Register, WorkerIdentity},
};
use aq_server::{
    profiled_execution::{
        accepted_preparation::{self as owner, PolicyDefinition, ProposalRequest},
        definitions::Definition,
        policies, registration,
    },
    store::NewNode,
    tokens, worker_api,
};
use std::sync::Arc;
use uuid::Uuid;
fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
async fn auth(state: &aq_server::AppState, token: &str) -> worker_api::AuthenticatedNode {
    let mut h = axum::http::HeaderMap::new();
    h.insert("authorization", format!("Bearer {token}").parse().unwrap());
    worker_api::authenticate_context(state, &h).await.unwrap()
}
#[tokio::test]
async fn authenticated_other_node_cannot_read_or_poison_foreign_original_proposal() {
    let mut c = common::native_offer::setup().await;
    let mut config = (*c.env.env.state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    let r = &p.qualified_runtimes[0];
    let policy = PolicyDefinition {
        schema_version: 1,
        contract: phase::contract_artifact().unwrap(),
        runtime: r.runtime.clone(),
        qualification: r.qualification.clone(),
        phase: PhasePolicy {
            schema_version: 2,
            maximum_preparation_ms: 10000,
            maximum_execution_lease_ms: 10000,
            maximum_observation_ms: 2000,
        },
        source_profile: reference("synthetic-source-profile"),
        source_disk_bytes: 1 << 20,
        owners: FactKind::all()
            .iter()
            .map(|k| OwnerAuthority {
                kind: *k,
                authority: reference(&format!("synthetic-owner/{k:?}")),
            })
            .collect(),
    };
    let body = serde_json::to_string(&policy).unwrap();
    p.definition_bodies.push(
        Definition::new(
            ArtifactRef {
                id: Text::new("aq.accepted-preparation-policy/crossnode-regression").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(body.as_bytes()),
            },
            ArtifactKind::Definition,
            body,
        )
        .unwrap(),
    );
    p.revision += 1;
    config.profiled_execution = Some(Arc::new(p));
    c.env.env.state.config = Arc::new(config);
    policies::install(&c.env.env.state).await.unwrap();
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(version, aq_server::STATE_FORMAT_VERSION.to_string());
    let present:bool=sqlx::query_scalar("SELECT to_regclass('profiled_v2_proposals') IS NOT NULL AND to_regclass('profiled_v2_accepts') IS NOT NULL").fetch_one(&c.env.env.pool).await.unwrap();
    assert!(
        present,
        "ordered current schema must own the proposal tables"
    );
    let store = &c.env.env.state.store;
    let owner = store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .find(|u| u.role == aq_server::store::Role::Owner)
        .unwrap();
    let now = (c.env.env.state.clock)();
    let enrollment = Uuid::new_v4().to_string();
    store
        .create_enrollment(
            &enrollment,
            owner.id,
            now,
            now + chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
    let token = tokens::new_node_credential();
    let b = store
        .redeem_enrollment(
            &enrollment,
            NewNode {
                name: "other-authenticated-worker".into(),
                credential_hash: tokens::verifier(&token),
                worker_version: "synthetic-test".into(),
                protocol_version: 2,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    let raw: serde_json::Value = sqlx::query_scalar(
        "SELECT state#>'{native,request}' FROM execution_nodes WHERE node_id=$1",
    )
    .bind(c.request.identity.node_id)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    let mut register: Register = serde_json::from_value(raw).unwrap();
    register.operation_id = Uuid::new_v4();
    register.identity = WorkerIdentity {
        node_id: b.id,
        boot_id: Uuid::new_v4(),
    };
    let b_auth = auth(&c.env.env.state, &token).await;
    registration::register(&c.env.env.state, &b_auth, &register)
        .await
        .unwrap();
    let (status, body) = common::native_offer::poll(&c).await;
    assert_eq!(status, 200);
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("expected native offer")
    };
    assert_eq!(page.offers.len(), 1);
    let offer = page.offers.remove(0);
    let negotiation = phase::Negotiation {
        contract: phase::contract_artifact().unwrap(),
        features: phase::required_features(),
    };
    let foreign = ProposalRequest {
        identity: register.identity.clone(),
        binding: offer.offered.offer.binding.clone(),
        negotiation: negotiation.clone(),
    };
    let result = owner::propose(&c.env.env.state, &b_auth, &foreign).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_v2_proposals")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "foreign node must neither retain nor poison another attempt's proposal"
    );
    assert!(result.is_err());
    assert_eq!(result.err().unwrap().status(), 403);
    let own = ProposalRequest {
        identity: c.request.identity.clone(),
        binding: offer.offered.offer.binding.clone(),
        negotiation,
    };
    let a_auth = auth(&c.env.env.state, &c.env.env.credential).await;
    owner::propose(&c.env.env.state, &a_auth, &own)
        .await
        .unwrap();
    let indexed: Uuid =
        sqlx::query_scalar("SELECT node_id FROM profiled_v2_proposals WHERE attempt_id=$1")
            .bind(own.binding.attempt_id)
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(indexed, c.request.identity.node_id);
    let mut wrong_boot = own;
    wrong_boot.identity.boot_id = Uuid::new_v4();
    assert_eq!(
        owner::propose(&c.env.env.state, &a_auth, &wrong_boot)
            .await
            .err()
            .unwrap()
            .status(),
        403
    );
}
