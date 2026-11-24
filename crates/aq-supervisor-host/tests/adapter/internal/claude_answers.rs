use super::*;

#[tokio::test]
async fn claude_run_passes_the_token_only_in_the_environment_and_never_in_argv_or_rows() {
    // The answer echoes the token the fake read from its environment.
    let answer = r#"{"choice":"yes","rationale":"token @TOKEN@"}"#;
    let f =
        Fixture::with_claude("exit 1", &claude_printing(&claude_result(false, answer), 0)).await;
    let id = f.submit(&[claude_candidate()]).await;
    assert!(f.tick().await);
    let env = f.claude_env();
    let home = PathBuf::from(&env["HOME"]);
    assert_eq!(home.parent().unwrap(), f.root().join("homes"));
    let home_text = home.to_str().unwrap();
    let expected: BTreeMap<String, String> = [
        ("HOME", home_text),
        ("CLAUDE_CONFIG_DIR", home_text),
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("DISABLE_TELEMETRY", "1"),
        ("DISABLE_ERROR_REPORTING", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
        ("CLAUDE_CODE_OAUTH_TOKEN", CLAUDE_TOKEN),
    ]
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .into();
    assert_eq!(env, expected);
    let settings = home.join("settings.json");
    assert_eq!(
        f.claude_argv(),
        [
            "-p",
            "--output-format",
            "json",
            "--input-format",
            "text",
            "--model",
            "claude-fixture",
            "--setting-sources",
            "user",
            "--settings",
            settings.to_str().unwrap(),
            "--strict-mcp-config",
            "--no-session-persistence",
            "--disable-slash-commands",
            "--tools",
            "",
        ]
    );
    assert_eq!(f.claude_rec("stdin"), validate::prompt(&request()));
    let pwd = PathBuf::from(f.claude_rec("pwd").trim());
    assert_eq!(pwd.parent().unwrap(), f.root().join("scratch"));
    assert_eq!(f.claude_rec("listing"), "");
    // The token is never written to disk: the home holds only the settings.
    assert_eq!(f.claude_rec("home"), "settings.json\n");
    let written: serde_json::Value = serde_json::from_str(&f.claude_rec("settings")).unwrap();
    assert!(written.is_object());
    assert!(!f.claude_rec("settings").contains(CLAUDE_TOKEN));
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
    assert_eq!(
        row.answer.clone().unwrap(),
        json!({"choice":"yes","rationale":"token [credential redacted]"})
    );
    assert!(!format!("{row:?}").contains(CLAUDE_TOKEN));
    assert!(!f.rec("argv").exists(), "the Codex native ran");
}

#[tokio::test]
async fn claude_result_and_usage_complete_the_task() {
    let f =
        Fixture::with_claude("exit 1", &claude_printing(&claude_result(false, ANSWER), 0)).await;
    let id = f.submit(&[claude_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
    assert_eq!(row.runner.as_deref(), Some(RUNNER));
    assert_eq!(
        row.answer.unwrap(),
        json!({"choice":"yes","rationale":"the input says so"})
    );
    let usage = row.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (13, 9));
    for dir in ["scratch", "homes"] {
        let left: Vec<_> = std::fs::read_dir(f.root().join(dir)).unwrap().collect();
        assert!(left.is_empty(), "{dir} keeps {left:?}");
    }
    assert!(!f.tick().await, "nothing else is queued");
}

#[tokio::test]
async fn claude_is_error_with_usage_limit_text_is_provider_unavailable() {
    // Exit 0: `is_error` alone fails the run. The text echoes the token.
    let text = "Claude AI usage limit reached|1731456000 (@TOKEN@)";
    let f = Fixture::with_claude("exit 1", &claude_printing(&claude_result(true, text), 0)).await;
    let id = f.submit(&[claude_candidate(), api_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "queued");
    assert_eq!(row.candidate_index, 1);
    let attempt = &row.attempts[0];
    assert_eq!(
        attempt.reason,
        Some(aq_internal_work::Reason::ProviderUnavailable)
    );
    assert!(
        attempt.detail.contains("usage limit reached"),
        "{}",
        attempt.detail
    );
    assert!(!attempt.detail.contains(CLAUDE_TOKEN), "{}", attempt.detail);
    assert!(attempt.detail.len() <= 500);
}

#[tokio::test]
async fn claude_output_that_is_not_json_is_provider_error_with_a_capped_redacted_detail() {
    let noise = format!("@TOKEN@ {}", "x".repeat(2000));
    let f = Fixture::with_claude("exit 1", &claude_printing(&noise, 1)).await;
    let id = f.submit(&[claude_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.reason, Some(aq_internal_work::Reason::ProviderError));
    let detail = &row.attempts[0].detail;
    assert!(detail.contains("[credential redacted]"), "{detail}");
    assert!(!detail.contains(CLAUDE_TOKEN), "{detail}");
    assert!(detail.len() <= 500, "{}", detail.len());
}

/// Claude's effort the way the worker's interactive launch passes it:
/// `--effort <effort>` for `low medium high xhigh max`, `off` as
/// `MAX_THINKING_TOKENS=0`, nothing for `default`, no effort or any other
/// value. The `thinking` option is not applied (the launch maps it to
/// nothing). The reference reaches the native nowhere.
#[tokio::test]
async fn claude_run_passes_the_resolved_effort_and_never_the_reference() {
    for (effort, flag, thinking) in [
        (Some("high"), Some("high"), None),
        (Some("max"), Some("max"), None),
        (Some("off"), None, Some("0")),
        (Some("default"), None, None),
        (Some("ultra"), None, None),
        (None, None, None),
    ] {
        let f = Fixture::with_claude("exit 1", &claude_printing(&claude_result(false, ANSWER), 0))
            .await;
        let id = f.submit(&[with_effort(claude_candidate(), effort)]).await;
        assert!(f.tick().await);
        let effort = format!("{effort:?}");
        assert_eq!(f.row(id).await.status, "answered", "{effort}");
        let argv = f.claude_argv();
        let passed = argv
            .iter()
            .position(|a| a == "--effort")
            .map(|i| argv[i + 1].as_str());
        assert_eq!(passed, flag, "{effort}: {argv:?}");
        let env = f.claude_env();
        assert_eq!(
            env.get("MAX_THINKING_TOKENS").map(String::as_str),
            thinking,
            "{effort}"
        );
    }
}

/// Spec 0012 §6: fallback between providers is real.
#[tokio::test]
async fn a_profile_listing_codex_then_claude_falls_back_to_claude_when_codex_is_unavailable() {
    let codex = "echo '{\"type\":\"turn.failed\",\"error\":{\"message\":\"You have hit your usage limit. Try again later.\"}}'\n\
                     exit 1";
    let f = Fixture::with_claude(codex, &claude_printing(&claude_result(false, ANSWER), 0)).await;
    let id = f.submit(&[codex_candidate(), claude_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "queued");
    assert_eq!(row.candidate_index, 1);
    assert!(
        !f.rec("claude.argv").exists(),
        "Claude ran on the first tick"
    );
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
    assert_eq!(row.attempts.len(), 2);
    assert_eq!(row.attempts[0].provider, "openai");
    assert_eq!(
        row.attempts[0].reason,
        Some(aq_internal_work::Reason::ProviderUnavailable)
    );
    assert_eq!(row.attempts[1].provider, "anthropic");
    assert_eq!(row.attempts[1].reason, None);
    let usage = row.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (13, 9));
    assert!(!f.tick().await, "nothing else is queued");
}
