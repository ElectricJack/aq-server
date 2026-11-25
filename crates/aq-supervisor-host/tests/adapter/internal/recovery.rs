use super::*;

/// While a 600 s compile holds the Codex login, a blocking judgment whose
/// first candidate is Codex skips it at once ("codex login busy") and is
/// answered by its Claude candidate. A background task with plenty of
/// time keeps waiting for the login.
#[tokio::test]
async fn a_blocking_judgment_skips_a_busy_codex_login_and_is_answered_by_claude() {
    let answers = claude_printing(&claude_result(false, ANSWER), 0);
    let f = Fixture::with_claude(&slow_or_answering(), &answers).await;
    let long = Request {
        deadline_ms: 600_000,
        blocking: false,
        ..request()
    };
    let compile = f.submit_request(&long, &[slow_codex()]).await;
    let background = f
        .submit_request(&long, &[codex_candidate(), claude_candidate()])
        .await;
    let (config, pool) = (f.config.clone(), f.pool.clone());
    let host = tokio::spawn(serve(config, pool, RUNNER.into(), || Ok(true)));
    f.started().await;
    let judgment = f.submit(&[codex_candidate(), claude_candidate()]).await;
    let row = answered_within(&f, judgment, 10).await;
    let waiting = f.row(background).await;
    let running = f.row(compile).await.status;
    host.abort();
    assert_eq!(row.attempts.len(), 2, "{:?}", row.attempts);
    assert_eq!(
        row.attempts[0].reason,
        Some(aq_internal_work::Reason::LoginBusy)
    );
    assert_eq!(row.attempts[0].detail, "codex login busy");
    assert_eq!(row.attempts[1].provider, "anthropic");
    assert_eq!(running, "running");
    assert_eq!(
        (waiting.status.as_str(), waiting.candidate_index),
        ("queued", 0)
    );
    assert!(waiting.attempts.is_empty());
}

/// A blocking task whose only candidate is Codex is not skipped: it waits
/// for the busy login and runs when the login is free.
#[tokio::test]
async fn a_codex_only_task_waits_for_the_busy_login_and_then_runs() {
    let f = Fixture::new(&slow_or_answering()).await;
    let short_compile = Request {
        deadline_ms: 4_000,
        blocking: false,
        ..request()
    };
    let compile = f.submit_request(&short_compile, &[slow_codex()]).await;
    let (config, pool) = (f.config.clone(), f.pool.clone());
    let host = tokio::spawn(serve(config, pool, RUNNER.into(), || Ok(true)));
    f.started().await;
    let judgment = f.submit(&[codex_candidate()]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let waiting = f.row(judgment).await;
    let row = answered_within(&f, judgment, 15).await;
    host.abort();
    assert_eq!(
        (waiting.status.as_str(), waiting.attempts.len()),
        ("queued", 0)
    );
    assert_eq!(row.attempts.len(), 1, "{:?}", row.attempts);
    assert_eq!(row.attempts[0].reason, None);
    assert_eq!(f.row(compile).await.status, "failed");
}

/// A Codex run that panics after its native refreshed the login still
/// copies the refresh back before the login is released, so the next
/// Codex run starts from it.
#[tokio::test]
async fn a_codex_run_that_panics_copies_its_refresh_back_before_the_login_is_released() {
    let mut f = Fixture::new(&rotating()).await;
    std::fs::write(f.rec("current"), "refresh-A").unwrap();
    let config = std::sync::Arc::try_unwrap(f.config).ok().unwrap();
    f.config = std::sync::Arc::new(config.panic_in_codex_runs(1));
    f.submit(&[codex_candidate()]).await;
    let second = f.submit(&[codex_candidate()]).await;
    let (config, pool) = (f.config.clone(), f.pool.clone());
    let host = tokio::spawn(serve(config, pool, RUNNER.into(), || Ok(true)));
    let row = answered_within(&f, second, 15).await;
    host.abort();
    assert_eq!(row.attempts.len(), 1, "{:?}", row.attempts);
    assert!(!f.rec("reused").exists(), "a refresh token was used twice");
    assert_eq!(refresh_token(&f), "refresh-Axx");
}

/// A leftover run home holding a login is never removed unchecked: with
/// Codex no longer configured there is nothing to compare it with, so it
/// is kept.
#[tokio::test]
async fn startup_keeps_a_leftover_login_when_codex_is_not_configured() {
    let f = Fixture::new(&answering()).await;
    let home = f
        .root()
        .join("homes")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&home).unwrap();
    replace(
        &home.join("auth.json"),
        &login_at("B", "2026-11-13T08:00:00Z"),
    );
    let config: InternalConfig = serde_json::from_value(json!({
        "database_url": "postgres://unused",
        "homes": f.root().join("homes"),
        "scratch": f.root().join("scratch"),
        "poll_ms": 100,
    }))
    .unwrap();
    recover_run_dirs(&config.for_tests());
    assert!(
        home.join("auth.json").exists(),
        "the leftover login was deleted"
    );
}

/// A leftover login of another account is not copied in, however new.
#[tokio::test]
async fn startup_does_not_copy_back_a_login_of_another_account() {
    let f = Fixture::new(&answering()).await;
    let home = f
        .root()
        .join("homes")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&home).unwrap();
    let other = login_for("B", "2026-11-13T08:00:00Z", "account-2");
    replace(&home.join("auth.json"), &other);
    recover_run_dirs(&f.config);
    assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("A"));
}

/// A newer leftover login that cannot be copied back (the login
/// directory is read-only) keeps its home for the next start.
#[tokio::test]
async fn startup_keeps_a_newer_leftover_login_it_could_not_copy_back() {
    let f = Fixture::new(&answering()).await;
    let home = f
        .root()
        .join("homes")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&home).unwrap();
    replace(
        &home.join("auth.json"),
        &login_at("B", "2026-11-13T08:00:00Z"),
    );
    let read_only = ReadOnly::new(f.root().join("login"));
    recover_run_dirs(&f.config);
    drop(read_only);
    assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("A"));
    assert!(
        home.join("auth.json").exists(),
        "the newer login was deleted"
    );
}

/// Codex refreshed its login and then hung: the refresh is copied back
/// after the runner has killed it and recorded the timeout.
#[tokio::test]
async fn a_login_refreshed_before_a_timeout_is_copied_back() {
    let behaviour = format!(
        "printf '%s' '{}' > \"$CODEX_HOME/auth.json\"\nsleep 30",
        login("B")
    );
    assert_refresh_after_timeout(&behaviour, "B").await;
}

/// At the stop the native gets SIGTERM first, and a moment to finish
/// writing a refresh before SIGKILL; that refresh is copied back.
#[tokio::test]
async fn a_timed_out_native_is_terminated_first_and_its_refresh_is_kept() {
    let behaviour = format!(
        "trap 'printf %s '\\''{}'\\'' > \"$CODEX_HOME/auth.json\"; exit 0' TERM\n\
             sleep 30 &\nwait",
        login("C")
    );
    assert_refresh_after_timeout(&behaviour, "C").await;
}

#[tokio::test]
async fn native_whose_digest_differs_is_refused_before_exec() {
    let f = Fixture::with_digest(&answering(), Some(Digest::of(b"another binary"))).await;
    let id = f.submit(&[codex_candidate()]).await;
    assert!(f.tick().await);
    assert!(!f.rec("argv").exists(), "the native ran");
    let row = f.row(id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(
        row.reason,
        Some(aq_internal_work::Reason::ProviderUnavailable)
    );
    assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("A"));
}

/// The native's digest is remembered with its file's metadata; a native
/// changed after it was verified is hashed again and refused.
#[tokio::test]
async fn a_native_changed_after_it_was_verified_is_hashed_again_and_refused() {
    let f = Fixture::new(&answering()).await;
    let first = f.submit(&[codex_candidate()]).await;
    assert!(f.tick().await);
    assert_eq!(f.row(first).await.status, "answered");
    let native = f.root().join("codex");
    let mut script = std::fs::read_to_string(&native).unwrap();
    script.push_str("# changed\n");
    std::fs::write(&native, script).unwrap();
    let second = f.submit(&[codex_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(second).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.attempts[0].detail, "native executable refused");
}

async fn assert_refresh_after_timeout(behaviour: &str, tag: &str) {
    let f = Fixture::new(behaviour).await;
    let id = f.submit_request(&short(), &[codex_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.attempts[0].detail, "codex did not answer within 3 s");
    assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login(tag));
}
