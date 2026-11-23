//! Spec 0007 phase 1 (S7, S8): a handed-off attempt's successor starts from
//! the saved commit, and owner/service handoffs are not charged to the lineage
//! budgets. Synthetic SQL owners and a synthetic central aq-git peer; no
//! provider and no real Git export.
use crate::profiled_results::auto_settlement56::{
    CheckedSource, ReportSetup, auto_env_source, evidence_rows, head_of,
};
use crate::profiled_results::auto_settlement56::{git_peer, ready_candidate};
use crate::{
    common::{self, profiled::NOW, source_exports::setup},
    profiled_results::auto_settlement56::{AutoEnv, admitted, auto_env_with, silent_git},
};
use aq_admission::transport::ProcessOutcome;
use aq_server::{
    profiled_execution::{handoff, replacement::Replacement, source, source_exports},
    project_bindings::domain::FrozenProjectBinding,
    source_retention::{Runtime, SourceRetentionConfig},
};
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

struct Handed {
    env: AutoEnv,
    socket: std::path::PathBuf,
    _git: tempfile::TempDir,
}
/// A recorded Failed report whose saved checkpoint is centrally retained,
/// with the central aq-git socket configured for delivery.
async fn handed(reason: Option<&str>) -> Handed {
    let mut env = Box::pin(auto_env_with(
        "not_required",
        axum::http::StatusCode::OK,
        false,
        Some(ProcessOutcome::Failed),
    ))
    .await;
    let git = tempfile::tempdir().unwrap();
    let socket = git.path().join("git.sock");
    env.f.state.source_retention = Some(Arc::new(
        Runtime::new(SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            staging_root: git.path().join("staging"),
            git_root: git.path().join("git"),
            git_socket: socket.clone(),
            max_staging_bytes: 2097152,
            max_bundle_bytes: 4096,
            bulk_concurrency: 2,
            incomplete_ttl_secs: 60,
            git_deadline_ms: 30_000,
        })
        .unwrap(),
    ));
    if let Some(reason) = reason {
        sqlx::query("INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) VALUES($1,$2,$3,now())")
            .bind(env.report.binding.attempt_id)
            .bind(env.f.node.node_id)
            .bind(reason)
            .execute(&env.f.db.pool().await)
            .await
            .unwrap();
    }
    Handed {
        env,
        socket,
        _git: git,
    }
}
fn replacement_of(predecessor: Uuid) -> Replacement {
    let artifact = |id: &str| common::profiled::artifact(id, id.as_bytes());
    Replacement {
        predecessor,
        attempts_started: 1,
        exempt_attempts: 0,
        elapsed_ms: 1,
        stop_receipt: artifact("stop"),
        retained_source_receipt: artifact("retained"),
        effect_reconciliation_receipt: artifact("effects"),
        needs_release: false,
    }
}
async fn comments(h: &Handed) -> Vec<aq_task_comments::Comment> {
    let thread = aq_task_comments::Thread {
        project_id: admitted(&h.env).await.project_id,
        task_id: h.env.f.selection.lineage.task_id,
    };
    aq_task_comments::list(&h.env.f.db.pool().await, thread, None, 50)
        .await
        .unwrap()
        .0
}
fn export_request(base: &str, handle: Uuid) -> source_exports::Request {
    serde_json::from_value(json!({"schema_version":1,"operation_id":Uuid::new_v4(),"project_id":Uuid::new_v4(),
        "project_revision":1,"repository_id":Uuid::new_v4(),"logical_identity":"synthetic","binding_generation":1,
        "base":{"algorithm":"sha1","hex":base},"source_retention_handle":handle,
        "source_observation":common::profiled::artifact("observation", b"observation")}))
    .unwrap()
}

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
    let notes = comments(&h).await;
    assert_eq!(notes.len(), 1);
    // Review minor: worker-reported text is the predecessor agent's comment.
    let author = serde_json::to_value(&notes[0].author).unwrap();
    assert_eq!(author["kind"], "agent", "{author}");
    assert_eq!(author["attempt_id"], json!(predecessor), "{author}");
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
        sent[0]["params"]["commit"]["message"]
            .as_str()
            .unwrap()
            .contains("\n\nworker-reported: "),
        "{}",
        sent[0]
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
    // The publication fence accepts only the fresh export of that candidate.
    let mut conn = pool.acquire().await.unwrap();
    let fresh = export_request(&candidate, found.retention_handle);
    assert!(
        handoff::export_is_handoff(&mut conn, predecessor, &fresh)
            .await
            .unwrap()
    );
    let original = export_request(&base, found.retention_handle);
    assert!(
        !handoff::export_is_handoff(&mut conn, predecessor, &original)
            .await
            .unwrap()
    );
    let other = export_request(&candidate, Uuid::new_v4());
    assert!(
        !handoff::export_is_handoff(&mut conn, predecessor, &other)
            .await
            .unwrap()
    );
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

/// The next attempt of the same task: a new attempt, session, offer and
/// generation on the fixture's offer (as `replacement_source` does).
fn next_attempt(f: &mut common::profiled::Fixture, generation: u64) {
    let attempt = Uuid::new_v4();
    let session = aq_session_protocol::SessionIdentity::Remote {
        node_id: f.node.node_id,
        boot_id: f.node.boot_id,
        attempt_id: attempt,
        assignment_generation: generation,
        session_id: Uuid::new_v4(),
    };
    let offer = &mut f.profiled.offer;
    offer.binding.session = session.clone();
    offer.binding.attempt_id = attempt;
    offer.binding.assignment_generation = generation;
    offer.binding.offer_id = Uuid::new_v4();
    offer.manifest.bindings.session = session;
    offer.manifest.bindings.attempt_id = attempt;
    offer.manifest.bindings.assignment_generation = generation;
    offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&offer.manifest).unwrap();
    f.decision = Uuid::new_v4();
}

/// Spec 0007 Q1 (Review Focus 6): owner and service handoffs are not charged
/// to the B4 retry budget; a crash restart is.
#[tokio::test]
async fn owner_stop_handoffs_do_not_exhaust_the_retry_budget() {
    use crate::replacement_release::{fail, granted_at};
    use aq_server::profiled_execution::replacement::{self, Assessment};
    let mut f = common::profiled::fixture().await;
    let task = f.selection.lineage.task_id;
    let pool = f.db.pool().await;
    let mut attempts = vec![];
    for generation in 1..=3 {
        if generation > 1 {
            next_attempt(&mut f, generation);
        }
        let (attempt, _, mut hot) = granted_at(&f, NOW - 10_000 + generation * 1_000).await;
        fail(&pool, attempt, &mut hot).await;
        sqlx::query("INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) VALUES($1,$2,'owner_stop',now())")
            .bind(attempt).bind(f.node.node_id).execute(&pool).await.unwrap();
        attempts.push(attempt);
        if generation < 3 {
            let assessed = replacement::assess_task(&f.state, task, NOW).await.unwrap();
            let Assessment::Replace(r) = assessed else {
                panic!("attempt {generation} is replaceable: {assessed:?}");
            };
            replacement::release(&f.state, task, &r).await.unwrap();
        }
    }
    let assess = || replacement::assess_task(&f.state, task, NOW);
    let reason = |attempt: Uuid, reason: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query("UPDATE attempt_handoffs SET reason=$2 WHERE attempt_id=$1")
                .bind(attempt)
                .bind(reason)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    // Three owner stops: none is charged, the lineage is still replaceable.
    let assessed = assess().await.unwrap();
    let Assessment::Replace(r) = assessed else {
        panic!("owner stops never exhaust the retry budget: {assessed:?}");
    };
    assert_eq!((r.attempts_started, r.exempt_attempts), (3, 3));
    // A service stop is exempt as well; a crash restart is charged.
    reason(attempts[1], "service_stop").await;
    reason(attempts[0], "restart").await;
    let assessed = assess().await.unwrap();
    let Assessment::Replace(r) = assessed else {
        panic!("one charged crash restart stays within the budget: {assessed:?}");
    };
    assert_eq!((r.attempts_started, r.exempt_attempts), (3, 2));
    // Three crashes are charged in full and exhaust the budget.
    reason(attempts[1], "restart").await;
    reason(attempts[2], "restart").await;
    assert_eq!(
        assess().await.unwrap(),
        Assessment::Blocked("lineage_retry_budget_exhausted")
    );
}

/// Review I1(c): only the first exempt handoffs of a lineage are free, up to
/// Settings › Selection defaults `max_uncharged_continuations`; later owner
/// stops are charged as normal attempts against `max_attempts`. The fixture's
/// attempts pin no policy, so the current Settings values apply.
#[tokio::test]
async fn exempt_handoffs_are_capped_at_five_per_lineage() {
    use crate::replacement_release::{fail, granted_at};
    use aq_server::profiled_execution::replacement::{self, Assessment};
    let mut f = common::profiled::fixture().await;
    let task = f.selection.lineage.task_id;
    let pool = f.db.pool().await;
    let limits = aq_profiles::roles::SelectionDefaults::default();
    let (cap, last) = (
        u64::from(limits.max_uncharged_continuations),
        u64::from(limits.max_uncharged_continuations + limits.max_attempts),
    );
    for generation in 1..=last {
        if generation > 1 {
            next_attempt(&mut f, generation);
        }
        let (attempt, _, mut hot) = granted_at(&f, NOW - 10_000 + generation * 1_000).await;
        fail(&pool, attempt, &mut hot).await;
        sqlx::query("INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) VALUES($1,$2,'owner_stop',now())")
            .bind(attempt).bind(f.node.node_id).execute(&pool).await.unwrap();
        let assessed = replacement::assess_task(&f.state, task, NOW).await.unwrap();
        let Assessment::Replace(r) = assessed else {
            assert_eq!(generation, last, "{assessed:?}");
            assert_eq!(
                assessed,
                Assessment::Blocked("lineage_retry_budget_exhausted")
            );
            return;
        };
        let exempt = generation.min(cap) as u32;
        assert_eq!(
            (r.attempts_started, r.exempt_attempts),
            (generation as u32, exempt)
        );
        replacement::release(&f.state, task, &r).await.unwrap();
    }
    panic!("the last owner stop leaves `max_attempts` charged attempts");
}

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
    let git = tempfile::tempdir().unwrap();
    let socket = git.path().join("git.sock");
    let config = SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: git.path().join("staging"),
        git_root: git.path().join("git"),
        git_socket: socket.clone(),
        max_staging_bytes: 2097152,
        max_bundle_bytes: 4096,
        bulk_concurrency: 2,
        incomplete_ttl_secs: 60,
        git_deadline_ms: 30_000,
    };
    let delivery = aq_server::delivery::DeliveryService::new(pool, &config.git_socket);
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
