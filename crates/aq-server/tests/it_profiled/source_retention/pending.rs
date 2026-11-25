use super::*;
pub(super) async fn assert_unconfirmed_offer(h: &Harness) {
    let state: String =
        sqlx::query_scalar("SELECT state FROM source_transfers WHERE transfer_id=$1")
            .bind(h.manifest.transfer_id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(state, "import_pending");
    let status = sqlx::query_scalar::<_, String>(
        "SELECT state::text FROM execution_attempts WHERE attempt_id=$1",
    )
    .bind(h.offer.attempt.attempt_id)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert!(status.contains("offered"));
}
pub(super) async fn check_cleanup_preserves_pending(h: &Harness) {
    sqlx::query(
        "UPDATE source_transfers SET updated_at=now()-interval '10 days' WHERE transfer_id=$1",
    )
    .bind(h.manifest.transfer_id)
    .execute(&h.pool)
    .await
    .unwrap();
    let mut next = h.begin_body(Uuid::new_v4());
    next["manifest"]["transfer_id"] = json!(Uuid::new_v4());
    next["manifest"]["artifact_id"] = json!(Uuid::new_v4());
    assert_eq!(
        h.json("/v1/execution/sources/begin", next).await.0,
        StatusCode::OK
    );
    assert!(
        h.dir
            .path()
            .join("git/artifacts/incoming")
            .join(h.manifest.artifact_id.to_string())
            .exists(),
        "TTL cleanup must not remove protected import bytes"
    );
}
pub(super) fn paused_retry_peer(
    h: &Harness,
) -> (
    Arc<tokio::sync::Semaphore>,
    Arc<tokio::sync::Semaphore>,
    tokio::task::JoinHandle<()>,
) {
    let native: CanonicalManifest = serde_json::from_slice(&h.bytes).unwrap();
    let canonical = hash(&serde_json::to_vec(&native).unwrap());
    let acknowledgement = json!({"receiver_installation_id":Uuid::new_v4(),"repository_id":h.manifest.source.repository_id,"binding_generation":1,"transfer_id":h.manifest.artifact_id,"source":h.manifest.checkpoint,"retention_handle":h.manifest.artifact_id,"manifest_digest":canonical,"retention_state":"active","historical":false});
    let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let resume = Arc::new(tokio::sync::Semaphore::new(0));
    let signal = entered.clone();
    let go = resume.clone();
    let peer = tokio::spawn(async move {
        let mut operations = vec![];
        for index in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            operations.push(request["operation_id"].clone());
            if index == 0 {
                signal.add_permits(1);
                go.acquire().await.unwrap().forget();
            }
            let reply = json!({"protocol_version":1,"request_id":request["request_id"],"operation_id":request["operation_id"],"ok":true,"result":acknowledgement});
            reader
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        assert_eq!(
            operations[0], operations[1],
            "recovery must reuse admitted import identity"
        );
    });
    (entered, resume, peer)
}

#[tokio::test]
async fn slow_import_does_not_hold_attempt_or_node_locks_and_control_status_progresses() {
    let h = Harness::new().await;
    h.upload_manifest().await;
    let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let go = release.clone();
    let socket = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        signal.notify_one();
        go.notified().await;
        let r: Value = serde_json::from_str(&line).unwrap();
        reader.get_mut().write_all(format!("{}\n",json!({"protocol_version":2,"request_id":r["request_id"],"operation_id":r["operation_id"],"ok":true,"result":{}})).as_bytes()).await.unwrap();
    });
    let pending = h.finalize();
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let (code, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        h.json("/v1/execution/sources/status", h.status_body()),
    )
    .await
    .expect("control status must progress during import");
    assert_eq!(code, StatusCode::OK);
    check_cleanup_preserves_pending(&h).await;
    tokio::time::timeout(std::time::Duration::from_secs(1),sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{status}','\"cancelling\"'::jsonb) WHERE attempt_id=$1").bind(h.offer.attempt.attempt_id).execute(&h.pool)).await.expect("cancellation row must not be locked by socket I/O").unwrap();
    let (code, body) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        h.json(
            &format!("/api/workers/{}/revoke", h.identity.node_id),
            json!({}),
        ),
    )
    .await
    .expect("node revocation must progress during import");
    assert_eq!(code, StatusCode::NO_CONTENT, "{body}");
    release.notify_one();
    socket.await.unwrap();
    assert_eq!(pending.await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.json("/v1/execution/sources/status", h.status_body())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}
