use super::*;

/// At most one agent run per host, so one of the two slots always
/// serves answer tasks: a second agent task waits, an answer task runs.
#[tokio::test]
async fn only_one_agent_run_holds_a_slot_and_answer_runs_keep_the_other() {
    let f = Fixture::with_claude("exit 1", &claude_agent_hangs()).await;
    let long = Request {
        deadline_ms: 1_800_000,
        ..agent_request()
    };
    let first = f.submit_request(&long, &[claude_candidate()]).await;
    let second = f.submit_request(&long, &[claude_candidate()]).await;
    let host = serving(&f);
    f.started().await;
    let judgment = f.submit(&[claude_candidate()]).await;
    let row = answered_within(&f, judgment, 10).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (first, second) = (f.row(first).await, f.row(second).await);
    let agents = std::fs::read_to_string(f.rec("agents")).unwrap();
    host.abort();
    assert_eq!(row.attempts.len(), 1);
    assert_eq!(first.status, "running");
    assert_eq!(
        (second.status.as_str(), second.attempts.len()),
        ("queued", 0)
    );
    assert_eq!(agents, "x\n", "a second agent run started");
}

/// A host whose Codex sandbox cannot start (the probe fails) passes a
/// Codex agent candidate on at once, and the next candidate answers.
#[tokio::test]
async fn a_codex_agent_candidate_moves_on_at_once_when_the_sandbox_cannot_start() {
    let claude = format!(
        "{}\n{}",
        saving("claude"),
        claude_printing(&claude_result(false, "done"), 0)
    );
    let f = Fixture::with_claude(&saving("codex"), &claude).await;
    std::fs::write(f.rec("no-sandbox"), "").unwrap();
    let id = f
        .submit_request(&agent_request(), &[codex_candidate(), claude_candidate()])
        .await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.attempts.len(), 1, "{:?}", row.attempts);
    assert_eq!(
        row.attempts[0].reason,
        Some(aq_internal_work::Reason::ProviderUnavailable)
    );
    assert_eq!(
        row.attempts[0].detail,
        "codex sandbox cannot start on this host"
    );
    assert!(row.attempts[0].elapsed_ms < 5_000);
    assert!(!f.rec("codex.tree").exists(), "the Codex agent ran");
    // The probe is not repeated for the same native.
    std::fs::remove_file(f.rec("no-sandbox")).unwrap();
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
}

/// The owner's requirement on the Codex lock: a blocking 60 s judgment
/// queued while a 30 minute Codex agent run holds the login skips Codex
/// at once and is answered by Claude.
#[tokio::test]
async fn a_blocking_judgment_during_a_long_codex_agent_run_is_answered_by_claude() {
    let hanging = "touch \"$rec/started\"\nsleep 30";
    let answers = claude_printing(&claude_result(false, ANSWER), 0);
    let f = Fixture::with_claude(hanging, &answers).await;
    let long = Request {
        deadline_ms: 1_800_000,
        ..agent_request()
    };
    let agent = f.submit_request(&long, &[codex_candidate()]).await;
    let host = serving(&f);
    f.started().await;
    let judgment = f.submit(&[codex_candidate(), claude_candidate()]).await;
    let row = answered_within(&f, judgment, 10).await;
    let agent = f.row(agent).await;
    host.abort();
    assert_eq!(
        row.attempts[0].reason,
        Some(aq_internal_work::Reason::LoginBusy)
    );
    assert_eq!(row.attempts[1].provider, "anthropic");
    assert_eq!(agent.status, "running");
}

/// A Codex agent run would hold the login for up to 30 minutes, so it
/// is not started while a blocking task that ends on Codex waits (here
/// on an API candidate first); once that task has had Codex, it runs.
#[tokio::test]
async fn a_codex_agent_run_waits_while_a_blocking_codex_only_task_is_queued() {
    let behaviour = format!(
        "case \" $* \" in *\" workspace-write \"*) {};; *) {};; esac",
        saving("codex"),
        answering()
    );
    let f = Fixture::new(&behaviour).await;
    let agent = f
        .submit_request(&agent_request(), &[codex_candidate()])
        .await;
    let blocking = f.submit(&[api_candidate(), codex_candidate()]).await;
    assert!(!f.tick().await, "the agent task was claimed");
    assert_eq!(f.row(agent).await.status, "queued");
    // The server's API candidate fails; the task moves on to Codex.
    let on_api = store::Queue::at(&f.pool, now_ms())
        .claim(Way::Api, &[], "server")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(on_api.id, blocking);
    let failed = aq_internal_work::Reason::ProviderError;
    store::Queue::at(&f.pool, now_ms())
        .fail_candidate(store::CandidateAttempt::new(blocking, 0), failed, "down")
        .await
        .unwrap();
    assert!(f.tick().await);
    assert_eq!(f.row(blocking).await.status, "answered");
    assert_eq!(f.row(agent).await.status, "queued");
    assert!(f.tick().await);
    let row = f.row(agent).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
}
