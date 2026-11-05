//! Actual current-store/HTTP owners with explicitly synthetic host proof DTOs.
//! No physical worker, kernel stop, model/provider or production eligibility claim.
mod common;
#[allow(dead_code)]
#[path = "common/phase_refresh_terminal.rs"]
mod terminal;
use aq_admission::{Digest, preparation_refresh_v1 as phase};
use aq_phase_terminal_protocol::refresh_v1 as wire;
use aq_server::profiled_execution::{phase_refresh_terminal as owner, policies};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;
use uuid::Uuid;

#[allow(dead_code)]
#[path = "common/phase_frozen_dispatch.rs"]
mod frozen_fixture;

fn refresh_only_terminal_for_frozen_case(
    config: &mut aq_server::profiled_execution::config::ProfiledConfig,
) {
    use aq_admission::{ArtifactKind, ArtifactRef, Text};
    use aq_server::profiled_execution::{
        accepted_refresh, definitions::Definition, phase_refresh_execution, phase_refresh_terminal,
    };
    fn reference(id: &str) -> ArtifactRef {
        ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(id.as_bytes()),
        }
    }
    fn publish(
        config: &mut aq_server::profiled_execution::config::ProfiledConfig,
        id: &str,
        body: String,
    ) -> ArtifactRef {
        let artifact = ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(body.as_bytes()),
        };
        config
            .definition_bodies
            .push(Definition::new(artifact.clone(), ArtifactKind::Definition, body).unwrap());
        artifact
    }
    let runtime = config.qualified_runtimes[0].clone();
    // Genuine independent operator definitions, not conversion of a retained
    // old/new proposal, accepted receipt, execution receipt or live permission.
    let policy = accepted_refresh::PolicyDefinition {
        schema_version: 1,
        contract: phase::contract_artifact().unwrap(),
        runtime: runtime.runtime.clone(),
        qualification: runtime.qualification.clone(),
        phase: phase::PhasePolicy {
            schema_version: 2,
            maximum_preparation_ms: 10000,
            maximum_execution_lease_ms: 10000,
            maximum_observation_ms: 2000,
        },
        source_profile: reference("synthetic-source-profile"),
        source_disk_bytes: 1 << 20,
        owners: phase::FactKind::all()
            .iter()
            .map(|kind| phase::OwnerAuthority {
                kind: *kind,
                authority: reference(&format!("synthetic-owner/{kind:?}")),
            })
            .collect(),
    };
    let policy = publish(
        config,
        "aq.accepted-preparation-refresh-policy/mixed-test",
        serde_json::to_string(&policy).unwrap(),
    );
    let execution = phase_refresh_execution::ExecutionQualification {
        schema_version:1,contract:phase::contract_artifact().unwrap(), runtime:runtime.runtime.clone(), runtime_qualification:runtime.qualification.clone(),
        proposal_policy:policy,adapters:phase_refresh_execution::AdapterSet::RegistryObservationContinuityCurrentProjectPrincipalInventoryHealthUnissuedGraphRetentionPrivateV1,
    };
    publish(
        config,
        "aq.phase-refresh-execution-qualification/mixed-test",
        serde_json::to_string(&execution).unwrap(),
    );
    let proof = publish(
        config,
        "aq.synthetic-phase-terminal-proof/mixed-test",
        "Explicit synthetic proof definition, no physical qualification".into(),
    );
    let terminal = phase_refresh_terminal::qualification::TerminalQualification {
        schema_version: 1,
        contract: wire::contract_artifact().unwrap(),
        runtime: runtime.runtime,
        runtime_qualification: runtime.qualification,
        proof_producer: proof,
    };
    publish(
        config,
        "aq.phase-terminal-refresh-qualification/mixed-test",
        serde_json::to_string(&terminal).unwrap(),
    );
}

async fn post(
    f: &terminal::Prepared,
    endpoint: &str,
    bytes: Vec<u8>,
    nonce: Option<Uuid>,
) -> (StatusCode, Vec<u8>) {
    let mut request = Request::post(format!("/v1/worker/profiled-v2/{endpoint}"))
        .header(
            "authorization",
            format!("Bearer {}", f.c.env.env.credential),
        )
        .header("content-type", "application/json");
    if let Some(nonce) = nonce {
        request = request.header("aq-delivery-nonce", nonce.to_string());
    } else {
        request = request.header("aq-phase-terminal-api-version", "1");
    }
    let response = aq_server::build_worker_router(f.c.env.env.state.clone())
        .oneshot(request.body(Body::from(bytes)).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    if nonce.is_none() {
        assert_eq!(response.headers()["aq-phase-terminal-api-version"], "1");
    }
    (
        response.status(),
        to_bytes(response.into_body(), 131072)
            .await
            .unwrap()
            .to_vec(),
    )
}
async fn snapshot(f: &terminal::Prepared) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('hot',(SELECT to_jsonb(h) FROM profiled_authority_hot h),'journal',(SELECT to_jsonb(j) FROM profiled_owner_journals j),'grants',(SELECT count(*) FROM profiled_phase_execution_grants),'started',(SELECT count(*) FROM profiled_started_receipts),'stopped',(SELECT count(*) FROM profiled_stopped_receipts),'closures',(SELECT count(*) FROM profiled_launch_closures),'holds',(SELECT holds_task FROM execution_attempts LIMIT 1))")
        .fetch_one(&f.c.env.env.pool).await.unwrap()
}
fn history_request(f: &terminal::Prepared) -> wire::HistoryRequest {
    wire::HistoryRequest {
        schema_version: 1,
        negotiation: wire::Negotiation::current().unwrap(),
        operation_id: Uuid::new_v4(),
        binding: f.prepared.original.binding.clone(),
        accepted_receipt_digest: f.a.digest().unwrap(),
        prepare_operation_id: f.prepared.original.operation_id,
    }
}

#[tokio::test]
async fn exact_refresh_prepare_http_uses_actual_current_store_and_its_own_qualification() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let before = snapshot(&f).await;
    let mut wrong = json!(phase::Document::Prepare(Box::new(f.prepared.clone())));
    // Malicious profile substitution in untrusted bytes, not an old owner DTO.
    wrong["body"]["negotiation"] = json!({
        "contract":aq_admission::preparation_v2::contract_artifact().unwrap(),
        "features":aq_admission::preparation_v2::required_features(),
    });
    let (status, _) = post(
        &f,
        "prepare",
        serde_json::to_vec(&wrong).unwrap(),
        Some(Uuid::new_v4()),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(snapshot(&f).await, before);
    let nonce = Uuid::new_v4();
    let (status, bytes) = post(
        &f,
        "prepare",
        serde_json::to_vec(&phase::Document::Prepare(Box::new(f.prepared.clone()))).unwrap(),
        Some(nonce),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let phase::Document::ExecutionDelivery(reply) = phase::decode(&bytes).unwrap() else {
        panic!("expected nominal refresh execution delivery")
    };
    reply
        .validate_for(&f.prepared, &f.a, &f.q, &f.p, nonce)
        .unwrap();
    let (format, capacity): (String,i64) = sqlx::query_as("SELECT (SELECT value FROM meta WHERE key='state_format_version'),(SELECT count(*) FROM profiled_phase_terminal_capacity)")
        .fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(format, aq_server::STATE_FORMAT_VERSION.to_string());
    assert_eq!(capacity, 1);
    assert_eq!(snapshot(&f).await["grants"], 1);
}

#[tokio::test]
async fn actual_refresh_started_stopped_http_records_exact_new_ack_envelopes_and_retains_task() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let start = terminal::started_request(&f, &execution);
    let bytes = serde_json::to_vec(&start).unwrap();
    let (status, encoded) = post(&f, "started", bytes.clone(), None).await;
    assert_eq!(status, StatusCode::OK);
    let ack = wire::decode_started_receipt(&encoded, &start).unwrap();
    let after_start = snapshot(&f).await;
    let (status, replay) = post(&f, "started", bytes, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, encoded);
    assert_eq!(snapshot(&f).await, after_start);
    let stop = terminal::stop_request(&start);
    stop.validate_for(
        &terminal::context(&f, &execution, &terminal::checkpoint(&f)),
        &start,
    )
    .unwrap();
    let bytes = serde_json::to_vec(&stop).unwrap();
    let (status, encoded_stop) = post(&f, "stopped", bytes.clone(), None).await;
    assert_eq!(status, StatusCode::OK);
    let stopped = wire::decode_stopped_receipt(&encoded_stop, &stop).unwrap();
    let after_stop = snapshot(&f).await;
    let (status, replay) = post(&f, "stopped", bytes, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, encoded_stop);
    assert_eq!(snapshot(&f).await, after_stop);
    assert_eq!(after_stop["hot"]["state"]["stop"]["kind"], "stopped");
    assert_eq!(after_stop["holds"], true);
    let row = sqlx::query("SELECT initial_authority_kind,initial_authority_receipt_digest,phase_envelope_bytes,phase_ack_bytes FROM profiled_started_receipts")
        .fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(
        row.get::<String, _>("initial_authority_kind"),
        "phase_execution_v2"
    );
    assert_eq!(
        row.get::<String, _>("initial_authority_receipt_digest"),
        execution.digest().unwrap().as_str()
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&row.get::<Vec<u8>, _>("phase_envelope_bytes")).unwrap(),
        json!(start)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&row.get::<Vec<u8>, _>("phase_ack_bytes")).unwrap(),
        json!(ack)
    );
    assert_eq!(stopped.origin, ack.origin);
    assert!(
        aq_server::profiled_execution::stopped::record(
            &f.c.env.env.state,
            &terminal::auth(&f).await,
            &stop.original
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn new_history_lookup_survives_expiry_without_minting_local_or_delivery_authority() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let node = terminal::auth(&f).await;
    let q = history_request(&f);
    let before = snapshot(&f).await;
    let absent = owner::history::read(&f.c.env.env.state, &node, &q)
        .await
        .unwrap();
    assert!(absent.identity.is_none());
    assert_eq!(snapshot(&f).await, before);
    let execution = terminal::issue(&f).await;
    let before = snapshot(&f).await;
    let (status, bytes) = post(&f, "history", serde_json::to_vec(&q).unwrap(), None).await;
    assert_eq!(status, StatusCode::OK);
    let known = wire::decode_history_receipt(&bytes, &q).unwrap();
    assert_eq!(
        known
            .identity
            .as_ref()
            .unwrap()
            .origin
            .execution_receipt_digest,
        execution.digest().unwrap()
    );
    f.c.controlled_clock.as_ref().unwrap().store(
        execution.execution_deadline_ms as i64 + 1,
        std::sync::atomic::Ordering::SeqCst,
    );
    let historical = owner::history::read(&f.c.env.env.state, &node, &q)
        .await
        .unwrap();
    assert_eq!(json!(historical.identity), json!(known.identity));
    assert_eq!(snapshot(&f).await, before);
    let encoded = json!(historical);
    assert!(encoded.get("delivery").is_none() && encoded.get("clock").is_none());
}

#[tokio::test]
async fn duplicate_unknown_and_cross_profile_terminal_requests_never_mutate_authority() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let q = terminal::started_request(&f, &execution);
    let before = snapshot(&f).await;
    let original = serde_json::to_string(&q).unwrap();
    let duplicate = original.replacen(
        '{',
        &format!("{{\"negotiation\":{},", json!(q.negotiation)),
        1,
    );
    let mut unknown = json!(q);
    unknown["negotiation"]["contract"]["digest"] = json!(Digest::of(b"unknown terminal contract"));
    let mut mixed = json!(q);
    mixed["negotiation"]["features"] = json!(aq_phase_terminal_protocol::required_features());
    let mut extra = json!(q);
    extra["fake_runtime_eligibility"] = json!(true);
    for bytes in [
        duplicate.into_bytes(),
        serde_json::to_vec(&unknown).unwrap(),
        serde_json::to_vec(&mixed).unwrap(),
        serde_json::to_vec(&extra).unwrap(),
    ] {
        let (status, _) = post(&f, "started", bytes, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(snapshot(&f).await, before);
    }
}

#[tokio::test]
async fn expired_retired_runtime_still_cleans_up_with_exact_immutable_refresh_ack() {
    let mut f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let node = terminal::auth(&f).await;
    let start = terminal::started_request(&f, &execution);
    owner::started::record(&f.c.env.env.state, &node, &start)
        .await
        .unwrap();
    let stop = terminal::stop_request(&start);
    f.c.controlled_clock.as_ref().unwrap().store(
        execution.grant.expires_at_ms as i64 + 100,
        std::sync::atomic::Ordering::SeqCst,
    );
    let mut config = (*f.c.env.env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    profiled.revision += 1;
    profiled.qualified_runtimes.clear();
    profiled.context_qualifications.clear();
    profiled.definition_bodies.retain(|d| {
        !d.artifact()
            .id
            .as_str()
            .starts_with("aq.accepted-preparation-refresh-policy/")
            && !d
                .artifact()
                .id
                .as_str()
                .starts_with("aq.phase-refresh-execution-qualification/")
    });
    config.profiled_execution = Some(std::sync::Arc::new(profiled));
    f.c.env.env.state.config = std::sync::Arc::new(config);
    policies::install(&f.c.env.env.state).await.unwrap();
    let ack = owner::stopped::record(&f.c.env.env.state, &node, &stop)
        .await
        .unwrap();
    let after = snapshot(&f).await;
    assert_eq!(
        json!(
            owner::stopped::record(&f.c.env.env.state, &node, &stop)
                .await
                .unwrap()
        ),
        json!(ack)
    );
    assert_eq!(snapshot(&f).await, after);
    assert_eq!(after["holds"], true);
}

#[tokio::test]
async fn no_started_refresh_closure_revokes_permission_retains_holds_and_replays_exact_ack() {
    use aq_admission::granted_closure as closure;
    let f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../aq-phase-terminal-protocol/artifacts/refresh-v1/fixtures.json"
    ))
    .unwrap();
    let mut q: wire::ClosureRequest =
        serde_json::from_value(fixtures["closure_requests"][0].clone()).unwrap();
    let prepared = terminal::checkpoint(&f);
    let context = terminal::context(&f, &execution, &prepared);
    q.origin = wire::Origin::for_context(&context).unwrap();
    let r = &mut q.original;
    r.operation_id = Uuid::new_v4();
    r.binding = execution.grant.binding.clone();
    r.grant_id = execution.grant.grant_id;
    r.guardian_id = f.a.lease.guardian_id;
    r.launch_id = f.a.lease.launch_id;
    r.reservation_id = f.a.lease.hold.reservation.reservation_id;
    r.runtime = context.runtime.clone();
    r.qualification = context.runtime_qualification.clone();
    r.journal = closure::JournalCheckpoint {
        revision: prepared.revision + 2,
        digest: Digest::of(b"synthetic refresh no-start terminal checkpoint"),
    };
    let closure::Evidence::Suppressed {
        record,
        record_digest,
    } = &mut r.evidence
    else {
        panic!("explicit suppression template required")
    };
    record.operation_id = r.operation_id;
    record.suppression_id = Uuid::new_v4();
    record.binding = r.binding.clone();
    record.grant_id = r.grant_id;
    record.guardian_id = r.guardian_id;
    record.launch_id = r.launch_id;
    record.reservation_id = r.reservation_id;
    record.previous = prepared.clone();
    record.terminal_revision = r.journal.revision;
    *record_digest = record.digest().unwrap();
    q.validate_for(&context).unwrap();
    let bytes = serde_json::to_vec(&q).unwrap();
    let (status, encoded) = post(&f, "launch-closure", bytes.clone(), None).await;
    assert_eq!(status, StatusCode::OK);
    wire::decode_closure_receipt(&encoded, &q).unwrap();
    let after = snapshot(&f).await;
    assert_eq!(after["hot"]["state"]["revoked"], true);
    assert_eq!(after["hot"]["state"]["stop"]["kind"], "unknown");
    assert_eq!(after["holds"], true);
    assert_eq!(after["started"], 0);
    assert_eq!(after["stopped"], 0);
    assert_eq!(after["closures"], 1);
    let (status, replay) = post(&f, "launch-closure", bytes, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, encoded);
    assert_eq!(snapshot(&f).await, after);
}

#[tokio::test]
async fn refresh_route_activation_cannot_expose_an_actual_frozen_issuer_without_its_terminal_profile()
 {
    use aq_admission::preparation_v2 as frozen;
    let f = Box::pin(frozen_fixture::prepared_with_config(
        true,
        refresh_only_terminal_for_frozen_case,
    ))
    .await;
    // Route gating and the eligible library issuer share the synthetic fixture's
    // controlled clock, including its health and Graph observations.
    let nonce = Uuid::new_v4();
    let response = aq_server::build_worker_router(f.c.env.env.state.clone())
        .oneshot(
            Request::post("/v1/worker/profiled-v2/prepare")
                .header(
                    "authorization",
                    format!("Bearer {}", f.c.env.env.credential),
                )
                .header("content-type", "application/json")
                .header("aq-delivery-nonce", nonce.to_string())
                .body(Body::from(
                    serde_json::to_vec(&frozen::Document::Prepare(Box::new(f.prepared.clone())))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_phase_execution_grants")
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    // Prove this is an actually eligible frozen library issuer, not a malformed
    // or cross-profile DTO that would fail independently of the missing gate.
    let grant = aq_server::profiled_execution::phase_execution::prepare(
        &f.c.env.env.state,
        &frozen_fixture::auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    grant
        .validate_for(&f.prepared, &f.a, &f.q, &f.p, grant.delivery.nonce)
        .unwrap();
}
