//! Actual worker/HTTP/SQL acceptance; runtime qualification remains synthetic.
use super::*;
use aq_admission::{
    ArtifactKind,
    preparation_v2::{
        self as wire, AcceptRequest, AcceptedDelivery, FactKind, OwnerAuthority, PhasePolicy,
        Proposal,
    },
};
use aq_server::profiled_execution::{
    accepted_preparation::PolicyDefinition, definitions::Definition,
};
use sqlx::Row;
fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
pub(super) async fn configure(case: &mut Case, execution: bool) -> (Value, Option<ArtifactRef>) {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(
        version.parse::<u32>().unwrap(),
        aq_server::test_support::fixture_store_state_version()
    );
    assert!(
        version.parse::<u32>().unwrap() >= 30,
        "actual ordered preparation owner required"
    );
    let mut config = (*case.env.env.state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    let runtime = p.qualified_runtimes[0].clone();
    let source_profile = reference("synthetic-qualification/actual-worker-default-git-profile");
    let policy = PolicyDefinition {
        schema_version: 1,
        contract: wire::contract_artifact().unwrap(),
        runtime: runtime.runtime.clone(),
        qualification: runtime.qualification.clone(),
        phase: PhasePolicy {
            schema_version: 2,
            maximum_preparation_ms: 30000,
            maximum_execution_lease_ms: 10000,
            maximum_observation_ms: 2000,
        },
        source_profile: source_profile.clone(),
        source_disk_bytes: 21 * 1024 * 1024 * 1024 + 1024 * 1024,
        owners: FactKind::all()
            .iter()
            .map(|kind| OwnerAuthority {
                kind: *kind,
                authority: reference(&format!("synthetic-owner/{kind:?}")),
            })
            .collect(),
    };
    let body = serde_json::to_string(&policy).unwrap();
    let pin = ArtifactRef {
        id: Text::new("aq.accepted-preparation-policy/actual-worker-test").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(body.as_bytes()),
    };
    p.definition_bodies
        .push(Definition::new(pin.clone(), ArtifactKind::Definition, body).unwrap());
    let mut inventory_qualification = None;
    if execution {
        let execution_body = json!({"schema_version":1,"contract":wire::contract_artifact().unwrap(),
            "runtime":runtime.runtime,"runtime_qualification":runtime.qualification,
            "proposal_policy":pin,
            "adapters":"current_project_registry_principal_inventory_health_unissued_graph_retention_private_v1"}).to_string();
        p.definition_bodies.push(definition(
            "aq.phase-execution-qualification/actual-worker-test",
            execution_body,
        ));
        let adapter = definition(
            "aq.presence-adapter/actual-worker-fixture",
            "Synthetic server fixture adapter, not physical presence qualification".into(),
        );
        let evidence = definition(
            "aq.presence-evidence/actual-worker-fixture",
            "Synthetic server owner evidence, no physical worker presence claim".into(),
        );
        let q = aq_admission::scoped_presence::Qualification {
            schema_version:1, protocol:aq_admission::scoped_presence::Protocol::ScopedMaterialPresenceV1,
            runtime:runtime.runtime.clone(), runtime_qualification:runtime.qualification.clone(),
            adapter:adapter.artifact().clone(), adapter_qualification:evidence.artifact().clone(),
            inventory_semantics:aq_admission::scoped_presence::InventorySemantics::ProvisionableOnlyAndScopedPresenceV1,
            observation_protocol:aq_admission::scoped_presence::ObservationProtocol::GrantIssuanceBoundedBoottimeV1,
            maximum_age_ms:30_000, maximum_ttl_ms:30_000,
        };
        let q = definition(
            "aq.scoped-material-presence-qualification/actual-worker-fixture",
            serde_json::to_string(&q).unwrap(),
        );
        inventory_qualification = Some(q.artifact().clone());
        p.definition_bodies.extend([adapter, evidence, q]);
    }
    p.revision += 1;
    p.validate().unwrap();
    config.profiled_execution = Some(Arc::new(p));
    case.env.env.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&case.env.env.state)
        .await
        .unwrap();
    (
        json!({"negotiation":wire::Negotiation { contract:wire::contract_artifact().unwrap(),features:wire::required_features() },"source_profile":source_profile,"maximum_preparation_ms":30000}),
        inventory_qualification,
    )
}
pub(super) async fn verify(
    case: &Case,
    root: &std::path::Path,
    identity: &aq_admission::transport::WorkerIdentity,
    result: &Value,
) {
    let q: AcceptRequest = serde_json::from_value(result["request"].clone()).unwrap();
    let p: Proposal = serde_json::from_value(result["proposal"].clone()).unwrap();
    let initial: AcceptedDelivery =
        serde_json::from_value(result["initial_delivery"].clone()).unwrap();
    let replay: AcceptedDelivery =
        serde_json::from_value(result["replay_delivery"].clone()).unwrap();
    assert_eq!(&q.original.identity, identity);
    initial
        .validate_for(&q, &p, initial.delivery.nonce)
        .unwrap();
    replay.validate_for(&q, &p, replay.delivery.nonce).unwrap();
    assert_ne!(initial.delivery.nonce, replay.delivery.nonce);
    assert_eq!(
        serde_json::to_value(&initial.receipt).unwrap(),
        serde_json::to_value(&replay.receipt).unwrap()
    );
    let executed = result["phase"] == "granted_phase_execution";
    let history_only = result["phase"] == "phase_history_only";
    if !executed && !history_only {
        assert!(replay.delivery.clock.server_time_ms >= p.original.offered.clock.expires_at_ms);
        assert!(
            result["checked_at_ns"].as_u64().unwrap()
                > result["original_deadline_ns"].as_u64().unwrap()
        );
    }
    assert!(result["checked_at_ns"].as_u64().unwrap() < result["deadline_ns"].as_u64().unwrap());
    assert_eq!(result["first_history"], result["history"]);
    let snapshot: Value =
        serde_json::from_slice(&std::fs::read(root.join("journal/journal.json")).unwrap()).unwrap();
    assert_eq!(snapshot["state"]["format"], 10);
    assert_eq!(result["journal_format"], 10);
    let attempt =
        &snapshot["state"]["attempts"][q.original.acceptance.binding.attempt_id.to_string()];
    assert_eq!(attempt["phase_acceptance"]["request"], result["request"]);
    assert_eq!(attempt["phase_acceptance"]["proposal"], result["proposal"]);
    assert_eq!(attempt["phase_preparation"], result["history"]);
    assert_eq!(
        attempt["phase_preparation"]["delivery"],
        result["initial_delivery"]
    );
    assert_eq!(
        result["journal_revision"],
        attempt["phase_preparation"]["record_revision"]
    );
    if matches!(
        result["phase"].as_str(),
        Some("prepared_phase_inputs" | "granted_phase_execution" | "phase_history_only")
    ) {
        verify_inputs(root, result, attempt, &q, &p, &initial, &snapshot);
    } else {
        assert_eq!(snapshot["state"]["revision"], result["journal_revision"]);
        assert!(attempt["phase_prepared"].is_null());
    }
    assert!(
        attempt["phase_preparation"]["record_revision"]
            .as_u64()
            .unwrap()
            >= q.original.journal_revision + 2
    );
    assert!(
        attempt["preparation"].is_null()
            && attempt["prepared"].is_null()
            && attempt["initial_grant"].is_null()
    );
    let row = sqlx::query(
        "SELECT request,receipt,receipt_digest FROM profiled_v2_accepts WHERE attempt_id=$1",
    )
    .bind(q.original.acceptance.binding.attempt_id)
    .fetch_one(&case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<Value, _>("request"), result["request"]);
    assert_eq!(
        row.get::<Value, _>("receipt"),
        serde_json::to_value(&initial.receipt).unwrap()
    );
    assert_eq!(
        row.get::<String, _>("receipt_digest"),
        initial.receipt.digest().unwrap().as_str()
    );
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(q.original.acceptance.binding.attempt_id)
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    if executed {
        verify_execution(case, result, &q, &p, &initial, &snapshot, &hot).await;
    } else if history_only {
        super::phase_history::verify(case, result, &q, &p, &initial, &snapshot, &hot).await;
    } else {
        assert_eq!(hot["status"], "preparing");
        assert!(hot["grant"].is_null());
    }
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(q.original.acceptance.binding.attempt_id)
        .fetch_one(&case.env.env.pool)
        .await
        .unwrap()
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM profiled_execution_receipts WHERE kind IN ('accept','prepare','started')").fetch_one(&case.env.env.pool).await.unwrap(),0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_receipts")
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap(),
        if executed || history_only { 1 } else { 0 }
    );
    eprintln!(
        "bounded phase clocks: {}",
        json!({"proposal_expiry_ms":p.original.offered.clock.expires_at_ms,"accepted_issued_ms":initial.receipt.lease.issued_at_ms,"accepted_expiry_ms":initial.receipt.lease.expires_at_ms,"replay_server_ms":replay.delivery.clock.server_time_ms,"journal_format":snapshot["state"]["format"],"journal_revision":snapshot["state"]["revision"]})
    );
}

fn verify_inputs(
    root: &std::path::Path,
    result: &Value,
    attempt: &Value,
    acceptance: &AcceptRequest,
    proposal: &Proposal,
    accepted: &AcceptedDelivery,
    snapshot: &Value,
) {
    let prepare: wire::PrepareRequest =
        serde_json::from_value(result["prepare_request"].clone()).unwrap();
    prepare
        .validate_for(&accepted.receipt, acceptance, proposal)
        .unwrap();
    assert_eq!(
        attempt["phase_prepared"]["request"],
        result["prepare_request"]
    );
    assert_eq!(
        result["prepared_revision"].as_u64().unwrap(),
        prepare.original.journal_revision.checked_add(1).unwrap()
    );
    if result["phase"] == "granted_phase_execution" {
        assert!(
            snapshot["state"]["revision"].as_u64().unwrap()
                > result["prepared_revision"].as_u64().unwrap()
        );
    } else {
        assert_eq!(snapshot["state"]["revision"], result["prepared_revision"]);
    }
    assert!(
        result["prepared_revision"].as_u64().unwrap()
            > result["journal_revision"].as_u64().unwrap()
    );
    if result["phase"] == "prepared_phase_inputs" {
        assert!(
            result["prepared_at_ns"].as_u64().unwrap()
                > result["original_deadline_ns"].as_u64().unwrap()
        );
    }
    assert!(result["prepared_at_ns"].as_u64().unwrap() < result["deadline_ns"].as_u64().unwrap());
    assert_eq!(
        serde_json::to_value(&prepare.original.materials).unwrap(),
        result["material_receipts"]
    );
    assert_eq!(
        prepare.original.artifacts.len(),
        proposal.original.offered.artifacts.len() + 2
    );
    let workspace = std::path::PathBuf::from(result["workspace_path"].as_str().unwrap());
    assert!(workspace.starts_with(root.join("git/workspaces")));
    assert_eq!(
        std::fs::read(workspace.join("source")).unwrap(),
        b"actual source bytes\n"
    );
    assert!(
        attempt["downloads"]
            .as_object()
            .unwrap()
            .values()
            .all(|d| !d["receipt"].is_null())
    );
    assert!(attempt["source"]["verified"].is_object());
    eprintln!(
        "bounded phase inputs: {}",
        json!({"artifacts":prepare.original.artifacts.len(),"materials":prepare.original.materials.len(),"prepared_revision":result["prepared_revision"],"prepared_at_ns":result["prepared_at_ns"],"original_deadline_ns":result["original_deadline_ns"],"deadline_ns":result["deadline_ns"]})
    );
}

fn definition(id: &str, body: String) -> Definition {
    Definition::new(
        ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(body.as_bytes()),
        },
        ArtifactKind::Definition,
        body,
    )
    .unwrap()
}
pub(super) async fn record_current_inventory(
    case: &Case,
    identity: &aq_admission::transport::WorkerIdentity,
    qualification: &ArtifactRef,
) {
    let config = case
        .env
        .env
        .state
        .config
        .profiled_execution
        .as_ref()
        .unwrap();
    let runtime = &config.qualified_runtimes[0];
    let q = aq_admission::scoped_presence::InventoryV2 {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: identity.clone(),
        runtime: runtime.runtime.clone(),
        runtime_qualification: runtime.qualification.clone(),
        negotiation: aq_admission::scoped_presence::Negotiation {
            protocol: aq_admission::scoped_presence::Protocol::ScopedMaterialPresenceV1,
            qualification: qualification.clone(),
        },
        sequence: 1,
        ttl_ms: 30_000,
        qualifications: case
            .env
            .input
            .node
            .harnesses
            .iter()
            .map(|h| h.qualification.clone())
            .collect(),
        provisionable_only: vec![],
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", case.env.env.credential)
            .parse()
            .unwrap(),
    );
    let auth = aq_server::worker_api::authenticate_context(&case.env.env.state, &headers)
        .await
        .unwrap();
    let receipt =
        aq_server::profiled_execution::presence::inventory::record(&case.env.env.state, &auth, &q)
            .await
            .unwrap();
    receipt.validate_for(&q).unwrap();
    let snapshot: Value =
        sqlx::query_scalar("SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1")
            .bind(identity.node_id)
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(snapshot["target"]["boot_id"], json!(identity.boot_id));
    assert!(
        !snapshot["credential_bindings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        snapshot["credential_bindings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["available"] == false && !b["preparation"].is_null())
    );
}
pub(super) async fn record_operator_availability(case: &Case) {
    record_operator_availability_sequence(case, 2).await;
}
pub(super) async fn record_operator_availability_sequence(case: &Case, sequence: u64) -> Value {
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM intelligence_registry WHERE singleton")
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    let mut observation = case.env.manual.clone();
    observation["operation_id"] = json!(Uuid::new_v4());
    observation["expected_revision"] = json!(revision);
    observation["sequence"] = json!(sequence);
    let (status, body) = crate::common::selection::call(
        &aq_server::build_admin_router(case.env.env.state.clone()),
        "/api/intelligence/observations",
        Some(observation),
        "owner@localhost",
    )
    .await;
    assert!(status == 200, "synthetic operator assessment refused");
    body
}
/// Test-only wrapping of the actual issuer. No daemon production route change.
pub(super) fn execution_routes(state: aq_server::AppState) -> axum::Router {
    axum::Router::new()
        .route(
            "/v1/worker/profiled-v2/prepare",
            axum::routing::post(execution_request),
        )
        .layer(axum::extract::DefaultBodyLimit::max(wire::MAX_BYTES))
        .layer(axum::middleware::map_response(
            |mut response: axum::response::Response| async move {
                response.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    "no-store".parse().unwrap(),
                );
                response
            },
        ))
        .with_state(state)
}
async fn execution_request(
    axum::extract::State(state): axum::extract::State<aq_server::AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<axum::Json<wire::Document>, aq_server::error::AppError> {
    use aq_server::error::AppError;
    let auth = aq_server::worker_api::authenticate_context(&state, &headers).await?;
    let nonce = headers
        .get("aq-delivery-nonce")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|u| !u.is_nil())
        .ok_or_else(|| AppError::BadRequest("phase delivery nonce required".into()))?;
    let wire::Document::Prepare(q) = wire::decode(&body)
        .map_err(|_| AppError::BadRequest("invalid phase Prepare document".into()))?
    else {
        return Err(AppError::BadRequest(
            "phase Prepare document required".into(),
        ));
    };
    Ok(axum::Json(wire::Document::ExecutionDelivery(Box::new(
        aq_server::profiled_execution::phase_execution::prepare(&state, &auth, &q, nonce).await?,
    ))))
}
async fn verify_execution(
    case: &Case,
    result: &Value,
    acceptance: &AcceptRequest,
    proposal: &Proposal,
    accepted: &AcceptedDelivery,
    snapshot: &Value,
    hot: &Value,
) {
    let attempt = &snapshot["state"]["attempts"][acceptance
        .original
        .acceptance
        .binding
        .attempt_id
        .to_string()];
    let prepare: wire::PrepareRequest =
        serde_json::from_value(result["prepare_request"].clone()).unwrap();
    let delivery: wire::ExecutionDelivery =
        serde_json::from_value(result["execution_delivery"].clone()).unwrap();
    delivery
        .validate_for(
            &prepare,
            &accepted.receipt,
            acceptance,
            proposal,
            delivery.delivery.nonce,
        )
        .unwrap();
    let inventory_sequence = delivery
        .receipt
        .confirmation
        .facts
        .iter()
        .find_map(|fact| {
            if let wire::FactValue::Inventory {
                sequence,
                credential_semantics,
                ..
            } = &fact.body.value
            {
                assert!(matches!(
                    credential_semantics,
                    wire::GlobalCredentialSemantics::ProvisionableOnly
                ));
                Some(*sequence)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        inventory_sequence, 1,
        "actual first V2 operation for this new worker boot"
    );
    let record = &attempt["phase_execution"];
    assert_eq!(record["delivery"], result["execution_delivery"]);
    assert_eq!(record["request_digest"], json!(prepare.digest().unwrap()));
    assert_eq!(
        record["accepted_digest"],
        json!(accepted.receipt.digest().unwrap())
    );
    let mut canonical = record.clone();
    canonical.sort_all_objects();
    assert_eq!(
        result["execution_record_digest"],
        json!(Digest::of(&serde_json::to_vec(&canonical).unwrap()))
    );
    assert_eq!(record["record_revision"], result["execution_revision"]);
    if result.get("host_started").is_some() {
        assert_eq!(
            &snapshot["state"]["revision"],
            result
                .get("terminal_ack_revision")
                .unwrap_or(&result["host_revision"])
        );
        assert!(
            result["host_revision"].as_u64().unwrap()
                > result["execution_revision"].as_u64().unwrap()
        );
    } else {
        assert_eq!(snapshot["state"]["revision"], result["execution_revision"]);
    }
    assert!(
        result["execution_revision"].as_u64().unwrap()
            > result["prepared_revision"].as_u64().unwrap()
    );
    assert!(
        result["execution_checked_at_ns"].as_u64().unwrap()
            >= record["observed_before_write_ns"].as_u64().unwrap()
    );
    for (output, stored) in [
        ("execution_deadline_ns", "deadline_ns"),
        ("execution_hard_deadline_ns", "hard_deadline_ns"),
    ] {
        assert!(result[output].as_u64().unwrap() <= record[stored].as_u64().unwrap());
        assert!(
            result["execution_checked_at_ns"].as_u64().unwrap() < result[output].as_u64().unwrap()
        );
    }
    assert!(
        result["execution_deadline_ns"].as_u64().unwrap()
            <= result["execution_hard_deadline_ns"].as_u64().unwrap()
    );
    assert_eq!(
        hot["status"],
        if result.get("phase_stopped_ack").is_some() {
            "finished"
        } else {
            "granted"
        }
    );
    assert_eq!(
        hot["grant"],
        serde_json::to_value(&delivery.receipt.grant).unwrap()
    );
    assert_eq!(
        hot["execution_deadline_ms"],
        json!(delivery.receipt.execution_deadline_ms)
    );
    let row=sqlx::query("SELECT request_bytes,receipt_bytes FROM profiled_phase_execution_grants WHERE attempt_id=$1")
        .bind(prepare.original.binding.attempt_id).fetch_one(&case.env.env.pool).await.unwrap();
    let stored_request: wire::PrepareRequest =
        serde_json::from_slice(&row.get::<Vec<u8>, _>("request_bytes")).unwrap();
    let stored_receipt: wire::ExecutionReceipt =
        serde_json::from_slice(&row.get::<Vec<u8>, _>("receipt_bytes")).unwrap();
    assert_eq!(stored_request.digest().unwrap(), prepare.digest().unwrap());
    assert_eq!(
        stored_receipt.digest().unwrap(),
        delivery.receipt.digest().unwrap()
    );
    let installation: Value =
        sqlx::query_scalar("SELECT receipt FROM admission_material_receipts WHERE lease_id=$1")
            .bind(prepare.original.materials[0].lease_id)
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(
        installation,
        serde_json::to_value(&prepare.original.materials[0]).unwrap()
    );
    eprintln!(
        "bounded phase execution: {}",
        json!({"issued_at_ms":delivery.receipt.issued_at_ms,"grant_expires_at_ms":delivery.receipt.grant.expires_at_ms,"hard_deadline_ms":delivery.receipt.execution_deadline_ms,"execution_revision":result["execution_revision"],"checked_at_ns":result["execution_checked_at_ns"],"execution_deadline_ns":result["execution_deadline_ns"],"hard_deadline_ns":result["execution_hard_deadline_ns"]})
    );
}

pub(super) async fn diagnostics(case: &Case) {
    let availability:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('sequence',sequence,'received_at_ms',observation->'received_at_ms','expires_at_ms',observation->'expires_at_ms','state',observation->'state') FROM intelligence_observations ORDER BY (observation->>'received_at_ms')::BIGINT DESC LIMIT 5")
        .fetch_all(&case.env.env.pool).await.unwrap();
    let issued:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('operation_id',operation_id,'grant_id',grant_id,'issued_at_ms',issued_at_ms,'expires_at_ms',expires_at_ms,'execution_deadline_ms',execution_deadline_ms,'graph',convert_from(provenance_bytes,'UTF8')::JSONB->'graph','private_started_at_ms',convert_from(provenance_bytes,'UTF8')::JSONB->'private_started_at_ms','private_received_at_ms',convert_from(provenance_bytes,'UTF8')::JSONB->'private_received_at_ms') FROM profiled_phase_execution_grants WHERE node_id=$1 ORDER BY operation_id LIMIT 2")
        .bind(case.env.env.node).fetch_all(&case.env.env.pool).await.unwrap();
    // Numeric original clocks only. Never log proposal bodies, source paths,
    // credential/account references, contexts or arbitrary provider messages.
    let mut clocks: Vec<Value> = sqlx::query_scalar(r#"
        SELECT jsonb_build_object(
          'proposal_published_at_ms',p.published_at_ms,
          'offer_server_time_ms',p.proposal#>'{original,offered,clock,server_time_ms}',
          'offer_expires_at_ms',p.proposal#>'{original,offered,clock,expires_at_ms}',
          'maximum_observation_ms',p.proposal#>'{policy,maximum_observation_ms}',
          'selection_now_ms',d.decision#>'{inputs,now_ms}',
          'budget_observed_at_ms',d.decision#>'{inputs,budget,observed_at_ms}',
          'budget_expires_at_ms',d.decision#>'{inputs,budget,expires_at_ms}',
          'project_expires_at_ms',d.decision#>'{inputs,project,expires_at_ms}',
          'node_observed_at_ms',d.decision#>'{inputs,node,observed_at_ms}',
          'node_expires_at_ms',d.decision#>'{inputs,node,expires_at_ms}',
          'selection_expires_at_ms',d.decision#>'{outcome,resolved,expires_at_ms}',
          'source_observed_at_ms',l.execution_spec#>'{source,observed_at_ms}',
          'source_expires_at_ms',l.execution_spec#>'{source,expires_at_ms}',
          'ready_requested_at_ms',l.graph_task_observation#>'{read_provenance,ready_requested_at_ms}',
          'ready_received_at_ms',l.graph_task_observation#>'{read_provenance,ready_received_at_ms}',
          'ready_expires_at_ms',l.graph_task_observation#>'{read_provenance,ready_expires_at_ms}',
          'context_requested_at_ms',l.graph_task_observation#>'{read_provenance,context_requested_at_ms}',
          'context_received_at_ms',l.graph_task_observation#>'{read_provenance,context_received_at_ms}',
          'context_expires_at_ms',l.graph_task_observation#>'{read_provenance,context_expires_at_ms}')
        FROM profiled_v2_proposals p
        JOIN profiled_launches l USING(attempt_id)
        JOIN intelligence_decisions d ON d.decision_id=l.decision_id
        WHERE p.node_id=$1 ORDER BY p.published_at_ms DESC LIMIT 2
    "#).bind(case.env.env.node).fetch_all(&case.env.env.pool).await.unwrap();
    for record in &mut clocks {
        if let Some(fields) = record.as_object_mut() {
            for value in fields.values_mut() {
                if value.as_u64().is_none() {
                    *value = Value::Null;
                }
            }
        } else {
            *record = Value::Null;
        }
    }
    eprintln!("bounded original offer clocks: {}", json!(clocks));
    eprintln!(
        "bounded phase failure clocks (historical, no execution permission): {}",
        json!({"server_now_ms":chrono::Utc::now().timestamp_millis(),"availability":availability,"issued":issued})
    );
}
