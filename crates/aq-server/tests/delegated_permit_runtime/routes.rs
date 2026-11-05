//! HTTP routing qualification using decoded native bodies and an expired permit.
use super::*;
use serde_json::{Value, json};

pub(super) async fn exercise(
    f: &phase_refresh_fixture::Prepared,
    upgraded: &PermitUpgradeResponse,
    started: &aq_phase_terminal_protocol::refresh_v1::StartedRequest,
    revision: u64,
) {
    use aq_accounting::wire::*;
    use aq_admission::transport::*;
    let p = &upgraded.permit;
    let state = &f.c.env.env.state;
    let identity = WorkerIdentity {
        node_id: p.binding.node.node_id,
        boot_id: p.binding.node.boot_id,
    };
    let binding = started.original.binding.clone();
    let stopped = phase_refresh_fixture::stop_request(started);
    let material = MaterialFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: identity.clone(),
        binding: binding.clone(),
        lease_id: Uuid::new_v4(),
    };
    let artifact = ArtifactFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: identity.clone(),
        binding: binding.clone(),
        artifact: started.original.runtime.clone(),
        offset: 0,
        length: 1,
    };
    TransportDocument::ArtifactFetch(artifact.clone())
        .validate()
        .unwrap();
    TransportDocument::MaterialFetch(material.clone())
        .validate()
        .unwrap();
    let report = ResultReport {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        report_id: Uuid::new_v4(),
        identity: identity.clone(),
        binding: binding.clone(),
        supersedes: None,
        outcome: ProcessOutcome::Success,
        stop: stopped.original.evidence.clone(),
        source_receipt_ids: vec![],
        pending_effect_ids: vec![],
        conclusion: aq_admission::Label::new("route refusal fixture").unwrap(),
    };
    let event = ObservationRecord::seal(
        Uuid::new_v4(),
        Observation {
            binding: Binding {
                project_id: Uuid::new_v4(),
                task_id: Uuid::new_v4(),
                lineage_id: Uuid::new_v4(),
                attempt_id: binding.attempt_id,
                attempt_generation: binding.assignment_generation,
                session: binding.session.clone(),
            },
            provenance: Provenance {
                source_id: Uuid::new_v4(),
                source_generation: 1,
                source_kind: SourceKind::NativeTranscript,
                native_session_key: Text::new("fixture").unwrap(),
                cursor: SourceCursor::Record {
                    id: Text::new("record").unwrap(),
                },
                parser: started.original.runtime.clone(),
                raw_record: RawRecordEvidence::Available {
                    digest: Digest::of(b"source"),
                    byte_length: 6,
                },
            },
            sequence: 1,
            observed_at_ms: p.issued_at_ms,
            kind: ObservationKind::TurnBoundary { in_turn: false },
        },
    )
    .unwrap();
    let packet = Packet::seal(
        Uuid::new_v4(),
        Message::Batch(ObservationBatch {
            node_id: identity.node_id,
            boot_id: identity.boot_id,
            first_sequence: 1,
            last_sequence: 1,
            events: vec![event],
        }),
    )
    .unwrap();
    let peer = json!({"kind":"remote","node_id":identity.node_id,"boot_id":identity.boot_id});
    let capabilities = json!([{"id":"aq.terminal-relay","version":1}]);
    let terminal = json!({"session":binding.session,"terminal_id":Uuid::new_v4(),"epoch":1});
    let poll = json!({"version":1,"capability":"aq.terminal-relay","peer":peer,"cursor":0,"hold_ms":0,"active_subscriptions":0,"active_controllers":0,"advertised_capabilities":capabilities});
    let upload = json!({"version":1,"capability":"aq.terminal-relay","peer":peer,"exchange_id":Uuid::new_v4(),"cursor":0,"receipts":[],"epoch_receipts":[],"resize_receipts":[],"fence_acks":[],"frames":[],"snapshots":[],"gaps":[]});
    let register = json!({"version":1,"capability":"aq.terminal-relay","operation_id":Uuid::new_v4(),"peer":peer,"terminal":terminal,"expected_authority_revision":1,"input_mode":"finite_v1","advertised_capabilities":capabilities});
    let attempt = json!({"attempt_id":binding.attempt_id,"session_id":Uuid::new_v4(),"generation":binding.assignment_generation});
    let bodies: Vec<(&str, Value)> = vec![
        ("/v1/worker/profiled/material", json!(material)),
        ("/v1/worker/profiled/artifact", json!(artifact)),
        (
            "/v1/worker/profiled-v2/prepare",
            json!(aq_admission::preparation_refresh_v1::Document::Prepare(
                Box::new(f.prepared.clone())
            )),
        ),
        ("/v1/worker/profiled/started", json!(started.original)),
        ("/v1/worker/profiled-v2/started", json!(started)),
        ("/v1/worker/profiled-v2/stopped", json!(stopped)),
        ("/v1/worker/profiled/results", json!(report)),
        (
            "/v1/worker/profiled/renew",
            json!(SandboxDocument::RunnerRenewalRequest(
                RunnerRenewalRequest {
                    schema_version: 1,
                    operation_id: Uuid::new_v4(),
                    authority: upgraded.authority.clone(),
                    permit: p.clone(),
                    revision
                }
            )),
        ),
        ("/v1/worker/observations/batch", json!(packet)),
        ("/v1/terminal-relay/register", register),
        ("/v1/terminal-relay/poll", poll.clone()),
        ("/v1/terminal-relay/output-poll", poll),
        ("/v1/terminal-relay/upload", upload.clone()),
        ("/v1/terminal-relay/control-upload", upload),
        (
            "/v2/worker/messages/poll",
            json!({"identity":identity,"attempt":attempt,"limit":1}),
        ),
        (
            "/v2/worker/messages/ack",
            json!({"identity":identity,"attempt":attempt,"operation_id":Uuid::new_v4(),"delivery_id":Uuid::new_v4(),"disposition":"received_by_node"}),
        ),
    ];
    let mut h = headers(p);
    h.insert("aq-phase-terminal-api-version", "1".parse().unwrap());
    h.insert("aq-source-api-version", "1".parse().unwrap());
    f.c.controlled_clock
        .as_ref()
        .unwrap()
        .store(p.expires_at_ms as i64, std::sync::atomic::Ordering::SeqCst);
    for (path, body) in bodies {
        let response = request(state, path, &h, body).await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 131072)
            .await
            .unwrap();
        let expected = if path.starts_with("/v2/worker/messages/") {
            401
        } else {
            403
        };
        assert_eq!(
            status,
            expected,
            "{path}: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    use tower::ServiceExt;
    for suffix in [
        "".to_string(),
        "/manifest?offset=0&limit=1".into(),
        "/bundle?offset=0&limit=1".into(),
    ] {
        let path = format!("/v1/worker/profiled/source/{}{suffix}", binding.attempt_id);
        let mut request = axum::http::Request::builder().uri(&path);
        for (key, value) in &h {
            request = request.header(key, value);
        }
        let response = aq_server::build_worker_router(state.clone())
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{path}");
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_result_reports WHERE attempt_id=$1")
            .bind(binding.attempt_id)
            .fetch_one(&f.c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "refused result has no effect");
}
