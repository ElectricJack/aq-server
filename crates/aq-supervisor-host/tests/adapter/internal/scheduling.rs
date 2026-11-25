use super::*;

/// The native answers and exits, but a grandchild it left behind still
/// holds stdout open: the answer is reported, not held until the stop,
/// and the grandchild is killed with the session.
#[tokio::test]
async fn a_grandchild_holding_stdout_open_does_not_delay_the_report() {
    let behaviour = format!("sleep 30 &\necho $! > \"$rec/grandchild\"\n{}", answering());
    let f = Fixture::new(&behaviour).await;
    let id = f.submit_request(&short(), &[codex_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
    let pid = std::fs::read_to_string(f.rec("grandchild")).unwrap();
    let stat = format!("/proc/{}/stat", pid.trim());
    let mut alive = true;
    for _ in 0..100 {
        // Gone, or a zombie waiting for its reaper.
        alive = std::fs::read_to_string(&stat)
            .is_ok_and(|s| !s[s.rfind(')').unwrap()..].starts_with(") Z"));
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive, "the grandchild still runs");
}

/// A grandchild that left the session cannot be killed with it; it holds
/// stdout open for at most the drain window, not until the stop.
#[tokio::test]
async fn a_grandchild_that_left_the_session_holds_the_report_only_for_the_drain() {
    let behaviour = format!("setsid sleep 10 &\n{}", answering());
    let f = Fixture::new(&behaviour).await;
    let id = f.submit_request(&short(), &[codex_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
}

/// A long login run does not keep the host from claiming the next task:
/// a blocking judgment is answered while a 600 s compile still runs.
#[tokio::test]
async fn a_long_login_run_does_not_keep_the_host_from_claiming_the_next_task() {
    let hanging = "touch \"$rec/started\"\nsleep 30";
    let answers = claude_printing(&claude_result(false, ANSWER), 0);
    let f = Fixture::with_claude(hanging, &answers).await;
    let compile = Request {
        deadline_ms: 600_000,
        blocking: false,
        ..request()
    };
    let compile = f.submit_request(&compile, &[codex_candidate()]).await;
    let (config, pool) = (f.config.clone(), f.pool.clone());
    let host = tokio::spawn(serve(config, pool, RUNNER.into(), || Ok(true)));
    f.started().await;
    let judgment = f.submit(&[claude_candidate()]).await;
    let answered = tokio::time::timeout(Duration::from_secs(10), async {
        while f.row(judgment).await.status != "answered" {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    host.abort();
    assert!(answered.is_ok(), "the judgment waited behind the compile");
    assert_eq!(f.row(compile).await.status, "running");
}

/// One Codex run per login at a time: two queued Codex tasks run one
/// after the other, the second starting from the first's refresh.
#[tokio::test]
async fn two_codex_tasks_never_spend_one_refresh_token() {
    let f = Fixture::new(&rotating()).await;
    std::fs::write(f.rec("current"), "refresh-A").unwrap();
    let a = f.submit(&[codex_candidate()]).await;
    let b = f.submit(&[codex_candidate()]).await;
    serve_until_done(&f, &[a, b]).await;
    for id in [a, b] {
        let row = f.row(id).await;
        assert_eq!(row.status, "answered", "{:?}", row.attempts);
    }
    assert!(!f.rec("reused").exists(), "a refresh token was used twice");
    assert_eq!(refresh_token(&f), "refresh-Axx");
}

/// A Codex candidate that refreshed and timed out is copied back before
/// the same task's Codex fallback copies the login in.
#[tokio::test]
async fn a_codex_fallback_after_a_timeout_starts_from_the_copied_back_login() {
    let f = Fixture::new(&rotating()).await;
    std::fs::write(f.rec("current"), "refresh-A").unwrap();
    let slow = Candidate {
        model: "gpt-slow".into(),
        ..codex_candidate()
    };
    let request = Request {
        deadline_ms: 4_000,
        ..request()
    };
    let id = f.submit_request(&request, &[slow, codex_candidate()]).await;
    let extend = async {
        // The floor gives the slow candidate all 4 s; give the fallback
        // 3 s more so it has time to answer.
        f.started().await;
        sqlx::query("UPDATE internal_tasks SET deadline_at_ms=deadline_at_ms+3000 WHERE id=$1")
            .bind(id)
            .execute(&f.pool)
            .await
            .unwrap();
    };
    let ids = [id];
    let ((), ()) = tokio::join!(serve_until_done(&f, &ids), extend);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
    assert_eq!(
        row.attempts[0].reason,
        Some(aq_internal_work::Reason::DeadlineExceeded)
    );
    assert!(!f.rec("reused").exists(), "a refresh token was used twice");
    assert_eq!(refresh_token(&f), "refresh-Axx");
}

/// A config check that fails is treated like a config change: nothing
/// more is claimed, the run in progress ends and copies its refresh back,
/// and then the error is returned.
#[tokio::test]
async fn a_failing_config_check_lets_the_run_in_progress_finish_and_copy_back() {
    let behaviour = format!(
        "printf '%s' '{}' > \"$CODEX_HOME/auth.json\"\nsleep 1\n{}",
        login("B"),
        answering()
    );
    let f = Fixture::new(&behaviour).await;
    let id = f.submit(&[codex_candidate()]).await;
    let (config, pool) = (f.config.clone(), f.pool.clone());
    let check = || Err(anyhow::anyhow!("protected file rejected"));
    let served = tokio::time::timeout(
        Duration::from_secs(10),
        serve(config, pool, RUNNER.into(), check),
    )
    .await
    .expect("serve returns");
    assert!(served.is_err());
    assert_eq!(f.row(id).await.status, "answered");
    assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("B"));
}

/// A run dropped mid-way (its host task aborted) kills the native's
/// whole session, not only its leader.
#[tokio::test]
async fn a_dropped_run_kills_the_whole_session() {
    let behaviour = "sleep 30 &\necho $! > \"$rec/grandchild\"\ntouch \"$rec/started\"\nsleep 30";
    let f = std::sync::Arc::new(Fixture::new(behaviour).await);
    f.submit(&[codex_candidate()]).await;
    let running = {
        let f = f.clone();
        tokio::spawn(async move { f.tick().await })
    };
    f.started().await;
    running.abort();
    let _ = running.await;
    let pid = std::fs::read_to_string(f.rec("grandchild")).unwrap();
    let stat = format!("/proc/{}/stat", pid.trim());
    let mut alive = true;
    for _ in 0..100 {
        alive = std::fs::read_to_string(&stat)
            .is_ok_and(|s| !s[s.rfind(')').unwrap()..].starts_with(") Z"));
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive, "the grandchild survived the dropped run");
}

/// A native that ignores SIGTERM is killed when the grace ends; the run
/// still records the runner's failure and removes its directories.
#[tokio::test]
async fn a_native_that_ignores_sigterm_is_killed_after_the_grace() {
    let f = Fixture::new("trap '' TERM\nsleep 30").await;
    let id = f.submit_request(&short(), &[codex_candidate()]).await;
    let started = std::time::Instant::now();
    assert!(f.tick().await);
    let took = started.elapsed();
    // The stop at about 2.7 s, then the 1 s grace.
    assert!(
        (Duration::from_millis(3_500)..Duration::from_secs(6)).contains(&took),
        "{took:?}"
    );
    let row = f.row(id).await;
    assert_eq!(row.attempts[0].detail, "codex did not answer within 3 s");
    for dir in ["scratch", "homes"] {
        let left: Vec<_> = std::fs::read_dir(f.root().join(dir)).unwrap().collect();
        assert!(left.is_empty(), "{dir} keeps {left:?}");
    }
}

/// At start the host clears what a crashed run left: only run
/// directories (named by a UUID), and a Codex login a run refreshed is
/// copied back first when it is newer than the login file.
#[tokio::test]
async fn startup_copies_back_a_newer_leftover_login_before_clearing_run_directories() {
    let f = Fixture::new(&answering()).await;
    let homes = f.root().join("homes");
    let leftover = |login: &str| {
        let home = homes.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&home).unwrap();
        replace(&home.join("auth.json"), login);
    };
    leftover(&login_at("B", "2026-11-13T08:00:00Z"));
    leftover(&login_at("C", "2026-11-01T00:00:00Z"));
    let scratch = f
        .root()
        .join("scratch")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir(&scratch).unwrap();
    std::fs::create_dir(homes.join("not-a-run")).unwrap();
    recover_run_dirs(&f.config);
    assert_eq!(
        std::fs::read_to_string(f.secret()).unwrap(),
        login_at("B", "2026-11-13T08:00:00Z")
    );
    let names = |dir: &str| -> Vec<_> {
        std::fs::read_dir(f.root().join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    };
    assert_eq!(names("homes"), ["not-a-run"]);
    assert!(names("scratch").is_empty());
}
