//! An explicit operator assessment of this deterministic static fixture only.
//! It is neither provider health nor a runtime automatic observation refresher.
use super::*;
use aq_admission::preparation_v2::{AcceptRequest, AcceptedDelivery, PrepareRequest, Proposal};
use std::{os::unix::fs::MetadataExt, path::Path};

fn synthetic_material(path: &Path, expected: &Digest, bytes: u64, uid: u32) {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(metadata.nlink(), 1);
    assert_eq!(metadata.uid(), uid);
    assert_eq!(metadata.len(), bytes);
    assert!(bytes > 0 && bytes <= 65536);
    let private = zeroize::Zeroizing::new(std::fs::read(path).unwrap());
    assert!(private.as_slice() == b"synthetic-not-a-provider-key");
    assert!(Digest::of(&private) == *expected);
}

pub(super) async fn assess(
    case: &Case,
    root: &Path,
    identity: &aq_admission::transport::WorkerIdentity,
    paused: &Value,
    original_host: &Value,
) {
    let started = chrono::Utc::now().timestamp_millis() as u64;
    assert_eq!(
        paused["phase"],
        "prepared_inputs_awaiting_synthetic_assessment"
    );
    let q: PrepareRequest = serde_json::from_value(paused["prepare_request"].clone()).unwrap();
    let accepted: AcceptedDelivery =
        serde_json::from_value(paused["initial_delivery"].clone()).unwrap();
    let accept: AcceptRequest = serde_json::from_value(paused["request"].clone()).unwrap();
    let proposal: Proposal = serde_json::from_value(paused["proposal"].clone()).unwrap();
    q.validate_for(&accepted.receipt, &accept, &proposal)
        .unwrap();
    assert!(q.original.identity == *identity);
    assert!(started < accepted.receipt.lease.expires_at_ms);

    // The opaque PreparedInputs owner is still held by the paused worker. Read
    // and verify its actual fsynced journal; this DTO mints no authority.
    let journal = std::fs::read(root.join("journal/journal.json")).unwrap();
    assert!(journal.len() <= 4_194_304);
    let snapshot: Value = serde_json::from_slice(&journal).unwrap();
    let mut canonical = snapshot["state"].clone();
    canonical.sort_all_objects();
    assert!(json!(Digest::of(&serde_json::to_vec(&canonical).unwrap())) == snapshot["digest"]);
    let state = &snapshot["state"];
    assert_eq!(state["format"], 10);
    assert!(state["identity"] == json!(identity));
    assert_eq!(state["revision"], paused["prepared_revision"]);
    let attempt = &state["attempts"][q.original.binding.attempt_id.to_string()];
    assert!(attempt["phase_prepared"]["request"] == json!(q));
    assert!(attempt["phase_execution"].is_null() && attempt["native_host"].is_null());
    assert!(attempt["source"]["verified"].is_object());
    assert!(
        attempt["downloads"]
            .as_object()
            .unwrap()
            .values()
            .all(|d| !d["receipt"].is_null())
    );
    let workspace = Path::new(paused["workspace_path"].as_str().unwrap());
    assert!(workspace.starts_with(root.join("git/workspaces")));
    assert!(std::fs::read(workspace.join("source")).unwrap() == b"actual source bytes\n");

    // Rehash the actual pinned deterministic program, host helper and tmux now,
    // not merely their earlier configuration. Scope stays synthetic/static.
    assert!(phase_host::configuration() == *original_host);
    let registry = case.env.env.state.config.intelligence.as_ref().unwrap();
    assert_eq!(accepted.receipt.lease.materials.len(), 1);
    let lease = &accepted.receipt.lease.materials[0];
    let configured = registry
        .config
        .credentials
        .iter()
        .find(|c| {
            c.credential.profile == lease.credential_profile
                && c.credential.revision == lease.credential_revision
        })
        .unwrap();
    assert!(configured.allowed_node_ids.contains(&identity.node_id));
    let uid = std::fs::metadata(root).unwrap().uid();
    synthetic_material(
        configured.secret_file.as_ref().unwrap(),
        &lease.material_digest,
        lease.material_bytes,
        uid,
    );
    synthetic_material(
        &root
            .join("materials")
            .join(lease.lease_id.to_string())
            .join("material.bin"),
        &lease.material_digest,
        lease.material_bytes,
        uid,
    );
    assert!(
        q.original
            .materials
            .iter()
            .any(|m| m.lease_id == lease.lease_id && m.material_digest == lease.material_digest)
    );

    let node: Value = sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1")
        .bind(identity.node_id)
        .fetch_one(&case.env.env.pool)
        .await
        .unwrap();
    assert!(node["identity"] == json!(identity));
    let metadata: aq_server::profiled_execution::registration::NativeRegistration =
        serde_json::from_value(node["native"].clone()).unwrap();
    aq_admission::transport::TransportDocument::Register(metadata.request.clone())
        .validate()
        .unwrap();
    assert!(metadata.request.identity == *identity);
    let runtime = &case
        .env
        .env
        .state
        .config
        .profiled_execution
        .as_ref()
        .unwrap()
        .qualified_runtimes[0];
    assert!(
        metadata.request.runtime == runtime.runtime
            && metadata.qualification == runtime.qualification
    );
    let inventory: Value = sqlx::query_scalar(
        "SELECT snapshot FROM intelligence_node_snapshots WHERE node_id=$1 AND boot_id=$2",
    )
    .bind(identity.node_id)
    .bind(identity.boot_id)
    .fetch_one(&case.env.env.pool)
    .await
    .unwrap();
    assert!(
        inventory["target"]
            == json!(aq_selection::ExecutionTarget::Worker {
                node_id: identity.node_id,
                boot_id: identity.boot_id
            })
    );
    assert!(inventory["harnesses"] == json!(registry.config.qualified_harnesses));
    let revoked: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM nodes WHERE id=$1")
        .bind(identity.node_id)
        .fetch_one(&case.env.env.pool)
        .await
        .unwrap();
    assert!(!revoked);

    // A new owner-authorized assessment gets its own operation/sequence/time.
    // The actual observation owner retains sequence2's immutable operation reply.
    let old: Value = sqlx::query_scalar(
        "SELECT response FROM intelligence_operations WHERE response->>'sequence'='2'",
    )
    .fetch_one(&case.env.env.pool)
    .await
    .unwrap();
    let observed = phase::record_operator_availability_sequence(case, 3).await;
    let retained: Value = sqlx::query_scalar(
        "SELECT response FROM intelligence_operations WHERE response->>'observation_id'=$1",
    )
    .bind(old["observation_id"].as_str().unwrap())
    .fetch_one(&case.env.env.pool)
    .await
    .unwrap();
    assert!(retained == old);
    assert_eq!(observed["sequence"], 3);
    assert!(observed["scope"] == old["scope"] && observed["source"] == old["source"]);
    let received = observed["received_at_ms"].as_u64().unwrap();
    assert!(received >= started && received > old["received_at_ms"].as_u64().unwrap());
    assert_eq!(
        observed["expires_at_ms"].as_u64().unwrap() - received,
        old["expires_at_ms"].as_u64().unwrap() - old["received_at_ms"].as_u64().unwrap()
    );
    eprintln!(
        "bounded new synthetic operator assessment: {}",
        json!({
            "scope":"deterministic_static_fixture_only","sequence":3,
            "checked_at_ms":started,"received_at_ms":received,
            "expires_at_ms":observed["expires_at_ms"],"prior_sequence":old["sequence"],
            "prior_received_at_ms":old["received_at_ms"],"original_preparation_expiry_ms":accepted.receipt.lease.expires_at_ms,
            "prepared_revision":paused["prepared_revision"],"material_revalidated":true,
            "peer_pins_revalidated":true,"node_identity_revalidated":true,"installed_harness_revalidated":true,
            "original_observation_retained":true,"provider_availability_claim":false
        })
    );
}
