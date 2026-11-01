//! Actual aq-git retention/ranges after real proposal expiry. Worker runtime,
//! resource journal and installation qualification remain synthetic and no host runs.
use super::*;
use aq_admission::{
    ArtifactKind, Resources,
    preparation_v2::{
        self as phase, AcceptRequest, FactKind, OwnerAuthority, PhasePolicy, ResourceHold,
    },
};
use aq_server::profiled_execution::{
    accepted_preparation::{self as owner, PolicyDefinition, ProposalRequest},
    definitions::Definition,
};
use sqlx::Row;
fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
async fn configure(case: &mut Case) {
    let mut config = (*case.env.env.state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    let runtime = &p.qualified_runtimes[0];
    let policy = PolicyDefinition {
        schema_version: 1,
        contract: phase::contract_artifact().unwrap(),
        runtime: runtime.runtime.clone(),
        qualification: runtime.qualification.clone(),
        phase: PhasePolicy {
            schema_version: 2,
            maximum_preparation_ms: 30000,
            maximum_execution_lease_ms: 10000,
            maximum_observation_ms: 2000,
        },
        source_profile: reference("synthetic-retained-reader-profile-no-hydration"),
        source_disk_bytes: 1 << 20,
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
        id: Text::new("aq.accepted-preparation-policy/actual-git-reader-test").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(body.as_bytes()),
    };
    p.definition_bodies
        .push(Definition::new(pin, ArtifactKind::Definition, body).unwrap());
    p.revision += 1;
    p.validate().unwrap();
    config.profiled_execution = Some(Arc::new(p));
    case.env.env.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&case.env.env.state)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../../migrations/0030_accepted_preparation_v2.sql"
    ))
    .execute(&case.env.env.pool)
    .await
    .unwrap();
}
async fn context(c: &Case) -> AuthenticatedNode {
    let mut h = axum::http::HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {}", c.env.env.credential).parse().unwrap(),
    );
    worker_api::authenticate_context(&c.env.env.state, &h)
        .await
        .unwrap()
}
async fn get(
    c: &Case,
    binding: &aq_admission::LaunchBinding,
    member: Option<&str>,
    offset: u64,
) -> axum::response::Response {
    let suffix = member
        .map(|m| format!("/{m}?offset={offset}&limit=128"))
        .unwrap_or_default();
    aq_server::build_worker_router(c.env.env.state.clone())
        .oneshot(
            axum::http::Request::get(format!(
                "/v1/worker/profiled/source/{}{suffix}",
                binding.attempt_id
            ))
            .header("authorization", format!("Bearer {}", c.env.env.credential))
            .header("aq-source-api-version", "1")
            .body(axum::body::Body::empty())
            .unwrap(),
        )
        .await
        .unwrap()
}
async fn member(
    c: &Case,
    b: &aq_admission::LaunchBinding,
    name: &str,
    size: u64,
    digest: &str,
) -> Vec<u8> {
    assert!((1..=1_048_576).contains(&size));
    let mut out = Vec::new();
    while (out.len() as u64) < size {
        let r = get(c, b, Some(name), out.len() as u64).await;
        assert_eq!(r.status(), 200);
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        assert!(!bytes.is_empty());
        out.extend(bytes);
        assert!((out.len() as u64) <= size);
    }
    assert_eq!(Digest::of(&out).as_str(), digest);
    out
}
#[tokio::test]
#[ignore = "qualification requires explicit pinned actual aq-git and Git executable paths"]
async fn actual_retained_source_reads_after_proposal_expiry_use_only_owned_v2_lease() {
    let mut actual = setup_actual_unpublished().await;
    configure(&mut actual.case).await;
    let c = &actual.case;
    let (status, body) = native_offer::poll(c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1);
    let native = page.offers.remove(0);
    let negotiation = phase::Negotiation {
        contract: phase::contract_artifact().unwrap(),
        features: phase::required_features(),
    };
    let request = ProposalRequest {
        identity: c.request.identity.clone(),
        binding: native.offered.offer.binding.clone(),
        negotiation: negotiation.clone(),
    };
    let auth = context(c).await;
    let proposal = owner::propose(&c.env.env.state, &auth, &request)
        .await
        .unwrap();
    let original = native_offer::acceptance(c, &native);
    let q = AcceptRequest {
        schema_version: 2,
        negotiation,
        proposal_digest: proposal.digest().unwrap(),
        hold: ResourceHold {
            maximum_preparation_ms: 30000,
            reservation: original.reservation.clone(),
            resources: Resources {
                slots: 1,
                memory_bytes: native.offered.offer.manifest.resources.memory_bytes,
                cpu_millis: native.offered.offer.manifest.resources.cpu_millis,
                disk_bytes: 1 << 20,
            },
            source_profile: reference("synthetic-retained-reader-profile-no-hydration"),
            source_disk_bytes: 1 << 20,
        },
        original,
    };
    let accepted = owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    assert!(accepted.receipt.lease.expires_at_ms > proposal.original.offered.clock.expires_at_ms);
    let before = get(c, &request.binding, None, 0).await;
    assert_eq!(before.status(), 200);
    let before: Value =
        serde_json::from_slice(&before.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let original_source = serde_json::to_value(&proposal.spec.source).unwrap();
    let original_manifest =
        serde_json::to_value(&proposal.original.offered.offer.manifest).unwrap();
    // No clock substitution, offer/source retiming, TTL change or retry tuning.
    let current = chrono::Utc::now().timestamp_millis() as u64;
    let wait = proposal
        .original
        .offered
        .clock
        .expires_at_ms
        .saturating_sub(current)
        + 25;
    assert!(wait <= 2025);
    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
    let current = chrono::Utc::now().timestamp_millis() as u64;
    assert!(current >= proposal.original.offered.clock.expires_at_ms);
    assert!(current < accepted.receipt.lease.expires_at_ms);
    let after = get(c, &request.binding, None, 0).await;
    assert_eq!(after.status(), 200);
    let after: Value =
        serde_json::from_slice(&after.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(before, after);
    let transfer: aq_worker_protocol::TransferManifest = serde_json::from_value(after).unwrap();
    let manifest = member(
        c,
        &request.binding,
        "manifest",
        transfer.manifest_bytes,
        &transfer.manifest_digest,
    )
    .await;
    let bundle = member(
        c,
        &request.binding,
        "bundle",
        transfer.bundle_bytes,
        transfer.bundle_digest.as_deref().unwrap(),
    )
    .await;
    let exported: Value = serde_json::from_slice(&manifest).unwrap();
    assert_eq!(exported["base"]["hex"], actual.base);
    assert_eq!(exported["source"]["hex"], actual.base);
    assert_eq!(exported["bundle_sha256"], Digest::of(&bundle).as_str());
    let file = actual.seed.join("accepted-v2-downloaded.bundle");
    std::fs::write(&file, &bundle).unwrap();
    git(&actual.seed, &["bundle", "verify", file.to_str().unwrap()]).await;
    let replay = owner::accept(&c.env.env.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        accepted.receipt.digest().unwrap(),
        replay.receipt.digest().unwrap()
    );
    assert_eq!(
        original_source,
        serde_json::to_value(&proposal.spec.source).unwrap()
    );
    assert_eq!(
        original_manifest,
        serde_json::to_value(&proposal.original.offered.offer.manifest).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&q.hold).unwrap(),
        serde_json::to_value(&replay.receipt.lease.hold).unwrap()
    );
    let mut artifacts = proposal
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
    for (name, bytes) in [("manifest", &manifest), ("bundle", &bundle)] {
        let digest = Digest::of(bytes);
        artifacts.push(aq_admission::transport::ArtifactReceipt {
            artifact: ArtifactRef {
                id: Text::new(format!("aq.source-{name}/{}", transfer.transfer_id)).unwrap(),
                version: Text::new("1").unwrap(),
                digest: digest.clone(),
            },
            bytes: bytes.len() as u64,
            digest,
        });
    }
    let materials = accepted
        .receipt
        .lease
        .materials
        .iter()
        .map(|m| MaterialReceipt {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            lease_id: m.lease_id,
            binding: m.binding.clone(),
            material_digest: m.material_digest.clone(),
            private_binding: ArtifactRef {
                id: Text::new(format!("aq.private-material/{}", m.lease_id)).unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(b"synthetic worker install claim; no physical worker"),
            },
            received_at_ms: accepted.receipt.lease.issued_at_ms,
        })
        .collect();
    let prepared = phase::PrepareRequest {
        schema_version: 2,
        negotiation: q.negotiation.clone(),
        accepted_receipt_digest: accepted.receipt.digest().unwrap(),
        expected_preparation_revision: 1,
        original: Prepare {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            identity: q.original.identity.clone(),
            binding: request.binding.clone(),
            reservation: q.original.reservation.clone(),
            guardian_id: q.original.guardian_id,
            launch_id: q.original.launch_id,
            journal_revision: q.original.journal_revision + 1,
            journal_digest: Digest::of(b"synthetic Prepared no host"),
            artifacts,
            materials,
        },
    };
    prepared
        .validate_for(&accepted.receipt, &q, &proposal)
        .unwrap();
    assert!(
        initial_grant::prepare(&c.env.env.state, &auth, &prepared.original, Uuid::new_v4())
            .await
            .is_err()
    );
    assert!((chrono::Utc::now().timestamp_millis() as u64) < accepted.receipt.lease.expires_at_ms);
    let pin = rpc(
        &actual.socket,
        transfer.source.repository_id,
        "retention.inspect",
        json!({"handle_id":exported["transfer_id"]}),
    )
    .await;
    rpc(&actual.socket,transfer.source.repository_id,"retention.release",json!({"handle_id":exported["transfer_id"],"expected_generation":pin["generation"],"refs":pin["refs"],"reason":"explicit qualification source retirement"})).await;
    assert_ne!(get(c, &request.binding, None, 0).await.status(), 200);
    let row=sqlx::query("SELECT a.holds_task,h.state->'grant' AS grant,(SELECT count(*) FROM admission_material_receipts) AS installations FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1").bind(request.binding.attempt_id).fetch_one(&c.env.env.pool).await.unwrap();
    assert!(row.get::<bool, _>("holds_task"));
    assert!(row.get::<Value, _>("grant").is_null());
    assert_eq!(row.get::<i64, _>("installations"), 0);
    println!(
        "accepted_v2_actual_git {}",
        json!({"proposal_clock":proposal.original.offered.clock,"historical_source_observed_at_ms":proposal.spec.source.observed_at_ms,"historical_source_expires_at_ms":proposal.spec.source.expires_at_ms,"accepted_issued_at_ms":accepted.receipt.lease.issued_at_ms,"accepted_expires_at_ms":accepted.receipt.lease.expires_at_ms,"read_started_at_ms":current,"source_profile":q.hold.source_profile,"transfer_id":transfer.transfer_id,"original_base":actual.base,"manifest_digest":transfer.manifest_digest,"bundle_digest":transfer.bundle_digest,"export_retirement_refused_reads":true,"holds_task":true,"grant":null,"installed_receipts":0})
    );
    actual.process.kill().await.unwrap();
    actual.process.wait().await.unwrap();
}
