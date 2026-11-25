use super::*;

#[tokio::test]
async fn codex_run_passes_read_only_sandbox_an_empty_cwd_and_the_validated_prompt() {
    let f = Fixture::new(&answering()).await;
    f.submit(&[codex_candidate()]).await;
    assert!(f.tick().await);
    let argv = f.argv();
    let after = |flag: &str| {
        let i = argv.iter().position(|a| a == flag).unwrap();
        argv[i + 1].clone()
    };
    assert_eq!(after("--sandbox"), "read-only");
    assert_eq!(after("--model"), "gpt-fixture");
    assert!(argv.iter().any(|a| a == "model_reasoning_effort=\"high\""));
    assert!(
        argv.iter()
            .any(|a| a == "shell_environment_policy.inherit=\"none\"")
    );
    assert!(argv.iter().any(|a| a == "--ephemeral"));
    assert!(!argv.iter().any(|a| a.contains("developer_instructions")));
    assert!(!argv.iter().any(|a| a.contains("AQ_SUPERVISOR")));
    assert_eq!(argv.last().unwrap(), "-");
    let cwd = PathBuf::from(after("--cd"));
    assert_eq!(cwd.parent().unwrap(), f.root().join("scratch"));
    let pwd = std::fs::read_to_string(f.rec("pwd")).unwrap();
    assert_eq!(Path::new(pwd.trim()), cwd);
    assert_eq!(std::fs::read_to_string(f.rec("listing")).unwrap(), "");
    let stdin = std::fs::read_to_string(f.rec("stdin")).unwrap();
    assert_eq!(stdin, validate::prompt(&request()));
}

#[tokio::test]
async fn codex_run_sets_the_effort_of_its_level() {
    let f = Fixture::new(&answering()).await;
    f.submit(&[with_effort(codex_candidate(), Some("low"))])
        .await;
    assert!(f.tick().await);
    let argv = f.argv();
    let efforts: Vec<_> = argv
        .iter()
        .filter(|a| a.starts_with("model_reasoning_effort="))
        .collect();
    assert_eq!(efforts, ["model_reasoning_effort=\"low\""]);
    assert!(
        !argv.iter().any(|a| a.starts_with("service_tier")),
        "{argv:?}"
    );
}

#[tokio::test]
async fn codex_run_without_an_effort_sets_no_effort() {
    let f = Fixture::new(&answering()).await;
    f.submit(&[with_effort(codex_candidate(), None)]).await;
    assert!(f.tick().await);
    let argv = f.argv();
    assert!(
        !argv
            .iter()
            .any(|a| a.starts_with("model_reasoning_effort") || a.starts_with("service_tier")),
        "{argv:?}"
    );
    assert_eq!(argv.last().unwrap(), "-");
}

#[tokio::test]
async fn codex_answer_is_completed_on_the_task_and_scratch_is_removed() {
    let f = Fixture::new(&answering()).await;
    let id = f.submit(&[codex_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered");
    assert_eq!(row.runner.as_deref(), Some(RUNNER));
    assert_eq!(
        row.answer.unwrap(),
        json!({"choice":"yes","rationale":"the input says so"})
    );
    let usage = row.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (11, 7));
    for dir in ["scratch", "homes"] {
        let left: Vec<_> = std::fs::read_dir(f.root().join(dir)).unwrap().collect();
        assert!(left.is_empty(), "{dir} keeps {left:?}");
    }
    assert!(!f.tick().await, "nothing else is queued");
}

#[tokio::test]
async fn codex_usage_limit_is_provider_unavailable_and_the_task_requeues_on_the_next_candidate() {
    // The error message echoes the login's access token; it must not reach the row.
    let f = Fixture::new(
            "echo '{\"type\":\"error\",\"message\":\"stream disconnected (access-A)\"}'\n\
             echo '{\"type\":\"turn.failed\",\"error\":{\"message\":\"You have hit your usage limit for access-A. Try again later.\"}}'\n\
             exit 1",
        )
        .await;
    let id = f.submit(&[codex_candidate(), api_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "queued");
    assert_eq!(row.candidate_index, 1);
    assert_eq!(row.runner, None);
    let attempt = &row.attempts[0];
    assert_eq!(
        attempt.reason,
        Some(aq_internal_work::Reason::ProviderUnavailable)
    );
    assert!(attempt.detail.contains("usage limit"), "{}", attempt.detail);
    assert!(!attempt.detail.contains("access-A"), "{}", attempt.detail);
    assert!(attempt.detail.len() <= 500);
    assert!(!f.tick().await, "the API candidate is not served here");
}

#[tokio::test]
async fn a_login_file_changed_during_a_run_is_not_overwritten_by_a_stale_copy() {
    let f = Fixture::new(&refreshing()).await;
    let id = f.submit(&[codex_candidate()]).await;
    let other = async {
        f.started().await;
        // Another runner refreshed the same login meanwhile.
        replace(&f.secret(), &login("C"));
        std::fs::write(f.rec("go"), b"").unwrap();
    };
    let (claimed, ()) = tokio::join!(f.tick(), other);
    assert!(claimed);
    assert_eq!(f.row(id).await.status, "answered");
    assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("C"));
}
