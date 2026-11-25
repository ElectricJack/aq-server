use super::*;

// The successor's base from a legacy predecessor's bundle receipt.
#[tokio::test]
async fn handoff_successor_starts_from_saved_commit() {
    let h = handed(Some("owner_stop")).await;
    let pool = h.env.f.db.pool().await;
    let predecessor = h.env.report.binding.attempt_id;
    let task = h.env.f.selection.lineage.task_id;
    let base = admitted(&h.env).await.resolved_object.hex;
    let candidate = "c".repeat(40);
    let peer = git_peer(
        pool.clone(),
        h.socket.clone(),
        vec![ready_candidate(&candidate)],
    )
    .await;
    let found = handoff::successor_base(&h.env.f.state, task, &replacement_of(predecessor))
        .await
        .unwrap()
        .expect("a handed-off predecessor gives a saved base");
    let sent = peer.await.unwrap();
    // The successor's base is the candidate of the saved checkpoint on the
    // predecessor's admitted base, retained by the candidate handle.
    assert_eq!(found.oid, candidate);
    assert_ne!(found.oid, base);
    assert_eq!(found.predecessor, predecessor);
    assert_eq!(
        found.retention_handle,
        handoff::candidate_operation(predecessor)
    );
    assert_eq!(sent[0]["operation"], "candidate.create");
    assert_eq!(sent[0]["operation_id"], json!(found.retention_handle));
    assert_eq!(sent[0]["params"]["base"]["hex"], json!(base));
    assert_eq!(
        sent[0]["params"]["sources"][0]["oid"]["hex"],
        json!(h.env.checkpoint)
    );
    let row = sqlx::query("SELECT candidate_oid,candidate_operation_id,note_comment_id FROM attempt_handoffs WHERE attempt_id=$1")
        .bind(predecessor).fetch_one(&pool).await.unwrap();
    assert_eq!(
        row.get::<Option<String>, _>("candidate_oid"),
        Some(candidate.clone())
    );
    assert_eq!(
        row.get::<Option<Uuid>, _>("candidate_operation_id"),
        Some(found.retention_handle)
    );
    check_handoff_note(&h, &sent[0], &row).await;
    check_export_fence(&pool, &found, &base).await;
}

#[tokio::test]
async fn plain_failure_successor_keeps_predecessor_export() {
    let h = handed(None).await;
    let pool = h.env.f.db.pool().await;
    let predecessor = h.env.report.binding.attempt_id;
    let calls = silent_git(&h.socket);
    let task = h.env.f.selection.lineage.task_id;
    assert!(
        handoff::successor_base(&h.env.f.state, task, &replacement_of(predecessor))
            .await
            .unwrap()
            .is_none(),
        "no handoff row: the successor shares the predecessor's export"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no aq-git I/O"
    );
    assert!(comments(&h).await.is_empty());
    let mut conn = pool.acquire().await.unwrap();
    let request = export_request(&"c".repeat(40), handoff::candidate_operation(predecessor));
    assert!(
        !handoff::export_is_handoff(&mut conn, predecessor, &request)
            .await
            .unwrap()
    );
}

// The successor's base from a legacy predecessor's bundle receipt.
#[tokio::test]
async fn handoff_poll_retry_replays_candidate_and_single_comment() {
    let h = handed(Some("service_stop")).await;
    let pool = h.env.f.db.pool().await;
    let predecessor = h.env.report.binding.attempt_id;
    let task = h.env.f.selection.lineage.task_id;
    let candidate = "d".repeat(40);
    let peer = git_peer(
        pool.clone(),
        h.socket.clone(),
        vec![ready_candidate(&candidate)],
    )
    .await;
    let first = handoff::successor_base(&h.env.f.state, task, &replacement_of(predecessor))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.await.unwrap().len(), 1);
    // The retry replays the stored candidate receipt: no socket listens now.
    std::fs::remove_file(&h.socket).unwrap();
    let again = handoff::successor_base(&h.env.f.state, task, &replacement_of(predecessor))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (again.oid, again.retention_handle),
        (first.oid, first.retention_handle)
    );
    let intents: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM delivery_intents WHERE operation='candidate.create'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(intents, 1, "one candidate operation");
    assert_eq!(comments(&h).await.len(), 1, "one handoff note");
}

/// The successor's admission observes the saved commit through the candidate
/// handle, never the configured source, and the stored body validates.
#[tokio::test]
async fn handoff_observation_is_the_saved_commit() {
    let (f, request, dir) = setup().await;
    let pool = f.db.pool().await;
    let row = sqlx::query("SELECT revision,binding,binding_observation FROM project_execution_bindings WHERE project_id=$1").bind(request.project_id).fetch_one(&pool).await.unwrap();
    let project = FrozenProjectBinding {
        project: serde_json::from_value(json!({"revision":row.get::<i64,_>("revision"),"binding":row.get::<Value,_>("binding"),"validated_at_ms":NOW,"validation":row.get::<Value,_>("binding_observation")})).unwrap(),
        snapshot_digest: "synthetic".into(), worker_membership_digest: "synthetic".into(),
        binding_observation: serde_json::from_value(row.get("binding_observation")).unwrap(), node_id: f.node.node_id,
        environment: "synthetic".into(), role: "synthetic".into(), intelligence_profile: "synthetic".into(), execution_profile: "synthetic".into(),
    };
    let policy = f
        .state
        .config
        .profiled_execution
        .as_ref()
        .unwrap()
        .project_policy_record(request.project_id)
        .unwrap();
    let base = handoff::HandoffBase {
        oid: "e".repeat(40),
        retention_handle: Uuid::new_v4(),
        predecessor: Uuid::new_v4(),
    };
    let socket = &f.state.source_retention.as_ref().unwrap().config.git_socket;
    assert!(socket.starts_with(dir.path()));
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let (oid, handle, repository) = (
        base.oid.clone(),
        base.retention_handle,
        request.repository_id,
    );
    let generation = request.binding_generation;
    let peer = tokio::spawn(async move {
        for operation in ["inspect", "retention.inspect"] {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let q: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(q["operation"], operation, "no ref resolution or export");
            let result = if operation == "inspect" {
                assert_eq!(q["params"]["oid"]["hex"], oid);
                json!({"oid":{"algorithm":"sha1","hex":oid},"tree":{"algorithm":"sha1","hex":"b".repeat(40)}})
            } else {
                assert_eq!(q["params"]["handle_id"], handle.to_string());
                json!({"version":1,"id":handle,"repository_id":repository,"binding_generation":generation,"generation":1,"state":"active","refs":[{"name":format!("refs/aq-git/candidates/{handle}/prefix/0"),"oid":oid}],"release_operation":null,"release_reason":null})
            };
            let mut reply = serde_json::to_vec(&json!({"protocol_version":1,"request_id":q["request_id"],"ok":true,"result":result})).unwrap();
            reply.push(b'\n');
            reader.get_mut().write_all(&reply).await.unwrap();
        }
    });
    let observed = source::observe_handoff(&f.state, &project, &policy, &base)
        .await
        .unwrap();
    peer.await.unwrap();
    assert_eq!(observed.admission.resolved_object.hex, base.oid);
    assert_ne!(observed.admission.resolved_object.hex, request.base.hex);
    assert_eq!(observed.retention_handle, base.retention_handle);
    let body: Value = serde_json::from_slice(&observed.bytes).unwrap();
    assert_eq!(
        body["evidence"]["handoff"]["predecessor"],
        json!(base.predecessor)
    );
    source::validate_body(&observed.bytes, request.project_id, NOW).unwrap();
}

async fn check_handoff_note(h: &Handed, request: &Value, row: &sqlx::postgres::PgRow) {
    let notes = comments(h).await;
    assert_eq!(notes.len(), 1);
    // Review minor: worker-reported text is the predecessor agent's comment.
    let author = serde_json::to_value(&notes[0].author).unwrap();
    assert_eq!(author["kind"], "agent", "{author}");
    assert_eq!(
        author["attempt_id"],
        json!(h.env.report.binding.attempt_id),
        "{author}"
    );
    assert!(
        notes[0]
            .body
            .as_deref()
            .unwrap()
            .contains("Worker-reported conclusion:"),
        "{:?}",
        notes[0].body
    );
    assert!(
        request["params"]["commit"]["message"]
            .as_str()
            .unwrap()
            .contains("\n\nworker-reported: "),
        "{}",
        request
    );
    assert!(
        notes[0]
            .body
            .as_deref()
            .unwrap()
            .contains("Handoff from attempt"),
        "{:?}",
        notes[0].body
    );
    assert!(
        notes[0].body.as_deref().unwrap().contains("owner_stop"),
        "{:?}",
        notes[0].body
    );
    assert_eq!(
        row.get::<Option<Uuid>, _>("note_comment_id"),
        Some(notes[0].comment_id)
    );
}

async fn check_export_fence(pool: &sqlx::PgPool, found: &handoff::HandoffBase, base: &str) {
    let predecessor = found.predecessor;
    let candidate = &found.oid;
    // The publication fence accepts only the fresh export of that candidate.
    let mut conn = pool.acquire().await.unwrap();
    let fresh = export_request(candidate, found.retention_handle);
    assert!(
        handoff::export_is_handoff(&mut conn, predecessor, &fresh)
            .await
            .unwrap()
    );
    let original = export_request(base, found.retention_handle);
    assert!(
        !handoff::export_is_handoff(&mut conn, predecessor, &original)
            .await
            .unwrap()
    );
    let other = export_request(candidate, Uuid::new_v4());
    assert!(
        !handoff::export_is_handoff(&mut conn, predecessor, &other)
            .await
            .unwrap()
    );
}
