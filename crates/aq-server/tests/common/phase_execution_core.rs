// Actual phase issuance SQL/owners; synthetic Graph, Git and worker installation.
// No physical host, real provider, or production route qualification.
use crate::common;
use crate::common::native_offer::{self, Case};
use crate::common::presence as presence_fixture;
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest, Resources, Text,
    control::{ControlReply, decode_reply},
};
use aq_server::{
    profiled_execution::{definitions::Definition, policies},
    worker_api,
};
use axum::body::to_bytes;
use base64::Engine;
use owner::{PolicyDefinition, ProposalRequest};
use phase::{AcceptRequest, FactKind, OwnerAuthority, PhasePolicy, ResourceHold};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;
pub fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
pub async fn configure(
    c: &mut Case,
    extra: fn(&mut aq_server::profiled_execution::config::ProfiledConfig),
) {
    let mut config = (*c.env.env.state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    let r = p.qualified_runtimes[0].clone();
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
    let a = ArtifactRef {
        id: Text::new(POLICY_ID).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(body.as_bytes()),
    };
    p.definition_bodies
        .push(Definition::new(a.clone(), ArtifactKind::Definition, body).unwrap());
    let execution = serde_json::json!({
        "schema_version":1,"contract":phase::contract_artifact().unwrap(),
        "runtime":r.runtime,"runtime_qualification":r.qualification,
        "proposal_policy":a,
        "adapters":ADAPTER_SET
    });
    p.definition_bodies.push(presence_fixture::definition(
        EXECUTION_ID,
        execution.to_string(),
    ));
    extra(&mut p);
    p.revision += 1;
    p.validate().unwrap();
    config.profiled_execution = Some(std::sync::Arc::new(p));
    c.env.env.state.config = std::sync::Arc::new(config);
    policies::install(&c.env.env.state).await.unwrap();
    let format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(
        format,
        aq_server::test_support::fixture_store_state_version().to_string()
    );
    let present:bool=sqlx::query_scalar("SELECT to_regclass('profiled_v2_proposals') IS NOT NULL AND to_regclass('profiled_v2_accepts') IS NOT NULL").fetch_one(&c.env.env.pool).await.unwrap();
    assert!(
        present,
        "current ordered schema must already contain both v2 tables"
    );
}
pub async fn auth(c: &Case) -> worker_api::AuthenticatedNode {
    let mut h = axum::http::HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {}", c.env.env.credential).parse().unwrap(),
    );
    worker_api::authenticate_context(&c.env.env.state, &h)
        .await
        .unwrap()
}
pub fn setup_with_inventory(
    v2: bool,
) -> impl std::future::Future<Output = (Case, phase::Proposal, AcceptRequest)> {
    Box::pin(setup_with_inventory_and_config(v2, |_| {}))
}
/// Builds a large owner future in this helper's frame and moves it to the heap,
/// so unoptimized test poll frames keep a pointer instead of the whole future.
fn on_heap<F, Fut>(build: F) -> std::pin::Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    Box::pin(build())
}
pub async fn setup_with_inventory_and_config(
    v2: bool,
    extra: fn(&mut aq_server::profiled_execution::config::ProfiledConfig),
) -> (Case, phase::Proposal, AcceptRequest) {
    let ready = on_heap(presence_fixture::setup).await;
    on_heap(|| setup_from_presence(ready, v2, extra)).await
}
/// Actual-source fixtures supply their owner before admission; never rewrite a
/// frozen offer or binding after the shared fixture has issued it.
pub async fn setup_from_presence(
    ready: presence_fixture::Case,
    v2: bool,
    extra: fn(&mut aq_server::profiled_execution::config::ProfiledConfig),
) -> (Case, phase::Proposal, AcceptRequest) {
    setup_from_presence_observed(ready, v2, extra, None).await
}
/// `short_observation_ttl_ms` adds a second, node-scoped positive availability
/// observation with that TTL before the offer is selected. It bounds the offer
/// window below the longer operator observation, which keeps current health
/// after the original proposal expires without a registry-changing new post.
async fn setup_from_presence_observed(
    ready: presence_fixture::Case,
    v2: bool,
    extra: fn(&mut aq_server::profiled_execution::config::ProfiledConfig),
    short_observation_ttl_ms: Option<u64>,
) -> (Case, phase::Proposal, AcceptRequest) {
    let mut c = ready.c;
    on_heap(|| configure(&mut c, extra)).await;
    if v2 {
        let node = auth(&c).await;
        let receipt = on_heap(|| {
            aq_server::profiled_execution::presence::inventory::record(
                &c.env.env.state,
                &node,
                &ready.request,
            )
        })
        .await
        .unwrap();
        receipt.validate_for(&ready.request).unwrap();
        c.request.inventory_sequence = ready.request.sequence;
    }
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM intelligence_registry WHERE singleton")
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    let mut observation = c.env.manual.clone();
    observation["operation_id"] = serde_json::json!(Uuid::new_v4());
    observation["expected_revision"] = serde_json::json!(revision);
    observation["sequence"] = serde_json::json!(2);
    let admin = aq_server::build_admin_router(c.env.env.state.clone());
    let (status, body) = on_heap(|| {
        common::selection::call(
            &admin,
            "/api/intelligence/observations",
            Some(observation),
            "owner@localhost",
        )
    })
    .await;
    assert_eq!(status, 200, "{body}");
    if let Some(ttl_ms) = short_observation_ttl_ms {
        let revision: i64 =
            sqlx::query_scalar("SELECT revision FROM intelligence_registry WHERE singleton")
                .fetch_one(&c.env.env.pool)
                .await
                .unwrap();
        let mut observation = c.env.manual.clone();
        observation["operation_id"] = serde_json::json!(Uuid::new_v4());
        observation["expected_revision"] = serde_json::json!(revision);
        observation["sequence"] = serde_json::json!(3);
        observation["scope"]["node_id"] = serde_json::json!(c.env.env.node);
        observation["ttl_ms"] = serde_json::json!(ttl_ms);
        let (status, body) = on_heap(|| {
            common::selection::call(
                &admin,
                "/api/intelligence/observations",
                Some(observation),
                "owner@localhost",
            )
        })
        .await;
        assert_eq!(status, 200, "{body}");
    }
    let (status, body) = on_heap(|| native_offer::poll(&c)).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    let offer = page.offers.remove(0);
    if CONTROLLED_OWNER_CLOCK {
        // Every expensive component/configuration step precedes the genuine new
        // Poll/resolver operation. Freeze only the trusted owner clock *after*
        // that actual decision/offer returned; no old snapshot is restamped.
        let at = (c.env.env.state.clock)();
        let decision: serde_json::Value = sqlx::query_scalar(
            "SELECT d.decision FROM profiled_launches l JOIN intelligence_decisions d USING(decision_id) WHERE l.attempt_id=$1",
        ).bind(offer.offered.offer.binding.attempt_id).fetch_one(&c.env.env.pool).await.unwrap();
        let decision: aq_server::intelligence::resolver::Decision =
            serde_json::from_value(decision).unwrap();
        assert!(at.timestamp_millis() as u64 >= decision.inputs.now_ms);
        assert!(
            (at.timestamp_millis() as u64)
                < decision.outcome.resolved.as_ref().unwrap().expires_at_ms,
            "genuine newly selected offer must still be fresh at controlled-clock boundary"
        );
        let clock = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(at.timestamp_millis()));
        c.controlled_clock = Some(clock.clone());
        c.env.env.state.clock = std::sync::Arc::new(move || {
            chrono::DateTime::from_timestamp_millis(clock.load(std::sync::atomic::Ordering::SeqCst))
                .unwrap()
        });
    }
    let negotiation = phase::Negotiation {
        contract: phase::contract_artifact().unwrap(),
        features: phase::required_features(),
    };
    let q = ProposalRequest {
        identity: c.request.identity.clone(),
        binding: offer.offered.offer.binding.clone(),
        negotiation: negotiation.clone(),
    };
    let node = auth(&c).await;
    let p = on_heap(|| owner::propose(&c.env.env.state, &node, &q))
        .await
        .unwrap();
    let mut original = native_offer::acceptance(&c, &offer);
    original.reservation_remaining_ms = 1000;
    let request = AcceptRequest {
        schema_version: 2,
        negotiation,
        proposal_digest: p.digest().unwrap(),
        hold: ResourceHold {
            maximum_preparation_ms: 10000,
            reservation: original.reservation.clone(),
            resources: Resources {
                slots: 1,
                memory_bytes: offer.offered.offer.manifest.resources.memory_bytes,
                cpu_millis: offer.offered.offer.manifest.resources.cpu_millis,
                disk_bytes: 1 << 20,
            },
            source_profile: reference("synthetic-source-profile"),
            source_disk_bytes: 1 << 20,
        },
        original,
    };
    (c, p, request)
}
pub struct Prepared {
    pub c: Case,
    pub p: phase::Proposal,
    pub q: AcceptRequest,
    pub a: phase::AcceptedReceipt,
    pub prepared: phase::PrepareRequest,
}
pub fn prepared(v2: bool) -> impl std::future::Future<Output = Prepared> {
    Box::pin(prepare_with_optional_config(v2, None))
}
#[allow(dead_code)] // Selected only by the path-imported qualified terminal fixture.
pub fn prepared_with_config(
    v2: bool,
    extra: fn(&mut aq_server::profiled_execution::config::ProfiledConfig),
) -> impl std::future::Future<Output = Prepared> {
    Box::pin(prepare_with_optional_config(v2, Some(extra)))
}
/// A prepared case whose original offer is bounded by a short-lived positive
/// observation, so the proposal can expire inside live health and preparation.
#[allow(dead_code)] // Selected only by the proposal-expiry atomicity test.
pub fn prepared_with_short_observation(
    v2: bool,
    ttl_ms: u64,
) -> impl std::future::Future<Output = Prepared> {
    Box::pin(async move {
        let ready = on_heap(presence_fixture::setup).await;
        let (c, p, q) =
            on_heap(|| setup_from_presence_observed(ready, v2, |_| {}, Some(ttl_ms))).await;
        prepare_from_parts(c, p, q).await
    })
}
async fn prepare_with_optional_config(
    v2: bool,
    extra: Option<fn(&mut aq_server::profiled_execution::config::ProfiledConfig)>,
) -> Prepared {
    let (c, p, q) = match extra {
        Some(extra) => setup_with_inventory_and_config(v2, extra).await,
        None => setup_with_inventory(v2).await,
    };
    prepare_from_parts(c, p, q).await
}
#[allow(dead_code)] // Selected only by the path-imported qualified terminal fixture.
pub fn prepared_from_presence(
    ready: presence_fixture::Case,
    extra: fn(&mut aq_server::profiled_execution::config::ProfiledConfig),
) -> impl std::future::Future<Output = Prepared> {
    Box::pin(async move {
        let (c, p, q) = setup_from_presence(ready, true, extra).await;
        prepare_from_parts(c, p, q).await
    })
}
async fn prepare_from_parts(c: Case, p: phase::Proposal, q: AcceptRequest) -> Prepared {
    let node = auth(&c).await;
    let a = on_heap(|| owner::accept(&c.env.env.state, &node, &q, Uuid::new_v4()))
        .await
        .unwrap()
        .receipt;
    let mut artifacts = p
        .original
        .offered
        .artifacts
        .iter()
        .map(|d| aq_admission::transport::ArtifactReceipt {
            artifact: d.artifact.clone(),
            bytes: d.bytes,
            digest: d.artifact.digest.clone(),
        })
        .collect::<Vec<_>>();
    let manifest: Value =
        sqlx::query_scalar("SELECT manifest FROM source_bases WHERE attempt_id=$1")
            .bind(q.original.acceptance.binding.attempt_id)
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    let manifest: aq_worker_protocol::TransferManifest = serde_json::from_value(manifest).unwrap();
    for (name, digest, bytes) in [
        (
            "manifest",
            manifest.manifest_digest.as_str(),
            manifest.manifest_bytes,
        ),
        (
            "bundle",
            manifest.bundle_digest.as_deref().unwrap(),
            manifest.bundle_bytes,
        ),
    ] {
        let digest: Digest = serde_json::from_value(serde_json::json!(digest)).unwrap();
        artifacts.push(aq_admission::transport::ArtifactReceipt {
            artifact: ArtifactRef {
                id: Text::new(format!("aq.source-{name}/{}", manifest.transfer_id)).unwrap(),
                version: Text::new("1").unwrap(),
                digest: digest.clone(),
            },
            bytes,
            digest,
        });
    }

    let fetch = aq_admission::transport::MaterialFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: q.original.identity.clone(),
        binding: q.original.acceptance.binding.clone(),
        lease_id: a.lease.materials[0].lease_id,
    };
    let response = on_heap(|| {
        native_offer::post(
            &c.env.env.state,
            &c.env.env.credential,
            "/v1/worker/profiled/material",
            serde_json::to_vec(&fetch).unwrap(),
        )
    })
    .await;
    assert_eq!(response.status(), 200);
    let delivery: aq_admission::transport::MaterialDelivery = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(
                response
                    .headers()
                    .get("aq-material-delivery")
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap(),
    )
    .unwrap();
    let bytes = on_heap(|| to_bytes(response.into_body(), 65536))
        .await
        .unwrap();
    assert_eq!(Digest::of(&bytes), a.lease.materials[0].material_digest);
    let record = aq_admission::scoped_presence::PrivateInstallationRecord {
        schema_version: 1,
        operation_id: fetch.operation_id,
        preparation_digest: a.digest().unwrap(),
        worker: q.original.identity.clone(),
        kernel_boot_id: Uuid::new_v4(),
        clock_source: aq_admission::scoped_presence::ClockSource::Boottime,
        sent_ns: 1_000_000_000,
        received_ns: 1_001_000_000,
        material_synced_ns: 1_002_000_000,
        retrieval_deadline_ns: delivery
            .clock
            .client_deadline_ns(1_000_000_000, 1_001_000_000)
            .unwrap(),
        delivery,
    };
    let materials = vec![record.receipt().unwrap()];
    let prepared = phase::PrepareRequest {
        schema_version: 2,
        negotiation: q.negotiation.clone(),
        accepted_receipt_digest: a.digest().unwrap(),
        expected_preparation_revision: a.lease.revision,
        original: aq_admission::transport::Prepare {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity: q.original.identity.clone(),
            binding: q.original.acceptance.binding.clone(),
            reservation: q.original.reservation.clone(),
            guardian_id: q.original.guardian_id,
            launch_id: q.original.launch_id,
            journal_revision: q.original.journal_revision + 1,
            journal_digest: Digest::of(b"synthetic durable Prepared record"),
            artifacts,
            materials,
        },
    };

    prepared.validate_for(&a, &q, &p).unwrap();
    if let Some(clock) = &c.controlled_clock {
        // Explicit logical Prepared boundary after the actual fetch/receipt.
        // Existing acceptance/selection/health timestamps remain immutable.
        clock.fetch_add(100, std::sync::atomic::Ordering::SeqCst);
    }
    Prepared {
        c,
        p,
        q,
        a,
        prepared,
    }
}
pub async fn assert_unissued(c: &Case) {
    let row=sqlx::query("SELECT (SELECT count(*) FROM profiled_phase_execution_grants) AS grants,(SELECT count(*) FROM admission_material_receipts) AS materials,state FROM profiled_authority_hot").fetch_one(&c.env.env.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("grants"), 0);
    assert_eq!(row.get::<i64, _>("materials"), 0);
    let hot: Value = row.get("state");
    assert_eq!(hot["status"], "preparing");
    assert!(hot["grant"].is_null());
}
