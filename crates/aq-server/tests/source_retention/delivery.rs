use super::*;
use aq_server::delivery::{
    CandidateRequest, CommitMetadata, DeliveryError, DeliveryService, ObservationRequest,
    OperationContext, PublishRequest, PublishResult,
};

pub(super) fn service(h: &Harness) -> DeliveryService {
    let config = SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: h.dir.path().join("staging"),
        git_root: h.dir.path().join("git"),
        git_socket: h.dir.path().join("git.sock"),
        max_staging_bytes: 2097152,
        max_bundle_bytes: 4096,
        bulk_concurrency: 2,
        incomplete_ttl_secs: 60,
    };
    DeliveryService::new(h.pool.clone(), &config)
}
pub(super) fn context(h: &Harness) -> OperationContext {
    OperationContext {
        operation_id: Uuid::new_v4(),
        repository_id: h.manifest.source.repository_id,
        binding_generation: 1,
    }
}
pub(super) fn candidate(h: &Harness, receipt: Uuid) -> CandidateRequest {
    CandidateRequest {
        context: context(h),
        base: h.manifest.source.base.clone(),
        source_receipts: vec![receipt],
        commit: CommitMetadata {
            name: "Coordinator".into(),
            email: "coordinator@example.test".into(),
            message: "central candidate".into(),
        },
    }
}
pub(super) async fn retain(h: &Harness) -> Uuid {
    let op = Uuid::new_v4();
    let m = &h.manifest;
    let ack = json!({"receiver_installation_id":Uuid::new_v4(),"repository_id":m.source.repository_id,"binding_generation":1,"transfer_id":m.artifact_id,"source":m.checkpoint,"retention_handle":m.artifact_id,"manifest_digest":m.manifest_digest,"retention_state":"active","historical":false});
    let receipt = json!({"receipt_id":op,"transfer_id":m.transfer_id,"repository_id":m.source.repository_id,"binding_generation":1,"source":m.checkpoint,"retention_handle":m.artifact_id,"manifest_digest":m.manifest_digest});
    sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,artifact_id,import_operation_id,manifest,state,receipt,central_receipt) VALUES($1,$2,$3,$4,$5,$6,$7,'retained',$8,$9)")
        .bind(m.transfer_id).bind(m.attempt.attempt_id).bind(h.identity.node_id).bind(m.source.repository_id).bind(m.artifact_id).bind(op).bind(json!(m)).bind(receipt).bind(ack).execute(&h.pool).await.unwrap();
    op
}
pub(super) async fn peer(h: &Harness, result: Value) -> tokio::task::JoinHandle<Value> {
    response_peer(h, json!({"ok":true,"result":result})).await
}
pub(super) async fn response_peer(h: &Harness, mut reply: Value) -> tokio::task::JoinHandle<Value> {
    let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
    let pool = h.pool.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        let id = Uuid::parse_str(req["operation_id"].as_str().unwrap()).unwrap();
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT request FROM delivery_intents WHERE operation_id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stored,
            line.as_bytes(),
            "intent must be committed before socket call"
        );
        reply["protocol_version"] = json!(1);
        reply["request_id"] = req["request_id"].clone();
        reply["operation_id"] = req["operation_id"].clone();
        reader
            .get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
        req
    })
}
pub(super) fn ready(h: &Harness, id: Uuid) -> Value {
    json!({"candidate_id":id,"state":"ready","candidate":h.manifest.checkpoint,"clean_prefix":h.manifest.checkpoint,"retention_handle":id,"conflict_step":null,"conflicting_paths":[],"conflict_path_count":0,"conflict_paths_truncated":false})
}
#[tokio::test]
async fn delivery_cross_checks_candidate_oid_and_binding_generation() {
    let h = Harness::new().await;
    let source = retain(&h).await;
    let svc = service(&h);
    let r = candidate(&h, source);
    let task = peer(&h, ready(&h, r.context.operation_id)).await;
    svc.create_candidate(r.clone()).await.unwrap();
    task.await.unwrap();
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    for matches_candidate in [false, true] {
        let p = PublishRequest {
            context: context(&h),
            candidate_id: r.context.operation_id,
            target: format!("refs/heads/check-{matches_candidate}"),
            expected_old: h.manifest.source.base.clone(),
        };
        let new = if matches_candidate {
            &h.manifest.checkpoint
        } else {
            &p.expected_old
        };
        let task = peer(&h, json!({"outcome":"applied","target":p.target,"old":p.expected_old,"new":new,"acknowledged_at":"2026-11-02T00:00:00Z"})).await;
        let result = svc.publish(p.clone()).await;
        let sent = task.await.unwrap();
        assert!(
            sent["params"].get("candidate").is_none(),
            "validation OID must stay off wire"
        );
        std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
        if matches_candidate {
            let receipt = result.unwrap();
            assert!(matches!(receipt.outcome, Ok(PublishResult::Applied { .. })));
            assert_eq!(service(&h).publish(p).await.unwrap().raw, receipt.raw);
        } else {
            assert!(matches!(result, Err(DeliveryError::Protocol)));
            assert!(matches!(
                service(&h).publish(p.clone()).await,
                Err(DeliveryError::Protocol)
            ));
            let held: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM delivery_destinations WHERE operation_id=$1",
            )
            .bind(p.context.operation_id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
            assert_eq!(held, 1, "mismatched applied reply cannot release the hold");
        }
    }
    for generation in [2, 1] {
        let observation = ObservationRequest {
            context: context(&h),
            target_id: r.context.operation_id,
        };
        let task = peer(&h, json!({"version":1,"id":r.context.operation_id,"generation":9,"status":"complete","phase":"complete","payload":{"repository_id":h.manifest.source.repository_id,"expected_binding_generation":generation},"payload_hash":"a".repeat(64),"result":{},"error":null,"metadata":{}})).await;
        let result = svc.operation_show(observation).await;
        task.await.unwrap();
        std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
        if generation == 1 {
            assert_eq!(result.unwrap().outcome.unwrap().generation, 9);
        } else {
            assert!(matches!(result, Err(DeliveryError::Protocol)));
        }
    }
}

#[tokio::test]
async fn delivery_candidate_transients_replay_same_frozen_operation() {
    let h = Harness::new().await;
    let source = retain(&h).await;
    let request = candidate(&h, source);
    let mut frozen = None;
    for code in ["overloaded", "io_error", "state_busy"] {
        let task = response_peer(&h, json!({"ok":false,"error":{"code":code,"message":"try again","details":{},"retry":"safe_same_operation"}})).await;
        let receipt = service(&h).create_candidate(request.clone()).await.unwrap();
        assert_eq!(receipt.outcome.unwrap_err().code, code);
        let sent = task.await.unwrap();
        if let Some(original) = &frozen {
            assert_eq!(&sent, original);
        } else {
            frozen = Some(sent);
        }
        std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM delivery_receipts WHERE operation_id=$1")
                .bind(request.context.operation_id)
                .fetch_one(&h.pool)
                .await
                .unwrap();
        assert_eq!(
            count, 0,
            "a transient attempt must not burn the operation ID"
        );
        sqlx::query("UPDATE source_transfers SET state='rejected' WHERE import_operation_id=$1")
            .bind(source)
            .execute(&h.pool)
            .await
            .unwrap();
        let mut changed = request.clone();
        changed.commit.message.push('x');
        assert!(matches!(
            service(&h).create_candidate(changed).await,
            Err(DeliveryError::IdempotencyConflict)
        ));
    }
    let task = peer(&h, ready(&h, request.context.operation_id)).await;
    let success = service(&h).create_candidate(request.clone()).await.unwrap();
    assert_eq!(task.await.unwrap(), frozen.unwrap());
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    assert_eq!(
        service(&h).create_candidate(request).await.unwrap().raw,
        success.raw
    );

    // Safe-same-operation advice alone is insufficient: only the named codes
    // without uncertainty qualify. Other failures remain immutable receipts.
    sqlx::query("UPDATE source_transfers SET state='retained' WHERE import_operation_id=$1")
        .bind(source)
        .execute(&h.pool)
        .await
        .unwrap();
    for (code, retry, details) in [
        ("overloaded", "manual", json!({})),
        (
            "io_error",
            "safe_same_operation",
            json!({"dispatched":true}),
        ),
        ("invalid_input", "safe_same_operation", json!({})),
    ] {
        let request = candidate(&h, source);
        let task = response_peer(&h, json!({"ok":false,"error":{"code":code,"message":"terminal","details":details,"retry":retry}})).await;
        let receipt = service(&h).create_candidate(request.clone()).await.unwrap();
        task.await.unwrap();
        std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
        assert_eq!(
            service(&h).create_candidate(request).await.unwrap().raw,
            receipt.raw
        );
    }
}

#[tokio::test]
async fn delivery_socket_exchange_releases_rows_and_pool_connection() {
    let mut h = Harness::new().await;
    h.pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(h._db.options())
        .await
        .unwrap();
    let source = retain(&h).await;
    let request = candidate(&h, source);
    let svc = service(&h);
    let restarted = service(&h);
    let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
    let pool = h.pool.clone();
    let result = ready(&h, request.context.operation_id);
    let peer = tokio::spawn(async move {
        let mut readers = Vec::new();
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let id: Uuid = serde_json::from_value(request["operation_id"].clone()).unwrap();
            // While aq-git has not replied, the service's sole pooled connection
            // and the intent row must be available to unrelated database work.
            let mut tx = pool.begin().await.unwrap();
            sqlx::query(
                "SELECT operation_id FROM delivery_intents WHERE operation_id=$1 FOR UPDATE NOWAIT",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            tx.commit().await.unwrap();
            readers.push(reader);
            requests.push(request);
        }
        assert_eq!(requests[0], requests[1]);
        for (index, reader) in readers.iter_mut().enumerate() {
            let reply = json!({"protocol_version":1,"request_id":requests[index]["request_id"],"operation_id":requests[index]["operation_id"],"ok":true,"result":result,"attempt":index});
            reader
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let (a, b, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (a, b, peer) = tokio::join!(
            svc.create_candidate(request.clone()),
            restarted.create_candidate(request.clone()),
            peer
        );
        (a.unwrap(), b.unwrap(), peer.unwrap())
    })
    .await
    .expect("database work and overlapping calls must finish without a socket reply");
    assert_eq!(
        a.raw, b.raw,
        "first persisted terminal reply wins concurrent callers"
    );
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    assert_eq!(
        service(&h).create_candidate(request).await.unwrap().raw,
        a.raw
    );
}

#[tokio::test]
async fn delivery_canonical_hold_targets_and_nonempty_unique_sources() {
    let h = Harness::new().await;
    let source = retain(&h).await;
    for sources in [vec![], vec![source, source]] {
        let mut request = candidate(&h, source);
        request.source_receipts = sources;
        assert!(matches!(
            service(&h).create_candidate(request).await,
            Err(DeliveryError::InvalidInput(_))
        ));
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_intents")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let request = candidate(&h, source);
    let task = peer(&h, ready(&h, request.context.operation_id)).await;
    service(&h).create_candidate(request.clone()).await.unwrap();
    task.await.unwrap();
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    let publish = PublishRequest {
        context: context(&h),
        candidate_id: request.context.operation_id,
        target: "refs/heads/CAFÉ".into(),
        expected_old: h.manifest.source.base.clone(),
    };
    // No peer: the dispatched operation is indeterminate and holds its key.
    assert!(matches!(
        service(&h).publish(publish.clone()).await,
        Err(DeliveryError::Indeterminate)
    ));
    let target: String =
        sqlx::query_scalar("SELECT target FROM delivery_destinations WHERE operation_id=$1")
            .bind(publish.context.operation_id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(target, "refs/heads/café");
    let wire: Vec<u8> =
        sqlx::query_scalar("SELECT request FROM delivery_intents WHERE operation_id=$1")
            .bind(publish.context.operation_id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&wire).unwrap()["params"]["target"],
        publish.target
    );
    for legacy in [false, true] {
        if legacy {
            sqlx::query("UPDATE delivery_destinations SET target=$2 WHERE operation_id=$1")
                .bind(publish.context.operation_id)
                .bind("refs/heads/CAFE\u{301}")
                .execute(&h.pool)
                .await
                .unwrap();
        }
        for spelling in [
            "refs/heads/café",
            "refs/heads/cafe\u{301}",
            "refs/heads/CAFÉ",
        ] {
            let mut other = publish.clone();
            other.context.operation_id = Uuid::new_v4();
            other.target = spelling.into();
            assert!(matches!(
                service(&h).publish(other).await,
                Err(DeliveryError::DestinationBlocked)
            ));
        }
    }
}

#[tokio::test]
async fn delivery_replay_conflict_and_unknown_publish_are_fenced() {
    let h = Harness::new().await;
    let source = retain(&h).await;
    let r = candidate(&h, source);
    let svc = service(&h);
    let task = peer(&h, ready(&h, r.context.operation_id)).await;
    let first = svc.create_candidate(r.clone()).await.unwrap();
    let sent = task.await.unwrap();
    assert_eq!(
        sent["params"]["sources"][0]["oid"],
        json!(h.manifest.checkpoint)
    );
    assert_eq!(
        sent["params"]["sources"][0]["retention_handle"],
        json!(h.manifest.artifact_id)
    );
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    assert_eq!(
        service(&h).create_candidate(r.clone()).await.unwrap().raw,
        first.raw
    );
    let mut changed = r.clone();
    changed.commit.message.push('x');
    assert!(matches!(
        svc.create_candidate(changed).await,
        Err(DeliveryError::IdempotencyConflict)
    ));
    let p = PublishRequest {
        context: context(&h),
        candidate_id: r.context.operation_id,
        target: "refs/heads/main".into(),
        expected_old: h.manifest.source.base.clone(),
    };
    assert!(matches!(
        svc.publish(p.clone()).await,
        Err(DeliveryError::Indeterminate)
    ));
    assert!(matches!(
        service(&h).publish(p.clone()).await,
        Err(DeliveryError::Indeterminate)
    ));
    let mut other = p;
    other.context.operation_id = Uuid::new_v4();
    assert!(matches!(
        svc.publish(other).await,
        Err(DeliveryError::DestinationBlocked)
    ));
    assert!(
        sqlx::query("UPDATE delivery_receipts SET response='bad' WHERE operation_id=$1")
            .bind(r.context.operation_id)
            .execute(&h.pool)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn delivery_rejects_unretained_sources() {
    let h = Harness::new().await;
    assert!(matches!(
        service(&h)
            .create_candidate(candidate(&h, Uuid::new_v4()))
            .await,
        Err(DeliveryError::InvalidInput(_))
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_intents")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let source = retain(&h).await;
    let mut foreign = candidate(&h, source);
    foreign.context.repository_id = Uuid::new_v4();
    assert!(matches!(
        service(&h).create_candidate(foreign).await,
        Err(DeliveryError::InvalidInput(_))
    ));
    sqlx::query("UPDATE source_transfers SET central_receipt=NULL WHERE import_operation_id=$1")
        .bind(source)
        .execute(&h.pool)
        .await
        .unwrap();
    assert!(matches!(
        service(&h).create_candidate(candidate(&h, source)).await,
        Err(DeliveryError::InvalidInput(_))
    ));
}

#[tokio::test]
#[ignore = "qualification requires retained aq-git and Git 2.56.0 executables"]
async fn candidate_from_retained_source_publishes_once_real_git() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut h = Harness::new().await;
    let worker = tempfile::tempdir().unwrap();
    let seed = worker.path().join("seed");
    tokio::fs::create_dir(&seed).await.unwrap();
    git_cmd(&seed, &["init", "-b", "main"]).await;
    tokio::fs::write(seed.join("source"), "base\n")
        .await
        .unwrap();
    git_cmd(&seed, &["add", "."]).await;
    git_cmd(&seed, &["commit", "-m", "base"]).await;
    let base = git_cmd(&seed, &["rev-parse", "HEAD"]).await;
    tokio::fs::write(seed.join("source"), "survives complete worker loss\n")
        .await
        .unwrap();
    git_cmd(&seed, &["add", "."]).await;
    git_cmd(&seed, &["commit", "-m", "source"]).await;
    let source = git_cmd(&seed, &["rev-parse", "HEAD"]).await;
    let transfer_ref = format!("refs/aq-git/transfers/{}", h.manifest.artifact_id);
    git_cmd(&seed, &["update-ref", &transfer_ref, &source]).await;
    let bundle = worker.path().join("source.bundle");
    git_cmd(
        &seed,
        &["bundle", "create", bundle.to_str().unwrap(), &transfer_ref],
    )
    .await;
    let bundle_bytes = tokio::fs::read(&bundle).await.unwrap();
    h.manifest.source.base.hex = base.clone();
    h.manifest.checkpoint.hex = source.clone();
    h.offer.spec.bindings.as_mut().unwrap().source = Some(h.manifest.source.clone());
    h.offer.spec = h.offer.spec.clone().seal();
    h.manifest.resolved_spec_digest = h.offer.spec.resolved_spec_digest.clone();
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{offer}',$2::jsonb) WHERE attempt_id=$1").bind(h.offer.attempt.attempt_id).bind(serde_json::to_string(&h.offer).unwrap()).execute(&h.pool).await.unwrap();
    let mut m: CanonicalManifest = serde_json::from_slice(&h.bytes).unwrap();
    m.base = h.manifest.source.base.clone();
    m.source = h.manifest.checkpoint.clone();
    m.bundle_size = bundle_bytes.len() as u64;
    m.bundle_sha256 = Some(hash(&bundle_bytes));
    m.reference_only = false;
    h.bytes = serde_json::to_vec_pretty(&m).unwrap();
    h.manifest.manifest_digest = hash(&h.bytes);
    h.manifest.manifest_bytes = h.bytes.len() as u64;
    h.manifest.bundle_digest = m.bundle_sha256;
    h.manifest.bundle_bytes = m.bundle_size;
    let binary = std::env::var("AQ_TEST_GIT_CANDIDATE").expect("set immutable aq-git candidate");
    let git = std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap();
    let socket = h.dir.path().join("git.sock");
    let mut child = tokio::process::Command::new(binary)
        .arg("--state-dir")
        .arg(h.dir.path().join("git"))
        .arg("--git-executable")
        .arg(git)
        .arg("--listen")
        .arg(&socket)
        .arg("serve")
        .process_group(0)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut process_group =
        GitFixtureGroup(Some(nix::unistd::Pid::from_raw(child.id().unwrap() as i32)));
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        assert!(child.try_wait().unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let remote = h.dir.path().join("remote.git");
    git_cmd(
        h.dir.path(),
        &[
            "clone",
            "--bare",
            seed.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    )
    .await;
    git_cmd(&remote, &["update-ref", "refs/heads/main", &base]).await;
    let repo = h.manifest.source.repository_id;
    real_call(&socket,repo,"repo.register",json!({"identity":h.manifest.source.logical_identity,"mode":"central","remote_url":remote,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]})).await;
    assert_eq!(
        h.json("/v1/execution/sources/begin", h.begin_body(Uuid::new_v4()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        h.upload(Uuid::new_v4(), 0, h.bytes.clone()).await.0,
        StatusCode::OK
    );
    let path = format!(
        "/v1/execution/sources/{}/bundle?operation_id={}&offset=0&digest={}",
        h.manifest.transfer_id,
        Uuid::new_v4(),
        hash(&bundle_bytes)
    );
    assert_eq!(
        h.request(
            "PUT",
            &path,
            bundle_bytes,
            "application/octet-stream",
            Some(&h.token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let (code, receipt) = h
        .json("/v1/execution/sources/finalize", h.status_body())
        .await;
    assert_eq!(code, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["receipt"]["source"]["hex"], source);
    worker.close().unwrap(); // Complete stopped producer volume disappears.
    let svc = service(&h);
    let source_receipt = serde_json::from_value(receipt["receipt"]["receipt_id"].clone()).unwrap();
    let request = candidate(&h, source_receipt);
    let created = svc.create_candidate(request.clone()).await.unwrap();
    let result = created.outcome.as_ref().unwrap();
    assert_eq!(result.candidate_id, request.context.operation_id);
    let published_oid = result.candidate.as_ref().unwrap().hex.clone();
    assert_eq!(
        svc.create_candidate(request.clone()).await.unwrap().raw,
        created.raw
    );
    let shown = svc
        .candidate_show(ObservationRequest {
            context: context(&h),
            target_id: request.context.operation_id,
        })
        .await
        .unwrap();
    assert_eq!(
        shown.outcome.unwrap().candidate_id,
        request.context.operation_id
    );
    let publish = PublishRequest {
        context: context(&h),
        candidate_id: request.context.operation_id,
        target: "refs/heads/main".into(),
        expected_old: h.manifest.source.base.clone(),
    };
    let mut rejected = publish.clone();
    rejected.context.operation_id = Uuid::new_v4();
    rejected.expected_old.hex = "0".repeat(40);
    assert!(matches!(
        svc.publish(rejected).await.unwrap().outcome,
        Ok(PublishResult::Rejected { .. })
    ));
    let (a, b) = tokio::join!(svc.publish(publish.clone()), svc.publish(publish.clone()));
    let first = match (a, b) {
        (Ok(a), _) => a,
        (_, Ok(b)) => b,
        errors => panic!("{errors:?}"),
    };
    assert!(matches!(first.outcome, Ok(PublishResult::Applied { .. })));
    assert_eq!(
        git_cmd(&remote, &["rev-parse", "refs/heads/main"]).await,
        published_oid
    );
    assert_eq!(
        service(&h).publish(publish.clone()).await.unwrap().raw,
        first.raw
    );
    let observed = svc
        .operation_show(ObservationRequest {
            context: context(&h),
            target_id: publish.context.operation_id,
        })
        .await
        .unwrap();
    assert_eq!(observed.outcome.unwrap().id, publish.context.operation_id);
    let mut again = publish.clone();
    again.context.operation_id = Uuid::new_v4();
    let present = svc.publish(again).await.unwrap();
    assert!(matches!(
        present.outcome,
        Ok(PublishResult::AlreadyPresent {
            caused_by_this_operation: false,
            ..
        })
    ));
    let mut denied = publish.clone();
    denied.context.operation_id = Uuid::new_v4();
    denied.target = "refs/heads/forbidden".into();
    assert!(svc.publish(denied).await.unwrap().outcome.is_err());
    let mut changed = publish;
    changed.expected_old.hex = "0".repeat(40);
    assert!(matches!(
        svc.publish(changed).await,
        Err(DeliveryError::IdempotencyConflict)
    ));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id().unwrap() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    process_group.0 = None;
    assert!(status.success());
    assert_eq!(
        service(&h).create_candidate(request).await.unwrap().raw,
        created.raw
    );
}

#[tokio::test]
async fn delivery_protocol_bounds_and_conflict_results() {
    let h = Harness::new().await;
    let source = retain(&h).await;
    let svc = service(&h);
    let r = candidate(&h, source);
    let mut conflict = ready(&h, r.context.operation_id);
    conflict["state"] = json!("conflicted");
    conflict["candidate"] = Value::Null;
    conflict["conflict_step"] = json!(0);
    conflict["conflicting_paths"] = json!([{"encoding":"hex","value":"ff6162"}]);
    conflict["conflict_path_count"] = json!(1);
    let task = peer(&h, conflict).await;
    let result = svc.create_candidate(r.clone()).await.unwrap();
    task.await.unwrap();
    assert!(result.outcome.unwrap().candidate.is_none());
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    let p = PublishRequest {
        context: context(&h),
        candidate_id: r.context.operation_id,
        target: "refs/heads/main".into(),
        expected_old: h.manifest.source.base.clone(),
    };
    assert!(matches!(
        svc.publish(p).await,
        Err(DeliveryError::InvalidInput(_))
    ));
    for value in [json!({"state":"ready"}), {
        let mut bad = ready(&h, r.context.operation_id);
        bad["state"] = json!("conflicted");
        bad["candidate"] = Value::Null;
        bad["conflict_step"] = json!(0);
        bad["conflicting_paths"] = json!([{"encoding":"hex","value":"not hex"}]);
        bad
    }] {
        let mut r = r.clone();
        r.context.operation_id = Uuid::new_v4();
        let task = peer(&h, value).await;
        assert!(matches!(
            svc.create_candidate(r).await,
            Err(DeliveryError::Protocol)
        ));
        task.await.unwrap();
        std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    }
    let mut huge = r;
    huge.context.operation_id = Uuid::new_v4();
    huge.commit.message = "x".repeat(1024 * 1024);
    assert!(matches!(
        svc.create_candidate(huge).await,
        Err(DeliveryError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn candidate_from_retained_source_publishes_once() {
    let h = Harness::new().await;
    let source = retain(&h).await;
    let svc = service(&h);
    let request = candidate(&h, source);
    let task = peer(&h, ready(&h, request.context.operation_id)).await;
    let candidate = svc.create_candidate(request.clone()).await.unwrap();
    task.await.unwrap();
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    let p = PublishRequest {
        context: context(&h),
        candidate_id: request.context.operation_id,
        target: "refs/heads/main".into(),
        expected_old: h.manifest.source.base.clone(),
    };
    let task = peer(&h, json!({"outcome":"applied","target":p.target,"old":p.expected_old,"new":h.manifest.checkpoint,"acknowledged_at":"2026-11-02T00:00:00Z"})).await;
    let published = svc.publish(p.clone()).await.unwrap();
    let sent = task.await.unwrap();
    assert_eq!(
        sent["params"]["candidate_id"],
        json!(request.context.operation_id)
    );
    assert!(matches!(
        published.outcome,
        Ok(PublishResult::Applied { .. })
    ));
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    // No peer exists: all concurrent retries and reconstructed services must use
    // the exact persisted receipts, including the original acknowledgement time.
    let restarted = service(&h);
    let (a, b) = tokio::join!(svc.publish(p.clone()), restarted.publish(p.clone()));
    assert_eq!(a.unwrap().raw, published.raw);
    assert_eq!(b.unwrap().raw, published.raw);
    assert_eq!(
        restarted.create_candidate(request).await.unwrap().raw,
        candidate.raw
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM delivery_publish_dispatches WHERE operation_id=$1",
    )
    .bind(p.context.operation_id)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_destinations")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(held, 0);
}

#[tokio::test]
async fn delivery_indeterminate_receipts_and_lost_replies_keep_destination_held() {
    for code in [
        Some("operation_indeterminate"),
        Some("remote_auth_failed"),
        None,
    ] {
        let h = Harness::new().await;
        let source = retain(&h).await;
        let svc = service(&h);
        let request = candidate(&h, source);
        let task = peer(&h, ready(&h, request.context.operation_id)).await;
        svc.create_candidate(request.clone()).await.unwrap();
        task.await.unwrap();
        std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
        let p = PublishRequest {
            context: context(&h),
            candidate_id: request.context.operation_id,
            target: "refs/heads/main".into(),
            expected_old: h.manifest.source.base.clone(),
        };
        let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
        let pool = h.pool.clone();
        let id = p.context.operation_id;
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let dispatch: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM delivery_publish_dispatches WHERE operation_id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(dispatch, 1, "dispatch must already be committed");
            if let Some(code) = code {
                let reply = json!({"protocol_version":1,"request_id":request["request_id"],"operation_id":id,"ok":false,"operation_state":"pending","error":{"code":code,"message":"unknown dispatched outcome","retry":"reconcile_first","details":{"dispatched":true,"remote_effect_unknown":true}}});
                reader
                    .get_mut()
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
            // None deliberately loses the acknowledgement after reading publish.
        });
        let result = svc.publish(p.clone()).await;
        task.await.unwrap();
        std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
        if let Some(code) = code {
            let receipt = result.unwrap();
            assert_eq!(receipt.outcome.as_ref().unwrap_err().code, code);
            assert_eq!(
                service(&h).publish(p.clone()).await.unwrap().raw,
                receipt.raw
            );
        } else {
            assert!(matches!(result, Err(DeliveryError::Indeterminate)));
            assert!(matches!(
                service(&h).publish(p.clone()).await,
                Err(DeliveryError::Indeterminate)
            ));
        }
        let mut next = p;
        next.context.operation_id = Uuid::new_v4();
        assert!(matches!(
            svc.publish(next).await,
            Err(DeliveryError::DestinationBlocked)
        ));
    }
}

#[tokio::test]
async fn delivery_publish_errors_require_conclusive_refusal_to_release_destination() {
    for (code, state, details, held) in [
        ("io_error", None, Value::Null, true),
        ("future_error", None, Value::Null, true),
        ("io_error", Some("pending"), Value::Null, true),
        ("io_error", Some("complete"), Value::Null, true),
        (
            "remote_auth_failed",
            Some("failed"),
            json!({"dispatched":true}),
            true,
        ),
        ("target_not_allowed", Some("failed"), Value::Null, false),
    ] {
        let h = Harness::new().await;
        let source = retain(&h).await;
        let svc = service(&h);
        let request = candidate(&h, source);
        let task = peer(&h, ready(&h, request.context.operation_id)).await;
        svc.create_candidate(request.clone()).await.unwrap();
        task.await.unwrap();
        std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
        let p = PublishRequest {
            context: context(&h),
            candidate_id: request.context.operation_id,
            target: "refs/heads/main".into(),
            expected_old: h.manifest.source.base.clone(),
        };
        let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
        let pool = h.pool.clone();
        let id = p.context.operation_id;
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let dispatch: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM delivery_publish_dispatches WHERE operation_id=$1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(dispatch, 1, "dispatch must already be committed");
            let mut reply = json!({"protocol_version":1,"request_id":request["request_id"],"operation_id":id,"ok":false,"error":{"code":code,"message":"No space left on device","details":details,"retry":"safe_same_operation"}});
            if let Some(state) = state {
                reply["operation_state"] = json!(state);
            }
            let raw = format!("{reply}\n").into_bytes();
            reader.get_mut().write_all(&raw).await.unwrap();
            raw
        });
        let receipt = svc.publish(p.clone()).await.unwrap();
        assert_eq!(receipt.raw, task.await.unwrap());
        assert_eq!(receipt.outcome.as_ref().unwrap_err().code, code);
        if state.is_none() {
            assert_eq!(receipt.operation_state, None, "outcome remains unknown");
        }
        std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
        // With no peer, a reconstructed service must replay the exact error
        // receipt without dispatching again, even with safe_same_operation.
        let restarted = service(&h);
        assert_eq!(restarted.publish(p.clone()).await.unwrap().raw, receipt.raw);
        let holds: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM delivery_destinations WHERE repository_id=$1 AND target=$2",
        )
        .bind(p.context.repository_id)
        .bind(&p.target)
        .fetch_one(&h.pool)
        .await
        .unwrap();
        assert_eq!(holds, i64::from(held), "code={code}, state={state:?}");
        let mut next = p;
        next.context.operation_id = Uuid::new_v4();
        if held {
            assert!(matches!(
                restarted.publish(next).await,
                Err(DeliveryError::DestinationBlocked)
            ));
        } else {
            let task = peer(
                &h,
                json!({"outcome":"rejected","reason":"stale expected old"}),
            )
            .await;
            assert!(matches!(
                restarted.publish(next).await.unwrap().outcome,
                Ok(PublishResult::Rejected { .. })
            ));
            task.await.unwrap();
        }
    }
}

#[tokio::test]
async fn delivery_candidate_retries_frozen_request_after_lost_reply() {
    let h = Harness::new().await;
    let source = retain(&h).await;
    let svc = service(&h);
    let request = candidate(&h, source);
    let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
    let lost = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line
    });
    assert!(svc.create_candidate(request.clone()).await.is_err());
    let original = lost.await.unwrap();
    std::fs::remove_file(h.dir.path().join("git.sock")).unwrap();
    // A retry uses its frozen intent, not newly resolved source inputs.
    sqlx::query("UPDATE source_transfers SET state='rejected' WHERE import_operation_id=$1")
        .bind(source)
        .execute(&h.pool)
        .await
        .unwrap();
    let task = peer(&h, ready(&h, request.context.operation_id)).await;
    service(&h).create_candidate(request).await.unwrap();
    assert_eq!(
        task.await.unwrap(),
        serde_json::from_str::<Value>(&original).unwrap()
    );
}

// On assertion failure, stop only this fixture's newly created process group.
// On success the guard gets SIGTERM and reaps its engine before disarming this.
struct GitFixtureGroup(Option<nix::unistd::Pid>);
impl Drop for GitFixtureGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
        }
    }
}
