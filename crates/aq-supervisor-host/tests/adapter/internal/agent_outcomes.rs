use super::*;

/// What each candidate's agent did is kept: tool calls and results and
/// the agent's text, each item at most 2 KiB, at most 256 KiB in all,
/// with the login's secrets redacted, then the outcome.
#[tokio::test]
async fn the_transcript_is_kept_clipped_and_redacted() {
    let events = [
        json!({"type": "system", "subtype": "init", "tools": ["Bash"]}),
        json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "My token is @TOKEN@."},
                {"type": "tool_use", "id": "t1", "name": "Bash",
                    "input": {"command": "aq-playbooks check"}}]}}),
        json!({"type": "user", "message": {"content": [{"type": "tool_result",
                "tool_use_id": "t1", "is_error": true,
                "content": format!("@TOKEN@ {}", "e".repeat(5000))}]}}),
    ];
    let lines: Vec<String> = events.iter().map(|e| e.to_string()).collect();
    let long = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "y".repeat(3000)}]}});
    let claude = format!(
        "{}\ni=0\nwhile [ $i -lt 200 ]; do printf '%s\\n' '{long}'; i=$((i+1)); done\n\
             printf '%s\\n' '{}'\n{}",
        claude_printing(&lines.join("\n"), 0).replace("\nexit 0", ""),
        claude_result(false, "saved"),
        saving("claude"),
    );
    let codex = format!(
        "echo '{}'\necho '{}'\n{}",
        json!({"type": "item.completed", "item": {"type": "command_execution",
                "command": "aq-playbooks facts", "aggregated_output": "access-A refresh-A",
                "exit_code": 0, "status": "completed"}}),
        json!({"type": "item.completed", "item": {"type": "agent_message",
                "text": "I read id-A."}}),
        saving("codex")
    );
    let f = Fixture::with_claude(&codex, &claude).await;
    let id = f
        .submit_request(&agent_request(), &[claude_candidate()])
        .await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
    let kept = transcripts(&f, id).await;
    assert_eq!(kept.len(), 1);
    let (index, events) = &kept[0];
    assert_eq!(*index, 0);
    assert_claude_transcript(events);

    let id = f
        .submit_request(&agent_request(), &[codex_candidate()])
        .await;
    assert!(f.tick().await);
    assert_eq!(f.row(id).await.status, "answered");
    let kept = transcripts(&f, id).await;
    assert_eq!(
        kept[0].1,
        json!([
            {"kind": "tool", "tool": "shell", "input": "aq-playbooks facts"},
            {"kind": "result", "output": "[credential redacted] [credential redacted]",
                "error": false},
            {"kind": "text", "text": "I read [credential redacted]."},
        ])
    );
}

fn assert_claude_transcript(events: &serde_json::Value) {
    let text = events.to_string();
    assert!(text.len() <= 262_144, "{}", text.len());
    assert!(!text.contains(CLAUDE_TOKEN));
    let items = events.as_array().unwrap();
    for item in items {
        assert!(item.to_string().len() <= 2048, "{item}");
    }
    assert_eq!(
        items[0],
        json!({"kind": "text", "text": "My token is [credential redacted]."})
    );
    assert_eq!(
        items[1],
        json!({"kind": "tool", "tool": "Bash", "input": "{\"command\":\"aq-playbooks check\"}"})
    );
    assert_eq!(
        (items[2]["kind"].clone(), items[2]["error"].clone()),
        (json!("result"), json!(true))
    );
    assert_eq!(items[2]["clipped"], true);
    assert!(
        items[2]["output"]
            .as_str()
            .unwrap()
            .starts_with("[credential redacted] eee")
    );
    assert_eq!(items.last().unwrap(), &json!({"kind": "clipped"}));
    assert!(items.len() > 100, "{}", items.len());
}

/// An agent run stopped at its budget is killed with its whole process
/// group, its refreshed Codex login is still copied back, its workspace
/// is removed, and its attempt says it did not finish; what it did up to
/// then is kept.
#[tokio::test]
async fn a_timed_out_agent_run_is_killed_its_login_copied_back_and_its_workspace_removed() {
    let behaviour = format!(
        "printf '%s' '{}' > \"$CODEX_HOME/auth.json\"\n\
             echo '{}'\nsleep 30 &\necho $! > \"$rec/grandchild\"\nsleep 30",
        login("B"),
        json!({"type": "item.completed", "item": {"type": "agent_message",
                "text": "checking"}}),
    );
    let f = Fixture::new(&behaviour).await;
    let short = Request {
        deadline_ms: 3_000,
        ..agent_request()
    };
    let id = f.submit_request(&short, &[codex_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.reason, Some(aq_internal_work::Reason::DeadlineExceeded));
    assert_eq!(row.attempts[0].detail, "codex did not finish within 3 s");
    assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("B"));
    for dir in ["scratch", "homes"] {
        assert!(left_in(&f, dir).is_empty(), "{dir} keeps a run directory");
    }
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
    let kept = transcripts(&f, id).await;
    assert_eq!(kept[0].1, json!([{"kind": "text", "text": "checking"}]));
}

/// The engine CLI is reached only through its shim: with a clean
/// environment (no login token), and only in the workspace.
#[tokio::test]
async fn the_engine_cli_runs_through_a_shim_with_a_clean_environment_only_in_the_workspace() {
    let claude = format!(
        "aq-playbooks check draft.md\n(cd / && aq-playbooks check); echo $? > \"$rec/outside\"\n{}",
        saving("claude")
    );
    let f = Fixture::with_claude("exit 1", &claude).await;
    let id = f
        .submit_request(&agent_request(), &[claude_candidate()])
        .await;
    assert!(f.tick().await);
    assert_eq!(f.row(id).await.status, "answered");
    let env = std::fs::read_to_string(f.rec("engine.env")).unwrap();
    let env: BTreeMap<&str, &str> = env
        .split('\0')
        .filter(|e| !e.is_empty())
        .map(|e| e.split_once('=').unwrap())
        .collect();
    let keys: Vec<&str> = env.keys().copied().collect();
    assert_eq!(keys, ["HOME", "LANG", "PATH", "TMPDIR"]);
    assert_eq!(env["PATH"], "/usr/local/bin:/usr/bin:/bin");
    assert!(!format!("{env:?}").contains(CLAUDE_TOKEN));
    let workspace = PathBuf::from(f.claude_rec("pwd").trim());
    assert_eq!(env["TMPDIR"], workspace.join(".tmp").to_str().unwrap());
    let ran_in = std::fs::read_to_string(f.rec("engine.pwd")).unwrap();
    assert_eq!(Path::new(ran_in.trim()), workspace);
    // Outside the workspace the shim refuses, and the engine never runs.
    assert_eq!(std::fs::read_to_string(f.rec("outside")).unwrap(), "126\n");
    let args = std::fs::read_to_string(f.rec("engine.args")).unwrap();
    assert_eq!(args, "check draft.md\n");
}

/// Whichever harness a run used, what it stores (answer, outcome,
/// transcript, failure detail) has the secrets of both logins removed,
/// a Codex refresh copied back during the run included.
#[tokio::test]
async fn every_run_redacts_the_secrets_of_both_logins() {
    let codex = format!(
        "case \" $* \" in\n*\" workspace-write \"*)\n\
             echo '{}'\nmkdir -p out\n\
             printf '%s' '{{\"kind\":\"saved\",\"note\":\"{CLAUDE_TOKEN}\"}}' > out/outcome.json;;\n\
             *) echo '{}'; exit 1;;\nesac",
        json!({"type": "item.completed", "item": {"type": "agent_message",
                "text": format!("the claude token is {CLAUDE_TOKEN}")}}),
        json!({"type": "turn.failed", "error": {"message": format!("boom {CLAUDE_TOKEN}")}}),
    );
    let answer = r#"{"choice":"yes","rationale":"codex access-B"}"#;
    let claude = format!(
        "touch \"$rec/started\"\ni=0\n\
             while [ ! -e \"$rec/go\" ] && [ $i -lt 400 ]; do sleep 0.025; i=$((i+1)); done\n{}",
        claude_printing(&claude_result(false, answer), 0)
    );
    let f = std::sync::Arc::new(Fixture::with_claude(&codex, &claude).await);
    let agent = f
        .submit_request(&agent_request(), &[codex_candidate()])
        .await;
    assert!(f.tick().await);
    let row = f.row(agent).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
    assert_eq!(row.answer.unwrap()["note"], "[credential redacted]");
    let kept = transcripts(&f, agent).await;
    assert_eq!(
        kept[0].1,
        json!([{"kind": "text", "text": "the claude token is [credential redacted]"}])
    );
    let failing = f.submit(&[codex_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(failing).await;
    assert_eq!(row.detail.as_deref(), Some("boom [credential redacted]"));
    // A Codex refresh (login B) lands in the login file while Claude runs.
    let claude_run = f.submit(&[claude_candidate()]).await;
    let running = {
        let f = f.clone();
        tokio::spawn(async move { f.tick().await })
    };
    f.started().await;
    replace(&f.secret(), &login("B"));
    std::fs::write(f.rec("go"), "").unwrap();
    assert!(running.await.unwrap());
    let row = f.row(claude_run).await;
    assert_eq!(
        row.answer.as_ref().unwrap()["rationale"],
        "codex [credential redacted]"
    );
    assert!(!format!("{row:?}").contains("access-B"));
}
