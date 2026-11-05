use super::*;
use aq_server::source_retention::{Runtime, SourceRetentionConfig, gc::Sweep};
use aq_worker_protocol::TransferManifest;
use sqlx::Row;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Actual acceptance/completion owners, with a fake local aq-git peer. No Git
/// processes, large bundles, elapsed-time sleeps or external services.
#[tokio::test]
async fn source_retention_gc_acceptance_and_max_age() {
    let c = Box::pin(setup()).await;
    let pool = c.env.f.db.pool().await;
    let original: Value =
        sqlx::query_scalar("SELECT manifest FROM source_transfers WHERE import_operation_id=$1")
            .bind(c.env.operation)
            .fetch_one(&pool)
            .await
            .unwrap();
    for index in 1..5 {
        let mut manifest: TransferManifest = serde_json::from_value(original.clone()).unwrap();
        manifest.transfer_id = Uuid::new_v4();
        manifest.artifact_id = Uuid::new_v4();
        sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,artifact_id,import_operation_id,manifest,state,receipt,central_receipt,created_at) SELECT $2,attempt_id,node_id,repository_id,$3,$4,$5,'retained',receipt,central_receipt,created_at+($6::bigint * interval '1 second') FROM source_transfers WHERE import_operation_id=$1")
            .bind(c.env.operation).bind(manifest.transfer_id).bind(manifest.artifact_id)
            .bind(Uuid::new_v4()).bind(json!(manifest)).bind(index as i64)
            .execute(&pool).await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let config: SourceRetentionConfig = toml::from_str(&format!(
        "staging_root='{}'\ngit_root='{}'\ngit_socket='{}'",
        dir.path().join("staging").display(),
        dir.path().join("git").display(),
        dir.path().join("git.sock").display(),
    ))
    .unwrap();
    let mut state = c.env.f.state.clone();
    state.source_retention = Some(Arc::new(Runtime::new(config.clone()).unwrap()));
    let listener = tokio::net::UnixListener::bind(&config.git_socket).unwrap();
    let manifests: Vec<Value> = sqlx::query_scalar("SELECT manifest FROM source_transfers")
        .fetch_all(&pool)
        .await
        .unwrap();
    let mut handles = std::collections::HashMap::new();
    for manifest in manifests {
        let m: TransferManifest = serde_json::from_value(manifest).unwrap();
        handles.insert(m.artifact_id.to_string(), json!({"version":1,"id":m.artifact_id,
            "repository_id":m.source.repository_id,"binding_generation":m.source.binding_generation,
            "generation":7,"resource":"transfer","state":"active",
            "refs":[{"name":format!("refs/aq-git/transfers/{}",m.artifact_id),"oid":m.checkpoint.hex}],
            "release_operation":null,"release_reason":null}));
    }
    let requests = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let recorded = requests.clone();
    let peer_pool = pool.clone();
    let peer = tokio::spawn(async move {
        let mut replies = std::collections::HashMap::<String, Value>::new();
        let mut lose_first_release = true;
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(socket);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            recorded.lock().unwrap().push(req.clone());
            let handle = req["params"]["handle_id"].as_str().unwrap();
            let mut reply = json!({"protocol_version":1,"request_id":req["request_id"],"ok":true});
            if req["operation"] == "retention.inspect" {
                reply["result"] = handles[handle].clone();
            } else {
                assert_eq!(req["operation"], "retention.release");
                let op = req["operation_id"].as_str().unwrap().to_string();
                let stored: Value = sqlx::query_scalar(
                    "SELECT request FROM source_retention_releases WHERE operation_id=$1",
                )
                .bind(Uuid::parse_str(&op).unwrap())
                .fetch_one(&peer_pool)
                .await
                .unwrap();
                assert_eq!(stored, req, "intent is committed before aq-git dispatch");
                assert_eq!(req["params"]["expected_generation"], 7);
                assert_eq!(req["params"]["refs"], handles[handle]["refs"]);
                let result = replies.entry(op).or_insert_with(|| {
                    let record = handles.get_mut(handle).unwrap();
                    assert_eq!(record["state"], "active");
                    record["state"] = json!("released");
                    record["generation"] = json!(9);
                    record["release_operation"] = req["operation_id"].clone();
                    record["release_reason"] = req["params"]["reason"].clone();
                    record.clone()
                });
                reply["result"] = result.clone();
                reply["operation_id"] = req["operation_id"].clone();
                reply["operation_state"] = json!("complete");
                if lose_first_release {
                    lose_first_release = false;
                    continue; // Release succeeded; its response was lost.
                }
            }
            let mut bytes = serde_json::to_vec(&reply).unwrap();
            bytes.push(b'\n');
            stream.get_mut().write_all(&bytes).await.unwrap();
        }
    });

    assert_eq!(Sweep::default().tick(&state).await.unwrap(), 0);
    assert!(
        requests.lock().unwrap().is_empty(),
        "open task source is untouched"
    );
    let accepted = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    assert_eq!(accepted.disposition, EvaluationDisposition::Accepted);
    assert_eq!(Sweep::default().tick(&state).await.unwrap(), 0);
    assert!(
        requests.lock().unwrap().is_empty(),
        "uncertain completion remains protected"
    );
    let done = Box::pin(
        c.service
            .complete(&c.actor, accepted.completion_operation_id.unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(done.state, "confirmed");

    assert_eq!(Sweep::default().tick(&state).await.unwrap(), 0); // lost response
    assert_eq!(Sweep::default().tick(&state).await.unwrap(), 3);
    let retained: i64 = sqlx::query_scalar("SELECT count(*) FROM source_transfers t LEFT JOIN source_retention_releases r USING(transfer_id) WHERE r.released_at IS NULL")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(retained, config.keep_per_task as i64);
    let newest: Vec<bool> = sqlx::query_scalar("SELECT r.released_at IS NULL FROM source_transfers t LEFT JOIN source_retention_releases r USING(transfer_id) ORDER BY t.created_at DESC,t.transfer_id DESC LIMIT 2")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(newest, vec![true, true]);
    {
        let all = requests.lock().unwrap();
        let releases: Vec<_> = all
            .iter()
            .filter(|r| r["operation"] == "retention.release")
            .collect();
        assert_eq!(releases.len(), 4);
        assert_eq!(
            releases[0], releases[1],
            "uncertainty replays the same exact operation"
        );
    }
    let settled_at: i64 = sqlx::query_scalar(
        "SELECT max(created_at_ms) FROM profiled_task_completions WHERE state='confirmed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    state.clock = Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(settled_at + config.max_age as i64 * 1000 + 1)
            .unwrap()
    });
    assert_eq!(Sweep::default().tick(&state).await.unwrap(), 2);
    assert_eq!(Sweep::default().tick(&state).await.unwrap(), 0);
    let audit = sqlx::query("SELECT reason,handle_generation,response FROM source_retention_releases WHERE released_at IS NOT NULL")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(audit.len(), 5);
    assert_eq!(
        audit
            .iter()
            .filter(|r| r.get::<String, _>("reason") == "source_retention.max_age")
            .count(),
        2
    );
    for row in audit {
        assert_eq!(row.get::<i64, _>("handle_generation"), 7);
        assert_eq!(row.get::<Value, _>("response")["state"], "released");
    }
    peer.abort();
    c.env.peer.abort();
    c._graph.abort();
}
