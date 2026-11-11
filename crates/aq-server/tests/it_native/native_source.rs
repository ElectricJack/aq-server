//! Actual SQL/router/file reads with synthetic public Git peers/facts. No real
//! provider or complete native producer qualification is claimed by this file.
use crate::common;
use aq_admission::{
    ArtifactKind,
    control::{ControlReply, decode_reply},
};
use aq_server::profiled_execution::{
    source,
    source_exports::{self, Reservation},
};
use common::{
    profiled::NOW,
    source_exports::{setup, synthetic_fact},
};
use sqlx::Row;
use uuid::Uuid;
#[tokio::test]
async fn actual_source_owner_retains_typed_bytes_without_relabeling_context_proposal() {
    let (f, request, _dir) = setup().await;
    let pool = f.db.pool().await;
    let row=sqlx::query("SELECT artifact_id,body,descriptor FROM admission_artifacts WHERE descriptor->'artifact'=$1 AND owner_kind='source_retention'").bind(serde_json::to_value(&request.source_observation).unwrap()).fetch_one(&pool).await.unwrap();
    let bytes: Vec<u8> = row.get("body");
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let mut invalid = body.clone();
    invalid["evidence"]["commit_inspection"]["tree"]["hex"] = "0".repeat(40).into();
    assert!(
        source::validate_body(
            &serde_json::to_vec(&invalid).unwrap(),
            request.project_id,
            NOW
        )
        .is_err(),
        "Git's null tree ID cannot be source admission evidence"
    );
    let observation = source::Observation {
        admission: serde_json::from_value(body["admission"].clone()).unwrap(),
        retention_handle: request.source_retention_handle,
        artifact: request.source_observation.clone(),
        bytes,
    };
    let operation = Uuid::from_u128(1);
    let retained = source::retain_admission(&f.state, operation, &observation, NOW + 600000)
        .await
        .unwrap();
    assert_eq!(retained.kind, ArtifactKind::Source);
    assert_eq!(retained.artifact, request.source_observation);
    assert_eq!(
        source::retain_admission(&f.state, operation, &observation, NOW + 600000)
            .await
            .unwrap()
            .retention_receipt,
        retained.retention_receipt
    );
    let old: serde_json::Value =
        sqlx::query_scalar("SELECT descriptor FROM admission_artifacts WHERE artifact_id=$1")
            .bind(row.get::<Uuid, _>("artifact_id"))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(old["kind"], "context");
    assert!(
        matches!(
            source_exports::reserve(&f.state, request.clone())
                .await
                .unwrap(),
            Reservation::Pending(_)
        ),
        "a lower-ID Source receipt must not shadow the original Context proposal"
    );
    f.time
        .store((NOW + 30_001) as i64, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        source::retain_admission(&f.state, operation, &observation, NOW + 600000)
            .await
            .unwrap()
            .retention_receipt,
        retained.retention_receipt
    );
    assert!(
        source::retain_admission(&f.state, Uuid::new_v4(), &observation, NOW + 600000)
            .await
            .is_err()
    );
    let mut bad = observation;
    bad.retention_handle = Uuid::new_v4();
    assert!(
        source::retain_admission(&f.state, Uuid::new_v4(), &bad, NOW + 600000)
            .await
            .is_err()
    );
    assert!(source::validate_body(&bad.bytes, Uuid::new_v4(), NOW).is_err());
}
struct Prepared {
    f: common::profiled::Fixture,
    _dir: tempfile::TempDir,
    request: source_exports::Request,
    fact: aq_server::source_retention::profiled_base::VerifiedExport,
    peer: tokio::task::JoinHandle<()>,
    pause: std::sync::Arc<std::sync::atomic::AtomicBool>,
    entered: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        self.peer.abort();
    }
}
async fn prepared() -> Prepared {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (f, request, dir) = setup().await;
    let pool = f.db.pool().await;
    let Reservation::Pending(token) = source_exports::reserve(&f.state, request.clone())
        .await
        .unwrap()
    else {
        panic!()
    };
    let fact = synthetic_fact(&token);
    source_exports::finalize(&f.state, &token, &fact)
        .await
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&fact.canonical_bytes().unwrap()).unwrap();
    let retention = value["retention_inspection"].clone();
    let observation=serde_json::from_value(serde_json::json!({"request_digest":token.request_digest(),"export_operation_id":request.operation_id,"inspection":retention,"observed_at_ms":NOW,"expires_at_ms":NOW+30000})).unwrap();
    let mut tx = pool.begin().await.unwrap();
    source_exports::link_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        &token,
        &fact,
        &observation,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)")
        .bind(f.profiled.offer.binding.attempt_id)
        .bind(serde_json::json!(aq_server::execution::authority::hot::HotState::from_compat(&f.profiled).unwrap()))
        .execute(&pool).await.unwrap();
    let root = &f.state.source_retention.as_ref().unwrap().config.git_root;
    let artifact = root
        .join("artifacts")
        .join(request.operation_id.to_string());
    std::fs::create_dir_all(&artifact).unwrap();
    std::fs::write(root.join("format.json"),serde_json::to_vec(&serde_json::json!({"format_version":1,"installation_id":value["producer_installation_id"]})).unwrap()).unwrap();
    std::fs::write(artifact.join("manifest.json"), fact.manifest_bytes()).unwrap();
    std::fs::write(
        artifact.join("objects.bundle"),
        b"synthetic component bundle",
    )
    .unwrap();
    let listener = tokio::net::UnixListener::bind(
        &f.state.source_retention.as_ref().unwrap().config.git_socket,
    )
    .unwrap();
    let pause = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let resume = std::sync::Arc::new(tokio::sync::Notify::new());
    let (peer_pause, peer_entered, peer_resume) = (pause.clone(), entered.clone(), resume.clone());
    let peer = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let q: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(q["operation"], "retention.inspect");
            if peer_pause.load(std::sync::atomic::Ordering::SeqCst) {
                peer_entered.notify_one();
                peer_resume.notified().await;
            }
            let mut bytes=serde_json::to_vec(&serde_json::json!({"protocol_version":1,"request_id":q["request_id"],"ok":true,"result":retention})).unwrap();
            bytes.push(b'\n');
            stream.get_mut().write_all(&bytes).await.unwrap();
        }
    });
    Prepared {
        f,
        _dir: dir,
        request,
        fact,
        peer,
        pause,
        entered,
        resume,
    }
}
async fn get(
    p: &Prepared,
    suffix: &str,
    credential: &str,
    version: Option<&str>,
) -> axum::response::Response {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .uri(format!(
            "/v1/worker/profiled/source/{}{}",
            p.f.profiled.offer.binding.attempt_id, suffix
        ))
        .header("authorization", format!("Bearer {credential}"));
    if let Some(v) = version {
        request = request.header("aq-source-api-version", v);
    }
    aq_server::build_worker_router(p.f.state.clone())
        .oneshot(request.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap()
}
#[tokio::test]
async fn current_auth_source_status_and_exact_chunks_are_bounded_without_grant_or_receipt_side_effect()
 {
    use http_body_util::BodyExt;
    let p = prepared().await;
    let pool = p.f.db.pool().await;
    let response = get(&p, "", &p.f.credential, Some("1")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["aq-source-max-range"], "1048576");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let manifest: aq_worker_protocol::TransferManifest = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(manifest.artifact_id, p.request.operation_id);
    let response = get(&p, "/bundle?offset=2&limit=5", &p.f.credential, Some("1")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["aq-source-offset"], "2");
    assert!(response.headers()["aq-source-transfer"].len() <= 4096);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        &b"nthet"[..]
    );
    let response = get(
        &p,
        "/manifest?offset=0&limit=262144",
        &p.f.credential,
        Some("1"),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        p.fact.manifest_bytes()
    );
    let larger = get(
        &p,
        "/bundle?offset=0&limit=1048576",
        &p.f.credential,
        Some("1"),
    )
    .await;
    assert_eq!(larger.status(), 200);
    let refused = get(
        &p,
        "/bundle?offset=0&limit=1048577",
        &p.f.credential,
        Some("1"),
    )
    .await;
    assert_eq!(refused.status(), 400);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_execution_receipts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_owner_journals")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn version_range_unknown_fields_auth_and_symlink_fail_with_bounded_typed_errors() {
    use http_body_util::BodyExt;
    let p = prepared().await;
    for (suffix, version, credential, status) in [
        ("", None, p.f.credential.as_str(), 400),
        (
            "/bundle?offset=0&limit=0",
            Some("1"),
            p.f.credential.as_str(),
            400,
        ),
        (
            "/bundle?offset=0&limit=1&path=x",
            Some("1"),
            p.f.credential.as_str(),
            400,
        ),
        (
            "/secret?offset=0&limit=1",
            Some("1"),
            p.f.credential.as_str(),
            400,
        ),
        ("", Some("1"), "invalid", 401),
    ] {
        let response = get(&p, suffix, credential, version).await;
        assert_eq!(response.status(), status);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.len() < 1024);
        assert!(matches!(
            decode_reply(&bytes).unwrap(),
            ControlReply::Failure(_)
        ));
    }
    let artifact =
        p.f.state
            .source_retention
            .as_ref()
            .unwrap()
            .config
            .git_root
            .join("artifacts")
            .join(p.request.operation_id.to_string());
    let bundle = artifact.join("objects.bundle");
    std::fs::remove_file(&bundle).unwrap();
    std::os::unix::fs::symlink("/dev/null", &bundle).unwrap();
    assert_eq!(
        get(&p, "/bundle?offset=0&limit=1", &p.f.credential, Some("1"))
            .await
            .status(),
        409
    );
}

#[tokio::test]
async fn preparing_and_granted_source_reads_use_original_plan_and_current_authority() {
    use aq_admission::{
        Digest, Label, MaterialLease,
        transport::{DeliveryClock, NoLaunchReservation, PreparationPlan, TransportDocument},
    };
    use aq_server::execution::{
        authority::hot::{HotState, LaunchOwner},
        profiled::Status,
    };
    let p = prepared().await;
    let pool = p.f.db.pool().await;
    let binding = p.f.profiled.offer.binding.clone();
    let owner = LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    };
    let operation = Uuid::new_v4();
    let plan = PreparationPlan {
        schema_version: 1,
        operation_id: operation,
        binding: binding.clone(),
        reservation: NoLaunchReservation {
            reservation_id: owner.reservation_id,
            reservation_operation_id: Uuid::new_v4(),
            reservation_journal_digest: Digest::of(b"synthetic-reservation"),
        },
        guardian_id: owner.guardian_id,
        launch_id: owner.launch_id,
        clock: DeliveryClock::new(NOW, NOW + 1000).unwrap(),
        materials: vec![MaterialLease {
            schema_version: 1,
            lease_id: Uuid::new_v4(),
            binding: binding.clone(),
            credential_profile: p.f.selection.choice.credential_profile.clone(),
            credential_revision: p.f.selection.credential_revision,
            provider: Label::new(&p.f.selection.choice.provider).unwrap(),
            account_reference: Label::new(&p.f.selection.choice.account_reference).unwrap(),
            endpoint_reference: Label::new(&p.f.selection.choice.endpoint_reference).unwrap(),
            purpose: Label::new("model-authentication").unwrap(),
            material_digest: Digest::of(b"synthetic-secret"),
            material_bytes: 16,
            issued_at_ms: NOW,
            expires_at_ms: NOW + 1000,
        }],
    };
    let doc = TransportDocument::PreparationPlan(plan);
    doc.validate().unwrap();
    let mut hot = HotState::from_compat(&p.f.profiled).unwrap();
    hot.status = Status::Preparing;
    hot.owner = Some(owner);
    hot.preparation_operation_id = Some(operation);
    sqlx::query("INSERT INTO profiled_execution_receipts(node_id,operation_id,kind,request_digest,response) VALUES($1,$2,'accept',$3,$4)").bind(p.f.node.node_id).bind(operation).bind(Digest::of(b"synthetic-accept").as_str()).bind(serde_json::json!(doc)).execute(&pool).await.unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(binding.attempt_id)
        .bind(serde_json::json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(get(&p, "", &p.f.credential, Some("1")).await.status(), 200);
    p.f.time
        .store((NOW + 1001) as i64, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(get(&p, "", &p.f.credential, Some("1")).await.status(), 409);
    p.f.time
        .store(NOW as i64, std::sync::atomic::Ordering::SeqCst);
    hot.status = Status::Granted;
    hot.execution_deadline_ms = Some(NOW + 30000);
    hot.grant = Some(aq_session_protocol::ProfiledGrant {
        binding: binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 1000,
    });
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(binding.attempt_id)
        .bind(serde_json::json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        get(&p, "/bundle?offset=0&limit=1", &p.f.credential, Some("1"))
            .await
            .status(),
        200
    );
    p.f.time
        .store((NOW + 1001) as i64, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(get(&p, "", &p.f.credential, Some("1")).await.status(), 409);
    p.f.time
        .store(NOW as i64, std::sync::atomic::Ordering::SeqCst);
    hot.status = Status::Cancelling;
    hot.cancellation_id = Some(Uuid::new_v4());
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(binding.attempt_id)
        .bind(serde_json::json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(get(&p, "", &p.f.credential, Some("1")).await.status(), 409);
}

#[tokio::test]
async fn credential_rotation_during_git_io_refuses_already_authenticated_read() {
    let p = prepared().await;
    p.pause.store(true, std::sync::atomic::Ordering::SeqCst);
    let pool = p.f.db.pool().await;
    let read = get(&p, "", &p.f.credential, Some("1"));
    let rotate = async {
        tokio::time::timeout(std::time::Duration::from_secs(3), p.entered.notified())
            .await
            .unwrap();
        sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
            .bind(p.f.node.node_id)
            .bind("rotated-current-credential")
            .execute(&pool)
            .await
            .unwrap();
        p.resume.notify_one();
    };
    let (response, ()) = tokio::join!(read, rotate);
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn offer_expiry_during_git_io_refuses_preflight_authority() {
    let p = prepared().await;
    p.pause.store(true, std::sync::atomic::Ordering::SeqCst);
    let read = get(&p, "", &p.f.credential, Some("1"));
    let expire = async {
        tokio::time::timeout(std::time::Duration::from_secs(3), p.entered.notified())
            .await
            .unwrap();
        p.f.time
            .store((NOW + 30_001) as i64, std::sync::atomic::Ordering::SeqCst);
        p.resume.notify_one();
    };
    let (response, ()) = tokio::join!(read, expire);
    assert_eq!(response.status(), 409);
}
