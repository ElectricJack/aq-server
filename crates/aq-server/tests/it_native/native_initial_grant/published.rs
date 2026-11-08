//! Actual mounted offer/acceptance owners; the runtime/installation claims are
//! synthetic qualification fixtures, never a physical model/harness claim.
use super::common::native_offer::{self, Case};
use aq_admission::{
    ArtifactRef, Digest, MaterialReceipt, Text,
    control::InitialGrantDelivery,
    startup::NativeOffer,
    transport::{ArtifactReceipt, PreparationPlan, Prepare},
};
use aq_server::{
    profiled_execution::initial_grant,
    worker_api::{self, AuthenticatedNode},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use uuid::Uuid;
struct Prepared {
    case: Case,
    offer: NativeOffer,
    plan: PreparationPlan,
    request: Prepare,
    clock: Arc<AtomicI64>,
}
async fn auth(c: &Prepared) -> AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", c.case.env.env.credential)
            .parse()
            .unwrap(),
    );
    worker_api::authenticate_context(&c.case.env.env.state, &headers)
        .await
        .unwrap()
}
async fn setup() -> Prepared {
    let (case, offer) = native_offer::published().await;
    setup_from(case, offer).await
}
async fn setup_from(mut case: Case, offer: NativeOffer) -> Prepared {
    let accepted = native_offer::acceptance(&case, &offer);
    let plan = native_offer::accept(&case, &accepted).await.original;
    let clock = Arc::new(AtomicI64::new(plan.clock.server_time_ms as i64 + 1));
    let at = clock.clone();
    case.env.env.state.clock = Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(at.load(Ordering::SeqCst)).unwrap()
    });
    let mut artifacts = offer
        .offered
        .artifacts
        .iter()
        .map(|a| ArtifactReceipt {
            artifact: a.artifact.clone(),
            bytes: a.bytes,
            digest: a.artifact.digest.clone(),
        })
        .collect::<Vec<_>>();
    let transfer: Value =
        sqlx::query_scalar("SELECT manifest FROM source_bases WHERE attempt_id=$1")
            .bind(plan.binding.attempt_id)
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    let transfer: aq_worker_protocol::TransferManifest = serde_json::from_value(transfer).unwrap();
    for (name, hash, bytes) in [
        (
            "manifest",
            transfer.manifest_digest.as_str(),
            transfer.manifest_bytes,
        ),
        (
            "bundle",
            transfer.bundle_digest.as_deref().unwrap(),
            transfer.bundle_bytes,
        ),
    ] {
        let digest: Digest = serde_json::from_value(json!(hash)).unwrap();
        artifacts.push(ArtifactReceipt {
            artifact: ArtifactRef {
                id: Text::new(format!("aq.source-{name}/{}", transfer.transfer_id)).unwrap(),
                version: Text::new("1").unwrap(),
                digest: digest.clone(),
            },
            bytes,
            digest,
        });
    }
    let materials = plan
        .materials
        .iter()
        .map(|l| MaterialReceipt {
            schema_version: 1,
            operation_id: Uuid::new_v4(),
            lease_id: l.lease_id,
            binding: l.binding.clone(),
            material_digest: l.material_digest.clone(),
            private_binding: ArtifactRef {
                id: Text::new(format!("aq.private-material/{}", l.lease_id)).unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(
                    b"synthetic opaque installer record: no physical worker qualification",
                ),
            },
            received_at_ms: plan.clock.server_time_ms,
        })
        .collect();
    let request = Prepare {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: accepted.identity,
        binding: plan.binding.clone(),
        reservation: plan.reservation.clone(),
        guardian_id: plan.guardian_id,
        launch_id: plan.launch_id,
        journal_revision: accepted.journal_revision + 1,
        journal_digest: Digest::of(b"synthetic durable Prepared intent"),
        artifacts,
        materials,
    };
    aq_admission::transport::TransportDocument::Prepare(request.clone())
        .validate()
        .unwrap();
    Prepared {
        case,
        offer,
        plan,
        request,
        clock,
    }
}
async fn grant(c: &Prepared) -> Result<InitialGrantDelivery, aq_server::error::AppError> {
    initial_grant::prepare(
        &c.case.env.env.state,
        &auth(c).await,
        &c.request,
        Uuid::new_v4(),
    )
    .await
}
async fn ungranted(c: &Prepared) {
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(c.request.binding.attempt_id)
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(hot["status"], "preparing");
    assert!(hot["grant"].is_null());
    assert!(hot["execution_deadline_ms"].is_null());
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT phase FROM profiled_owner_journals WHERE attempt_id=$1"
        )
        .bind(c.request.binding.attempt_id)
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap(),
        "reserved"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_execution_receipts WHERE kind='prepare'"
        )
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap(),
        0
    );
}
#[tokio::test]
async fn actual_published_prepare_is_atomic_and_replays_original_versions_after_hot_record_changes()
{
    let c = setup().await;
    let first = grant(&c).await.unwrap();
    first
        .validate_for(&c.request, &c.plan, &c.offer, first.delivery.nonce)
        .unwrap();
    assert_eq!(
        first.original.grant.expires_at_ms,
        c.plan.clock.expires_at_ms
    );
    let row: (String, Uuid) =
        sqlx::query_as("SELECT phase,grant_id FROM profiled_owner_journals WHERE attempt_id=$1")
            .bind(c.request.binding.attempt_id)
            .fetch_one(&c.case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(row, ("prepared".into(), first.original.grant.grant_id));
    sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{effects}',$2) WHERE attempt_id=$1").bind(c.request.binding.attempt_id).bind(json!({"kind":"pending","count":1})).execute(&c.case.env.env.pool).await.unwrap();
    c.clock.fetch_add(50, Ordering::SeqCst);
    let next = grant(&c).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first.original).unwrap(),
        serde_json::to_value(&next.original).unwrap()
    );
    assert_eq!(first.versions, next.versions);
    assert_eq!(first.execution_deadline_ms, next.execution_deadline_ms);
    assert!(next.delivery.clock.valid_for_ms < first.delivery.clock.valid_for_ms);
    let current: i64 = sqlx::query_scalar(
        "SELECT record_revision FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(c.request.binding.attempt_id)
    .fetch_one(&c.case.env.env.pool)
    .await
    .unwrap();
    assert!(current as u64 > next.versions.record_revision);
    let mut changed = c.request.clone();
    changed.journal_digest = Digest::of(b"changed same operation");
    assert!(
        initial_grant::prepare(
            &c.case.env.env.state,
            &auth(&c).await,
            &changed,
            Uuid::new_v4()
        )
        .await
        .is_err()
    );
    changed = c.request.clone();
    changed.operation_id = Uuid::new_v4();
    assert!(
        initial_grant::prepare(
            &c.case.env.env.state,
            &auth(&c).await,
            &changed,
            Uuid::new_v4()
        )
        .await
        .is_err()
    );
    c.clock
        .store(first.original.grant.expires_at_ms as i64, Ordering::SeqCst);
    assert!(grant(&c).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_execution_receipts WHERE kind='prepare'"
        )
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap(),
        1
    );
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(c.request.binding.attempt_id)
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap()
    );
}
#[tokio::test]
async fn exact_full_artifact_and_material_receipt_coverage_is_mandatory() {
    let c = setup().await;
    let mut variants = vec![];
    let mut r = c.request.clone();
    r.artifacts.pop();
    variants.push(r);
    let mut r = c.request.clone();
    r.artifacts[0].bytes += 1;
    variants.push(r);
    let mut r = c.request.clone();
    let digest = Digest::of(b"wrong content");
    r.artifacts[0].digest = digest.clone();
    r.artifacts[0].artifact.digest = digest;
    variants.push(r);
    let mut r = c.request.clone();
    r.materials[0].private_binding.id = Text::new("other-owner").unwrap();
    variants.push(r);
    let mut r = c.request.clone();
    r.materials[0].material_digest = Digest::of(b"wrong private bytes");
    variants.push(r);
    for r in variants {
        assert!(
            initial_grant::prepare(&c.case.env.env.state, &auth(&c).await, &r, Uuid::new_v4())
                .await
                .is_err()
        );
        ungranted(&c).await;
    }
    grant(&c).await.unwrap();
}
#[tokio::test]
async fn receipt_failure_rolls_back_granted_hot_state_and_prepared_journal_then_exact_retry_succeeds()
 {
    let c = setup().await;
    sqlx::raw_sql("CREATE FUNCTION fixture_refuse_initial_grant() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='prepare' THEN RAISE EXCEPTION 'fixture receipt failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fixture_refuse_initial_grant BEFORE INSERT ON profiled_execution_receipts FOR EACH ROW EXECUTE FUNCTION fixture_refuse_initial_grant();").execute(&c.case.env.env.pool).await.unwrap();
    assert!(grant(&c).await.is_err());
    ungranted(&c).await;
    sqlx::raw_sql("DROP TRIGGER fixture_refuse_initial_grant ON profiled_execution_receipts; DROP FUNCTION fixture_refuse_initial_grant();").execute(&c.case.env.env.pool).await.unwrap();
    grant(&c).await.unwrap();
}
#[tokio::test]
async fn concurrent_exact_prepare_never_creates_two_grants_or_extends_the_original() {
    let c = setup().await;
    let a = auth(&c).await;
    let b = auth(&c).await;
    let (x, y) = tokio::join!(
        initial_grant::prepare(&c.case.env.env.state, &a, &c.request, Uuid::new_v4()),
        initial_grant::prepare(&c.case.env.env.state, &b, &c.request, Uuid::new_v4())
    );
    let original = x.or(y).expect("at least one original grant");
    let replay = grant(&c).await.unwrap();
    assert_eq!(
        serde_json::to_value(original.original).unwrap(),
        serde_json::to_value(replay.original).unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_execution_receipts WHERE kind='prepare'"
        )
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn changed_current_private_material_and_cancellation_never_issue_a_grant() {
    let c = setup().await;
    let registry = c.case.env.env.state.config.intelligence.as_ref().unwrap();
    let path = registry
        .config
        .credentials
        .iter()
        .find(|r| r.credential.profile == c.plan.materials[0].credential_profile)
        .unwrap()
        .secret_file
        .as_ref()
        .unwrap();
    let original = std::fs::read(path).unwrap();
    std::fs::write(path, b"changed synthetic credential fingerprint").unwrap();
    assert!(grant(&c).await.is_err());
    ungranted(&c).await;
    std::fs::write(path, original).unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{cancellation_id}',to_jsonb($2::text)) WHERE attempt_id=$1").bind(c.request.binding.attempt_id).bind(Uuid::new_v4().to_string()).execute(&c.case.env.env.pool).await.unwrap();
    assert!(grant(&c).await.is_err());
    ungranted(&c).await;
}

#[tokio::test]
async fn final_fence_refuses_expiry_after_observed_shared_quota_lock_wait() {
    let c = setup().await;
    let mut blocker = c.case.env.env.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('aq-native-preparation-quota-v1',0))",
    )
    .execute(&mut *blocker)
    .await
    .unwrap();
    let state = c.case.env.env.state.clone();
    let authenticated = auth(&c).await;
    let request = c.request.clone();
    let task = tokio::spawn(async move {
        initial_grant::prepare(&state, &authenticated, &request, Uuid::new_v4()).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1),async{loop{
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%aq-native-preparation-quota-v1%')").fetch_one(&c.case.env.env.pool).await.unwrap();
        if waiting{break}tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }}).await.expect("must observe actual quota lock wait");
    c.clock
        .store(c.plan.clock.expires_at_ms as i64, Ordering::SeqCst);
    blocker.commit().await.unwrap();
    assert!(task.await.unwrap().is_err());
    ungranted(&c).await;
}

#[tokio::test]
async fn credential_rotation_during_actual_private_git_wait_is_rechecked_without_sql_locks_held() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let c = setup().await;
    let original_socket = c
        .case
        .env
        .env
        .state
        .source_retention
        .as_ref()
        .unwrap()
        .config
        .git_socket
        .clone();
    let backend = original_socket.with_extension("grant-backend.sock");
    std::fs::rename(&original_socket, &backend).unwrap();
    let listener = tokio::net::UnixListener::bind(&original_socket).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let release = resume.clone();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        BufReader::new(&mut stream)
            .take(262145)
            .read_until(b'\n', &mut request)
            .await
            .unwrap();
        assert!(request.len() <= 262144);
        assert_eq!(request.last(), Some(&b'\n'));
        signal.notify_one();
        release.notified().await;
        let mut upstream = tokio::net::UnixStream::connect(backend).await.unwrap();
        upstream.write_all(&request).await.unwrap();
        let mut response = Vec::new();
        BufReader::new(upstream)
            .take(262145)
            .read_until(b'\n', &mut response)
            .await
            .unwrap();
        assert!(response.len() <= 262144);
        assert_eq!(response.last(), Some(&b'\n'));
        stream.write_all(&response).await.unwrap();
    });
    let state = c.case.env.env.state.clone();
    let authenticated = auth(&c).await;
    let request = c.request.clone();
    let task = tokio::spawn(async move {
        initial_grant::prepare(&state, &authenticated, &request, Uuid::new_v4()).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .expect("actual private Git request must reach gate");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
            .bind(c.request.identity.node_id)
            .bind("f".repeat(64))
            .execute(&c.case.env.env.pool),
    )
    .await
    .expect("external wait must hold no node SQL lock")
    .unwrap();
    resume.notify_one();
    peer.await.unwrap();
    assert!(task.await.unwrap().is_err());
    ungranted(&c).await;
}

#[tokio::test]
async fn ambiguous_historical_grant_receipts_refuse_instead_of_choosing_a_replacement() {
    let c = setup().await;
    grant(&c).await.unwrap();
    sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) SELECT node_id,$2,kind,request_digest,response FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$3").bind(c.request.identity.node_id).bind(Uuid::new_v4()).bind(c.request.operation_id).execute(&c.case.env.env.pool).await.unwrap();
    assert!(grant(&c).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT phase FROM profiled_owner_journals WHERE attempt_id=$1"
        )
        .bind(c.request.binding.attempt_id)
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap(),
        "prepared"
    );
}

#[tokio::test]
async fn mounted_prepare_uses_typed_outbound_control_and_never_mounts_on_human_listener() {
    use aq_admission::control::{ControlReply, decode_reply};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let c = setup().await;
    let nonce = Uuid::new_v4();
    let bytes = serde_json::to_vec(&c.request).unwrap();
    let build = |body: Vec<u8>, nonce: Option<Uuid>, credential: &str| {
        let mut builder = axum::http::Request::post("/v1/worker/profiled/prepare")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {credential}"));
        if let Some(id) = nonce {
            builder = builder.header("aq-delivery-nonce", id.to_string())
        }
        builder.body(axum::body::Body::from(body)).unwrap()
    };
    let worker = || aq_server::build_worker_router(c.case.env.env.state.clone());
    let human = aq_server::build_admin_router(c.case.env.env.state.clone());
    let mut request = build(bytes.clone(), Some(nonce), &c.case.env.env.credential);
    request
        .headers_mut()
        .insert("x-aq-dev-email", "owner@localhost".parse().unwrap());
    request
        .headers_mut()
        .insert("x-aq-request", "1".parse().unwrap());
    assert_eq!(human.oneshot(request).await.unwrap().status(), 404);
    for (body, physical, credential, status) in [
        (bytes.clone(), None, c.case.env.env.credential.as_str(), 400),
        (bytes.clone(), Some(nonce), "unrecognized", 401),
        (
            vec![b'x'; aq_admission::transport::MAX_FRAME_BYTES + 1],
            Some(nonce),
            c.case.env.env.credential.as_str(),
            413,
        ),
    ] {
        let response = worker()
            .oneshot(build(body, physical, credential))
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert!(matches!(
            decode_reply(&response.into_body().collect().await.unwrap().to_bytes()).unwrap(),
            ControlReply::Failure(_)
        ));
        ungranted(&c).await;
    }
    let response = worker()
        .oneshot(build(
            bytes.clone(),
            Some(nonce),
            &c.case.env.env.credential,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let ControlReply::InitialGrant(first) =
        decode_reply(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    else {
        panic!("expected initial grant")
    };
    first
        .validate_for(&c.request, &c.plan, &c.offer, nonce)
        .unwrap();
    c.clock.fetch_add(50, Ordering::SeqCst);
    let retry_nonce = Uuid::new_v4();
    let response = worker()
        .oneshot(build(bytes, Some(retry_nonce), &c.case.env.env.credential))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let ControlReply::InitialGrant(retry) =
        decode_reply(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    else {
        panic!("expected original grant retry")
    };
    retry
        .validate_for(&c.request, &c.plan, &c.offer, retry_nonce)
        .unwrap();
    assert_eq!(first.versions, retry.versions);
    assert_eq!(first.original.grant.grant_id, retry.original.grant.grant_id);
}

#[path = "published/actual_git.rs"]
mod actual_git;

#[tokio::test]
async fn graph_changes_during_preparation_refuse_without_retiming_original_inputs() {
    let c = setup().await;
    let original: Value = sqlx::query_scalar(
        "SELECT graph_task_observation FROM profiled_launches WHERE attempt_id=$1",
    )
    .bind(c.request.binding.attempt_id)
    .fetch_one(&c.case.env.env.pool)
    .await
    .unwrap();
    for mode in [1, 2, 3, 4] {
        c.case.mode.store(mode, Ordering::SeqCst);
        assert!(grant(&c).await.is_err(), "mode {mode} admitted stale graph");
        ungranted(&c).await;
    }
    // Private metadata is outside the authorized projection and cannot silently
    // enter startup. A fresh unchanged authorized projection may still grant.
    c.case.mode.store(5, Ordering::SeqCst);
    let first = grant(&c).await.unwrap();
    let retained: Value = sqlx::query_scalar(
        "SELECT graph_task_observation FROM profiled_launches WHERE attempt_id=$1",
    )
    .bind(c.request.binding.attempt_id)
    .fetch_one(&c.case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(original, retained);
    c.case.mode.store(4, Ordering::SeqCst);
    assert!(grant(&c).await.is_err());
    let grant_id: String = sqlx::query_scalar(
        "SELECT state->'grant'->>'grant_id' FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(c.request.binding.attempt_id)
    .fetch_one(&c.case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(grant_id, first.original.grant.grant_id.to_string());
}

#[tokio::test]
async fn actual_initial_grant_reconciliation_returns_history_without_live_clock_or_mutation() {
    use aq_admission::{
        control::{CurrentPhase, ReconcileEntry},
        transport::{JournalEvidence, Reconcile, ReconcileAttempt},
    };
    let c = setup().await;
    let first = grant(&c).await.unwrap();
    let request = Reconcile {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        after_attempt: None,
        limit: 1,
        attempts: vec![ReconcileAttempt {
            binding: c.request.binding.clone(),
            guardian_id: c.request.guardian_id,
            launch_id: c.request.launch_id,
            last_grant_id: None,
            journal: JournalEvidence::Unavailable {
                reason: aq_admission::transport::JournalUnavailable::Missing,
            },
        }],
    };
    let before:Value=sqlx::query_scalar("SELECT jsonb_build_object('hot',to_jsonb(h),'journal',to_jsonb(j),'receipts',(SELECT count(*) FROM profiled_execution_receipts)) FROM profiled_authority_hot h JOIN profiled_owner_journals j USING(attempt_id) WHERE attempt_id=$1").bind(c.request.binding.attempt_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    let page = aq_server::profiled_execution::reconciliation::reconcile(
        &c.case.env.env.state,
        &auth(&c).await,
        &request,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let ReconcileEntry::Current { authority } = &page.attempts[0] else {
        panic!()
    };
    assert!(
        matches!(authority.phase,CurrentPhase::Granted {grant_id,started_operation_id:None,..} if grant_id==first.original.grant.grant_id)
    );
    assert!(!serde_json::to_string(&page).unwrap().contains("\"clock\""));
    c.clock
        .store(first.original.grant.expires_at_ms as i64, Ordering::SeqCst);
    let expired = aq_server::profiled_execution::reconciliation::reconcile(
        &c.case.env.env.state,
        &auth(&c).await,
        &request,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let ReconcileEntry::Current { authority } = &expired.attempts[0] else {
        panic!()
    };
    assert!(
        matches!(authority.phase,CurrentPhase::StopRequired {last_grant_id:Some(id),..} if id==first.original.grant.grant_id)
    );
    let after:Value=sqlx::query_scalar("SELECT jsonb_build_object('hot',to_jsonb(h),'journal',to_jsonb(j),'receipts',(SELECT count(*) FROM profiled_execution_receipts)) FROM profiled_authority_hot h JOIN profiled_owner_journals j USING(attempt_id) WHERE attempt_id=$1").bind(c.request.binding.attempt_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn initial_deadline_respects_actual_pinned_lineage_policy_shorter_than_wall_time() {
    let mut case = native_offer::setup().await;
    let original: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(case.env.env.body["catalog_digest"].as_str().unwrap())
            .fetch_one(&case.env.env.pool)
            .await
            .unwrap();
    let catalog = aq_profiles::ValidatedCatalog::from_artifact(&original).unwrap();
    let mut artifact = catalog.artifact().clone();
    for profile in &mut artifact.catalog.intelligence {
        if let Some(policy) = profile.selection_policy.as_mut() {
            policy.max_lineage_elapsed_ms = 5000;
            profile.version = 3;
        }
    }
    for profile in &mut artifact.catalog.agents {
        profile.version = 3;
        profile.intelligence = "engineering@3".into();
    }
    let updated = aq_profiles::validate(artifact.catalog, artifact.instructions).unwrap();
    let app = aq_server::build_admin_router(case.env.env.state.clone());
    let (status,body)=super::common::selection::call(&app,"/api/profiles/catalogs/activate",Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"artifact_json":String::from_utf8(updated.canonical_bytes().unwrap()).unwrap()})),"owner@localhost").await;
    assert_eq!(status, 200, "{body}");
    case.env.env.body["catalog_digest"] = json!(updated.digest());
    case.env.env.body["allowed_agent_profiles"] = json!(["reviewer@3"]);
    case.env.env.body["allowed_intelligence_profiles"] = json!(["engineering@3"]);
    let (status,body)=super::common::selection::call(&app,&super::common::selection::path(),Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"binding":case.env.env.body})),"owner@localhost").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = native_offer::poll(&case).await;
    assert_eq!(status, 200, "{body}");
    let aq_admission::control::ControlReply::Poll(mut page) =
        aq_admission::control::decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1);
    let c = setup_from(case, page.offers.remove(0)).await;
    assert!(c.offer.offered.offer.manifest.resources.wall_time_ms > 5000);
    let first = grant(&c).await.unwrap();
    assert_eq!(
        first.execution_deadline_ms - first.original.clock.server_time_ms,
        5000
    );
    assert!(first.original.grant.expires_at_ms <= first.execution_deadline_ms);
    let replay = grant(&c).await.unwrap();
    assert_eq!(first.execution_deadline_ms, replay.execution_deadline_ms);
    assert_eq!(first.versions, replay.versions);
}

#[path = "published/material_receipts.rs"]
mod material_receipts;

#[path = "published/renewal.rs"]
mod renewal;

#[path = "published/presence.rs"]
mod presence;

/// Spec 0007 Q2 (fix round 1, C1): an offer selected while the owner's switch
/// is on, with no observation row at all. Time passes between selection,
/// accept and grant (nothing rewrites a fenced fact any more).
async fn switched_on() -> Prepared {
    let case = native_offer::setup().await;
    sqlx::query("DELETE FROM intelligence_observations")
        .execute(&case.env.env.pool)
        .await
        .unwrap();
    switch(&case, true, 1).await;
    let (status, body) = native_offer::poll(&case).await;
    assert_eq!(status, 200, "{body}");
    let reply = aq_admission::control::decode_reply(&serde_json::to_vec(&body).unwrap());
    let Ok(aq_admission::control::ControlReply::Poll(mut page)) = reply else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    case.env.env.time.fetch_add(2_000, Ordering::SeqCst);
    let offer = page.offers.remove(0);
    setup_from(case, offer).await
}
async fn switch(case: &Case, on: bool, expected: u64) {
    let pool = &case.env.env.pool;
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE email='owner@localhost'")
        .fetch_one(pool)
        .await
        .unwrap();
    let mut c = pool.acquire().await.unwrap();
    let now = (case.env.env.state.clock)();
    aq_machine_control::set_owner_availability(&mut c, on, expected, owner, Uuid::new_v4(), now)
        .await
        .unwrap();
}
async fn observation_rows(c: &Prepared) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM intelligence_observations")
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn owner_switch_on_offer_accepts_grants_and_replays_without_an_attestation_row() {
    let c = switched_on().await;
    c.clock.fetch_add(1_000, Ordering::SeqCst);
    let first = grant(&c).await.unwrap();
    c.clock.fetch_add(500, Ordering::SeqCst);
    let replay = grant(&c).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first.original).unwrap(),
        serde_json::to_value(&replay.original).unwrap()
    );
    assert_eq!(observation_rows(&c).await, 0);
}

#[tokio::test]
async fn owner_switch_flipped_between_selection_and_grant_refuses() {
    let c = switched_on().await;
    switch(&c.case, false, 2).await;
    let off = grant(&c).await.unwrap_err();
    assert!(format!("{off:?}").contains("availability"), "{off:?}");
    ungranted(&c).await;
    // Back on is a new generation: still not the switch the offer was selected under.
    switch(&c.case, true, 3).await;
    assert!(grant(&c).await.is_err());
    ungranted(&c).await;
}
