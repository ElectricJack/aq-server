use super::*;

#[tokio::test]
async fn retained_result_publishes_then_completes_and_advances_source() {
    use aq_server::profiled_execution::settlement::publication;
    let p = publish_env().await;
    let env = &p.env;
    let pool = env.f.db.pool().await;
    let base = admitted(env).await.resolved_object.hex;
    let candidate = "c".repeat(40);
    let peer = git_peer(
        pool.clone(),
        p.socket.clone(),
        vec![
            ready_candidate(&candidate),
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"applied","target":"refs/heads/main",
                "old":{"algorithm":"sha1","hex":base},"new":{"algorithm":"sha1","hex":candidate},
                "acknowledged_at":"2026-11-05T00:00:00Z"}}),
        ],
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    let sent = peer.await.unwrap();
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    assert_eq!(s.evidence["delivery"]["state"], "published", "{s:?}");
    assert_eq!(env.writes.load(Ordering::SeqCst), 1, "one Graph completion");
    assert!(
        !env.holds_task().await,
        "dependents released only after publication"
    );
    check_publication_requests(&sent, &base, &env.checkpoint);
    // The project's source head advanced to the published commit, retained by
    // the candidate handle.
    let source = &admitted(env).await;
    let head = publication::current_head(
        &pool,
        source.project_id,
        source.repository_id,
        source.repository_binding_generation,
        "refs/heads/main",
    )
    .await
    .unwrap()
    .expect("source head");
    assert_eq!(head.oid, candidate);
    assert_eq!(head.previous, base);
    assert_eq!(head.sequence, 1);
    assert_eq!(json!(head.retention_handle), sent[0]["operation_id"]);
    // Replay is idempotent and does no aq-git I/O (no socket listens now).
    std::fs::remove_file(&p.socket).unwrap();
    let again = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.disposition, AutoDisposition::Confirmed);
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
}

/// Owner decision 2026-11-13: an attempt that finishes successfully without
/// changing a file succeeds. Its retained checkpoint is the admitted base, so
/// required delivery has nothing to publish: no candidate, no push, no
/// delivery evidence, and the task completes.
#[tokio::test]
async fn unchanged_source_completes_without_publication() {
    use aq_server::profiled_execution::settlement::publication;
    let env = Box::pin(auto_env_source(
        "required",
        axum::http::StatusCode::OK,
        ReportSetup {
            changed: false,
            ..ReportSetup::default()
        },
    ))
    .await;
    assert_eq!(env.checkpoint, admitted(&env).await.resolved_object.hex);
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &checkpoint,
        },
        &validator,
        "passed",
    )
    .await;
    let pool = env.f.db.pool().await;
    let project = admitted(&env).await.project_id;
    sqlx::query("UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{publication}',$2) WHERE project_id=$1")
        .bind(project)
        .bind(json!({"mode":"fast_forward","target":"refs/heads/main"}))
        .execute(&pool)
        .await
        .unwrap();
    let git = tempfile::tempdir().unwrap();
    let socket = git.path().join("git.sock");
    let calls = silent_git(&socket);
    let config = aq_server::source_retention::SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: git.path().join("staging"),
        git_root: git.path().join("git"),
        git_socket: socket,
        max_staging_bytes: 2097152,
        max_bundle_bytes: 4096,
        bulk_concurrency: 2,
        incomplete_ttl_secs: 60,
        git_deadline_ms: 30_000,
    };
    let delivery = aq_server::delivery::DeliveryService::new(pool.clone(), &config.git_socket);
    let settling = env.report.binding.attempt_id;
    common::roles::pin_active(&pool, settling).await;
    let s = publication::deliver_then_settle(&env.f.state, &delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    // Spec 0014 §7.1: the snapshot pin is released with the attempt's hold on its task.
    assert!(!common::roles::pinned(&pool, settling).await);
    check_unchanged_evidence(&s);
    assert_eq!(env.writes.load(Ordering::SeqCst), 1, "one Graph completion");
    assert!(!env.holds_task().await, "dependents are released");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing sent to aq-git");
    assert!(evidence_rows(&env).await.is_empty(), "no delivery evidence");
    assert!(head_of(&env).await.is_none(), "the source head never moves");
}

fn check_publication_requests(sent: &[Value], base: &str, checkpoint: &str) {
    // Server-owned inputs only: the retained receipt, the admitted base and the policy target.
    assert_eq!(sent[0]["operation"], "candidate.create");
    assert_eq!(sent[0]["params"]["base"]["hex"], json!(base));
    assert_eq!(
        sent[0]["params"]["sources"][0]["oid"]["hex"],
        json!(checkpoint)
    );
    check_published_candidate(&sent[1], &sent[0], base);
}

fn check_unchanged_evidence(s: &AutoSettlement) {
    assert_eq!(s.evidence["delivery"]["state"], "no_change", "{s:?}");
    assert_eq!(s.evidence["source"]["changed"], false, "{s:?}");
}

fn check_published_candidate(publish: &Value, create: &Value, base: &str) {
    assert_eq!(publish["operation"], "publish");
    assert_eq!(publish["params"]["target"], "refs/heads/main");
    assert_eq!(publish["params"]["expected_old"]["hex"], json!(base));
    assert_eq!(publish["params"]["candidate_id"], create["operation_id"]);
}
