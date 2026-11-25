use super::*;

/// Spec 0013 §4.2: the agent works in a fresh workspace per candidate
/// holding only the request's files, and its answer is the outcome file
/// it saved, checked like any answer.
#[tokio::test]
async fn an_agent_run_gets_its_files_in_the_scratch_directory_and_its_outcome_is_the_answer() {
    // Codex leaves a file of its own and no outcome; Claude saves.
    let codex = format!(
        "{}\nrm out/outcome.json\ntouch codex-was-here",
        saving("codex")
    );
    let f = Fixture::with_claude(&codex, &saving("claude")).await;
    let id = f
        .submit_request(&agent_request(), &[codex_candidate(), claude_candidate()])
        .await;
    assert!(f.tick().await);
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
    assert_eq!(row.answer, Some(serde_json::from_str(OUTCOME).unwrap()));
    assert_eq!(
        row.attempts[0].reason,
        Some(aq_internal_work::Reason::InvalidOutput)
    );
    assert_eq!(
        row.attempts[0].detail,
        "the agent ended without saving, asking or giving up"
    );
    for name in ["codex", "claude"] {
        let tree = std::fs::read_to_string(f.rec(&format!("{name}.tree"))).unwrap();
        assert_eq!(tree, "./doc.md\n./guide/language.md\n", "{name}");
        let doc = std::fs::read_to_string(f.rec(&format!("{name}.doc"))).unwrap();
        assert_eq!(doc, "# Routing\nSay yes.");
    }
    let prompt = validate::agent_prompt(&agent_request());
    assert_eq!(std::fs::read_to_string(f.rec("stdin")).unwrap(), prompt);
    assert_eq!(f.claude_rec("stdin"), prompt);
    assert!(prompt.ends_with("finish with `aq-playbooks save`, `ask` or `unable`."));
    let codex_cwd = std::fs::read_to_string(f.rec("pwd")).unwrap();
    assert_ne!(
        codex_cwd,
        f.claude_rec("pwd"),
        "each candidate has its own workspace"
    );
    for dir in ["scratch", "homes"] {
        assert!(left_in(&f, dir).is_empty(), "{dir} keeps a run directory");
    }
}

/// Review Focus 4: an agent that ends without an outcome, or with one
/// that is malformed, too large, outside the schema or reached through a
/// link, fails its candidate as invalid output and answers nothing.
#[tokio::test]
async fn an_agent_that_ends_without_an_outcome_fails_the_run_and_changes_nothing() {
    // The model names the case; doc.md holds a valid outcome, so a
    // followed link would answer.
    let behaviour = "mkdir out\ncase \" $* \" in\n\
             *\" m-none \"*) ;;\n\
             *\" m-prose \"*) printf 'I saved it.' > out/outcome.json;;\n\
             *\" m-large \"*) head -c 5000 /dev/zero | tr '\\0' ' ' > out/outcome.json;;\n\
             *\" m-schema \"*) printf '%s' '{\"kind\":\"other\"}' > out/outcome.json;;\n\
             *\" m-link \"*) ln -s ../doc.md out/outcome.json;;\n\
             *\" m-dir-link \"*) rmdir out; mkdir elsewhere; cp doc.md elsewhere/outcome.json; \
             ln -s elsewhere out;;\n\
             *\" m-fifo \"*) mkfifo out/outcome.json;;\n\
             *\" m-hardlink \"*) ln doc.md out/outcome.json;;\n\
             esac";
    let f = Fixture::new(behaviour).await;
    let mut request = agent_request();
    if let Some(work) = request.agent.as_mut() {
        work.files.insert("doc.md".into(), OUTCOME.into());
    }
    for (model, detail) in [
        (
            "m-none",
            "the agent ended without saving, asking or giving up",
        ),
        ("m-prose", "output is not JSON"),
        (
            "m-large",
            "the outcome file is 5000 bytes, over the limit of 4096",
        ),
        ("m-schema", "output violates the schema"),
        (
            "m-link",
            "the outcome path out/outcome.json is a link or not a file",
        ),
        (
            "m-dir-link",
            "the outcome path out/outcome.json is a link or not a file",
        ),
        (
            "m-fifo",
            "the outcome path out/outcome.json is not a regular file",
        ),
        ("m-hardlink", "the outcome file has another link"),
    ] {
        let candidate = Candidate {
            model: model.into(),
            ..codex_candidate()
        };
        let id = f.submit_request(&request, &[candidate]).await;
        assert!(f.tick().await, "{model}");
        let row = f.row(id).await;
        assert_eq!(row.status, "failed", "{model}");
        assert_eq!(
            row.reason,
            Some(aq_internal_work::Reason::InvalidOutput),
            "{model}"
        );
        assert!(row.answer.is_none(), "{model}");
        let got = row.detail.unwrap();
        assert!(got.starts_with(detail), "{model}: {got}");
        assert!(left_in(&f, "scratch").is_empty(), "{model}");
    }
}

/// The Claude agent gets the file tools and a shell that may run only
/// the engine CLI, no web tools, a turn limit, the event stream as
/// output, and its file tools confined to the workspace.
#[tokio::test]
async fn the_claude_agent_argv_allows_only_the_engine_cli_in_bash() {
    let f = Fixture::with_claude("exit 1", &saving("claude")).await;
    let candidate = with_effort(claude_candidate(), Some("high"));
    let id = f.submit_request(&agent_request(), &[candidate]).await;
    assert!(f.tick().await);
    assert_eq!(f.row(id).await.status, "answered");
    // The settings are the host's, beside the agent's home, not in it.
    let home = PathBuf::from(&f.claude_env()["HOME"]);
    assert_eq!(home.file_name().unwrap(), "home");
    let settings = home.parent().unwrap().join("settings.json");
    assert_eq!(
        f.claude_argv(),
        [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
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
            "--restricted",
            "--permission-mode",
            "dontAsk",
            "--tools",
            "Bash,Read,Write,Edit,Glob,Grep",
            "--allowedTools",
            "Bash(aq-playbooks:*)",
            "Read",
            "Write",
            "Edit",
            "Glob",
            "Grep",
            "--disallowedTools",
            "WebFetch",
            "WebSearch",
            "--max-turns",
            "7",
            "--effort",
            "high",
        ]
    );
}

/// The Codex agent may write its workspace, and nothing reaches the
/// network from its sandbox; its shell inherits no environment.
#[tokio::test]
async fn the_codex_agent_runs_in_a_workspace_write_sandbox_without_network() {
    let f = Fixture::new(&saving("codex")).await;
    let id = f
        .submit_request(&agent_request(), &[codex_candidate()])
        .await;
    assert!(f.tick().await);
    assert_eq!(f.row(id).await.status, "answered");
    let argv = f.argv();
    let after = |flag: &str| {
        let i = argv.iter().position(|a| a == flag).unwrap();
        argv[i + 1].clone()
    };
    assert_eq!(after("--sandbox"), "workspace-write");
    let configs: Vec<&String> = argv
        .windows(2)
        .filter(|pair| pair[0] == "--config")
        .map(|pair| &pair[1])
        .collect();
    assert!(
        configs.contains(&&"sandbox_workspace_write.network_access=false".to_string()),
        "{argv:?}"
    );
    assert!(configs.contains(&&"shell_environment_policy.inherit=\"none\"".to_string()));
    assert!(configs.contains(&&"approval_policy=\"never\"".to_string()));
    let cwd = PathBuf::from(after("--cd"));
    assert_eq!(cwd.parent().unwrap(), f.root().join("scratch"));
    // `/tmp` is not writable; the shell's temporary directory is the
    // workspace's and its `PATH` starts with the shims.
    let has = |config: &str| configs.iter().any(|c| *c == config);
    assert!(
        has("sandbox_workspace_write.exclude_slash_tmp=true"),
        "{argv:?}"
    );
    let tmp = cwd.join(".tmp");
    assert!(has(&format!(
        "shell_environment_policy.set.TMPDIR=\"{}\"",
        tmp.display()
    )));
    let paths: Vec<_> = configs
        .iter()
        .filter(|c| c.starts_with("shell_environment_policy.set.PATH="))
        .collect();
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert!(
        paths[0].ends_with("/bin:/usr/local/bin:/usr/bin:/bin\""),
        "{paths:?}"
    );
    assert_eq!(argv.last().unwrap(), "-");
    // An answer task keeps the read-only sandbox.
    f.submit(&[codex_candidate()]).await;
    std::fs::remove_file(f.rec("argv")).unwrap();
    assert!(f.tick().await);
    let argv = f.argv();
    assert!(
        argv.windows(2)
            .any(|p| p[0] == "--sandbox" && p[1] == "read-only")
    );
    assert!(!argv.iter().any(|a| a.contains("sandbox_workspace_write")));
}

/// The harness gets its own login and nothing of the host's: no
/// database URL, no AQ credential, none of the host's environment, and
/// no file but the request's in its workspace.
#[tokio::test]
async fn the_agent_environment_has_no_database_url_or_server_credential() {
    // This test process has the test database's URL in its environment.
    assert!(std::env::var_os("DATABASE_URL").is_some());
    let codex = format!(
        "cat /proc/$$/environ > \"$rec/codex.env\"\n{}",
        saving("codex")
    );
    let f = Fixture::with_claude(&codex, &saving("claude")).await;
    for candidate in [codex_candidate(), claude_candidate()] {
        let id = f.submit_request(&agent_request(), &[candidate]).await;
        assert!(f.tick().await);
        assert_eq!(f.row(id).await.status, "answered");
    }
    let codex_env: BTreeMap<String, String> = std::fs::read_to_string(f.rec("codex.env"))
        .unwrap()
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (key, value) = entry.split_once('=').unwrap();
            (key.to_owned(), value.to_owned())
        })
        .collect();
    let keys: Vec<&str> = codex_env.keys().map(String::as_str).collect();
    assert_eq!(keys, ["CODEX_HOME", "HOME", "PATH", "TMPDIR"]);
    let claude_env = f.claude_env();
    let keys: Vec<&str> = claude_env.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
            "DISABLE_AUTOUPDATER",
            "DISABLE_ERROR_REPORTING",
            "DISABLE_TELEMETRY",
            "HOME",
            "PATH",
            "TMPDIR",
        ]
    );
    for (name, env) in [("codex", &codex_env), ("claude", &claude_env)] {
        for value in env.values() {
            assert!(!value.contains("postgres"), "{name}: {value}");
        }
        // The shims come first; the temporary directory is the workspace's.
        let home = PathBuf::from(&env["HOME"]);
        let bin = home.parent().unwrap().join("bin");
        assert_eq!(
            env["PATH"],
            format!("{}:/usr/local/bin:/usr/bin:/bin", bin.display())
        );
        let tmp = PathBuf::from(&env["TMPDIR"]);
        assert_eq!(tmp.file_name().unwrap(), ".tmp");
        assert_eq!(
            tmp.parent().unwrap().parent().unwrap(),
            f.root().join("scratch")
        );
        let tree = std::fs::read_to_string(f.rec(&format!("{name}.tree"))).unwrap();
        assert_eq!(tree, "./doc.md\n./guide/language.md\n", "{name}");
    }
    let argv = format!("{:?}{:?}", f.argv(), f.claude_argv());
    assert!(!argv.contains("postgres") && !argv.contains("DATABASE_URL"));
}
