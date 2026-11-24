use super::*;

/// A Success report of a handoff successor that changed nothing itself: its
/// retained checkpoint is its admitted base, the predecessor's saved commit
/// (handoff candidate). The predecessor attempt, its handoff row and the
/// candidate intent (whose frozen `base` is the predecessor's own admitted
/// base, `lineage_base`) are synthetic rows; delivery is required and the
/// project publishes by fast-forward to `refs/heads/main`.
/// `chain`: each handoff's frozen base, newest first (empty: the predecessor
/// changed nothing, so its candidate is its own base).
async fn quiet_successor(
    chain: &[&str],
) -> (
    AutoEnv,
    aq_server::delivery::DeliveryService,
    std::path::PathBuf,
    tempfile::TempDir,
) {
    let env = Box::pin(auto_env_source(
        "required",
        axum::http::StatusCode::OK,
        ReportSetup {
            changed: false,
            ..ReportSetup::default()
        },
    ))
    .await;
    let saved = admitted(&env).await.resolved_object.hex;
    assert_eq!(
        env.checkpoint, saved,
        "the successor changed nothing itself"
    );
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
    let source = admitted(&env).await;
    sqlx::query("UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{publication}',$2) WHERE project_id=$1")
        .bind(source.project_id)
        .bind(json!({"mode":"fast_forward","target":"refs/heads/main"}))
        .execute(&pool)
        .await
        .unwrap();
    seed_handoff_chain(&env, &pool, &source, chain).await;
    let git = tempfile::tempdir().unwrap();
    let socket = git.path().join("git.sock");
    let delivery = aq_server::delivery::DeliveryService::new(pool, &socket);
    (env, delivery, socket, git)
}

/// Review C-1: "no change" is judged against the lineage's original base. A
/// successor that only verified its predecessor's saved edits is delivered:
/// the candidate is built on the lineage base and published over it.
#[tokio::test]
async fn a_quiet_handoff_successor_publishes_the_predecessors_edits() {
    use aq_server::profiled_execution::settlement::{AutoDisposition, publication};
    let lineage = "1".repeat(40);
    let (env, delivery, socket, _git) = quiet_successor(&[&lineage]).await;
    let saved = env.checkpoint.clone();
    let pool = env.f.db.pool().await;
    let peer = git_peer(
        pool,
        socket,
        vec![
            ready_candidate(&saved),
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"applied","target":"refs/heads/main",
                "old":{"algorithm":"sha1","hex":lineage},"new":{"algorithm":"sha1","hex":saved},
                "acknowledged_at":"2026-11-05T00:00:00Z"}}),
        ],
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    assert_eq!(s.evidence["source"]["changed"], true, "{s:?}");
    assert_eq!(s.evidence["delivery"]["state"], "published", "{s:?}");
    let sent = peer.await.unwrap();
    assert_eq!(sent[0]["params"]["base"]["hex"], json!(lineage));
    assert_eq!(sent[0]["params"]["sources"][0]["oid"]["hex"], json!(saved));
    assert_eq!(sent[1]["params"]["expected_old"]["hex"], json!(lineage));
    let head = head_of(&env).await.expect("the source head moved");
    assert_eq!((head.oid, head.previous), (saved, lineage));
}

/// No edits anywhere in the lineage (the predecessor's saved commit is the
/// lineage base): the successor settles `no_change` and nothing is sent.
#[tokio::test]
async fn a_lineage_without_edits_settles_no_change() {
    use aq_server::profiled_execution::settlement::{AutoDisposition, publication};
    let (env, delivery, socket, _git) = quiet_successor(&[]).await;
    let calls = silent_git(&socket);
    let s = publication::deliver_then_settle(&env.f.state, &delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    assert_eq!(s.evidence["delivery"]["state"], "no_change", "{s:?}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(evidence_rows(&env).await.is_empty());
    assert!(head_of(&env).await.is_none());
}

/// Review R1-M4: two handoffs (C2 on C1 on B0) resolve to B0, so the quiet
/// third attempt publishes over B0.
#[tokio::test]
async fn a_two_hop_handoff_chain_publishes_over_the_first_base() {
    use aq_server::profiled_execution::settlement::{AutoDisposition, publication};
    let (middle, first) = ("2".repeat(40), "1".repeat(40));
    let (env, delivery, socket, _git) = quiet_successor(&[&middle, &first]).await;
    let saved = env.checkpoint.clone();
    let peer = git_peer(
        env.f.db.pool().await,
        socket,
        vec![
            ready_candidate(&saved),
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"applied","target":"refs/heads/main",
                "old":{"algorithm":"sha1","hex":first},"new":{"algorithm":"sha1","hex":saved},
                "acknowledged_at":"2026-11-05T00:00:00Z"}}),
        ],
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    let sent = peer.await.unwrap();
    assert_eq!(sent[0]["params"]["base"]["hex"], json!(first));
    assert_eq!(sent[1]["params"]["expected_old"]["hex"], json!(first));
}

/// Review R1-M1: a lineage longer than the hop bound is held visibly, never
/// settled against an intermediate commit.
#[tokio::test]
async fn a_lineage_past_the_hop_bound_is_held() {
    use aq_server::profiled_execution::settlement::{AutoDisposition, publication};
    let hops: Vec<String> = (0..=aq_machine_control::MAX_LINEAGE_HOPS)
        .map(|i| format!("{:040x}", i + 1))
        .collect();
    let chain: Vec<&str> = hops.iter().map(String::as_str).collect();
    let (env, delivery, socket, _git) = quiet_successor(&chain).await;
    let calls = silent_git(&socket);
    let s = publication::deliver_then_settle(&env.f.state, &delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.disposition, AutoDisposition::Held, "{s:?}");
    assert_eq!(
        s.reason.as_deref(),
        Some("lineage_history_unknown"),
        "{s:?}"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(env.holds_task().await);
}

async fn seed_handoff_chain(
    env: &AutoEnv,
    pool: &sqlx::PgPool,
    source: &aq_admission::SourceAdmission,
    chain: &[&str],
) {
    let saved = &env.checkpoint;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    let chain = if chain.is_empty() {
        vec![saved.as_str()]
    } else {
        chain.to_vec()
    };
    let mut candidate = saved.clone();
    for lineage_base in chain {
        let predecessor = Uuid::new_v4();
        let operation = handoff::candidate_operation(predecessor);
        sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state,holds_task) SELECT $1,task_id,node_id,$2,$3,kind,state,false FROM execution_attempts WHERE attempt_id=$4")
        .bind(predecessor)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(env.report.binding.attempt_id)
        .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at,candidate_operation_id,candidate_oid) VALUES($1,$2,'owner_stop',now(),$3,$4)")
        .bind(predecessor).bind(env.f.node.node_id).bind(operation).bind(&candidate).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO delivery_intents(operation_id,repository_id,binding_generation,operation,inputs,input_digest,payload_digest,request) VALUES($1,$2,$3,'candidate.create',$4,$5,$5,'\\x7b7d')")
        .bind(operation).bind(source.repository_id).bind(source.repository_binding_generation as i64)
        .bind(json!({"base":{"algorithm":"sha1","hex":lineage_base}})).bind("a".repeat(64))
        .execute(&mut *tx).await.unwrap();
        candidate = lineage_base.to_owned();
    }
    tx.commit().await.unwrap();
}
