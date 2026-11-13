use aq_codex_mapper::Paths;
use aq_session_protocol::LaunchManifest;
use aq_supervisor_host::adapter::native_recipe;
use std::path::Path;
#[test]
fn native_recipe_uses_scoped_route_stdin_and_no_provider_material() {
    let mut m: LaunchManifest = serde_json::from_str(include_str!(
        "../../../vendor/aq-session-host-0.1.7/tests/fixtures/manifest.json"
    ))
    .unwrap();
    m.harness.cli_version = aq_session_protocol::Text::new("0.166.0").unwrap();
    m.harness.harness = aq_session_protocol::Text::new("codex").unwrap();
    m.model.provider = aq_session_protocol::Text::new("openai").unwrap();
    m.model.model = aq_session_protocol::Text::new("fixture-model").unwrap();
    m.model.reasoning = aq_session_protocol::Text::new("high").unwrap();
    m.model.spending_budget = None;
    let recipe = native_recipe(
        &m.harness,
        &m.model,
        false,
        Paths {
            executable: Path::new("/runtime/codex"),
            workspace: Path::new("/workspace"),
            private_home: Path::new("/homes/test"),
        },
        12345,
    )
    .unwrap();
    assert_eq!(recipe.arguments.last().unwrap(), "-");
    assert!(
        recipe
            .arguments
            .iter()
            .any(|s| s.contains("http://127.0.0.1:12345/v1"))
    );
    assert!(recipe.arguments.iter().any(|s| s == "--ignore-user-config"));
    assert_eq!(
        recipe.environment["AQ_SUPERVISOR_CREDENTIAL_FILE"],
        "/homes/test/supervisor.credential"
    );
    assert!(recipe.arguments.iter().any(|s| s ==
        "shell_environment_policy.set.AQ_SUPERVISOR_CREDENTIAL_FILE=\"/homes/test/supervisor.credential\""));
    assert_eq!(
        recipe
            .arguments
            .iter()
            .rev()
            .find(|s| s.starts_with("sandbox_workspace_write.network_access="))
            .unwrap(),
        "sandbox_workspace_write.network_access=true"
    );
    assert!(
        !recipe
            .environment
            .keys()
            .any(|k| k.contains("KEY") || k.contains("TOKEN"))
    );
    assert!(
        native_recipe(
            &m.harness,
            &m.model,
            true,
            Paths {
                executable: Path::new("/runtime/codex"),
                workspace: Path::new("/workspace"),
                private_home: Path::new("/homes/test")
            },
            12345
        )
        .is_err()
    );
}

/// The `internal` mode's Codex runner, driven through one `tick` against a fake
/// native executable (a shell script) and a real test database. Runs under
/// the `test-support` identity: the current uid, no privilege drop, no chown.
mod internal {
    use aq_internal_work::{
        Affected, AnswerShape, Candidate, Purpose, Request, Way, store, validate,
    };
    use aq_session_protocol::Digest;
    use aq_supervisor_host::internal::{InternalConfig, tick};
    use serde_json::json;
    use std::{
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        path::{Path, PathBuf},
        time::Duration,
    };

    const RUNNER: &str = "internal-host:test";
    const ANSWER: &str = r#"{"choice":"yes","rationale":"the input says so"}"#;

    fn login(tag: &str) -> String {
        json!({"auth_mode":"chatgpt","tokens":{"id_token":format!("id-{tag}"),
            "access_token":format!("access-{tag}"),"refresh_token":format!("refresh-{tag}"),
            "account_id":"account-1"},"last_refresh":"2026-11-12T00:00:00Z"})
        .to_string()
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    fn request() -> Request {
        Request {
            purpose: Purpose::PlaybookCall,
            instructions: "Decide whether the input asks for a yes.".into(),
            input: "Please say yes.".into(),
            answer: AnswerShape::Choice {
                choices: vec!["yes".into(), "no".into()],
                rationale_max_bytes: 200,
            },
            blocking: true,
            affected: Affected {
                kind: "task".into(),
                id: "t-1".into(),
                project_id: None,
            },
            deadline_ms: 60_000,
            max_output_bytes: 4096,
            escalate: false,
            profile_override: None,
        }
    }

    fn codex_candidate() -> Candidate {
        Candidate {
            provider: "openai".into(),
            model: "gpt-fixture".into(),
            reasoning: "high".into(),
            harness: Some("codex".into()),
            way: Way::Login,
            credential_index: 0,
        }
    }

    fn api_candidate() -> Candidate {
        Candidate {
            harness: None,
            way: Way::Api,
            ..codex_candidate()
        }
    }

    /// Replaces `path` atomically with an owner-only file holding `body`.
    fn replace(path: &Path, body: &str) {
        let temp = path.with_extension("tmp");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .unwrap()
            .write_all(body.as_bytes())
            .unwrap();
        std::fs::rename(temp, path).unwrap();
    }

    struct Fixture {
        _db: aq_server::test_support::TestDb,
        pool: sqlx::PgPool,
        dir: tempfile::TempDir,
        config: InternalConfig,
    }

    impl Fixture {
        /// `behaviour` is shell run by the fake native after it has recorded
        /// its argv (NUL separated), stdin, cwd and the cwd's entries; `$out`
        /// is its `--output-last-message` path and `$rec` the record directory.
        async fn new(behaviour: &str) -> Self {
            Self::with_digest(behaviour, None).await
        }

        async fn with_digest(behaviour: &str, digest: Option<Digest>) -> Self {
            let db = aq_server::test_support::TestDb::create().await;
            drop(db.store().await);
            let pool = db.pool().await;
            let dir = tempfile::Builder::new()
                .prefix("aqih-")
                .tempdir_in("/tmp")
                .unwrap();
            let root = dir.path().canonicalize().unwrap();
            for sub in ["homes", "scratch", "rec", "login"] {
                std::fs::create_dir(root.join(sub)).unwrap();
                std::fs::set_permissions(root.join(sub), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            let rec = root.join("rec");
            let script = format!(
                "#!/bin/sh\numask 077\nrec='{rec}'\nout=\nwhere=\nprev=\n\
                 for a in \"$@\"; do\n  printf '%s\\0' \"$a\" >> \"$rec/argv\"\n\
                 \x20 [ \"$prev\" = --output-last-message ] && out=$a\n\
                 \x20 [ \"$prev\" = --cd ] && where=$a\n  prev=$a\ndone\n\
                 cat > \"$rec/stdin\"\npwd > \"$rec/pwd\"\nls -A \"$where\" > \"$rec/listing\"\n{behaviour}\n",
                rec = rec.display()
            );
            let native = root.join("codex");
            std::fs::write(&native, &script).unwrap();
            std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o700)).unwrap();
            let secret = root.join("login").join("auth.json");
            replace(&secret, &login("A"));
            let digest = digest.unwrap_or_else(|| Digest::of(script.as_bytes()));
            let config: InternalConfig = serde_json::from_value(json!({
                "database_url": "postgres://unused",
                "homes": root.join("homes"),
                "scratch": root.join("scratch"),
                "poll_ms": 100,
                "codex": {"native": native, "native_digest": digest, "secret_file": secret},
                "claude": null,
            }))
            .unwrap();
            Self {
                _db: db,
                pool,
                dir,
                config: config.for_tests(),
            }
        }

        fn root(&self) -> PathBuf {
            self.dir.path().canonicalize().unwrap()
        }
        fn rec(&self, name: &str) -> PathBuf {
            self.root().join("rec").join(name)
        }
        fn secret(&self) -> PathBuf {
            self.root().join("login").join("auth.json")
        }
        fn argv(&self) -> Vec<String> {
            let bytes = std::fs::read(self.rec("argv")).unwrap();
            bytes
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8(a.to_vec()).unwrap())
                .collect()
        }
        async fn submit(&self, candidates: &[Candidate]) -> uuid::Uuid {
            self.submit_request(&request(), candidates).await
        }
        async fn submit_request(&self, request: &Request, candidates: &[Candidate]) -> uuid::Uuid {
            store::submit(&self.pool, request, candidates, now_ms())
                .await
                .unwrap()
        }
        async fn tick(&self) -> bool {
            tick(&self.config, &self.pool, RUNNER, now_ms())
                .await
                .unwrap()
        }
        async fn row(&self, id: uuid::Uuid) -> store::Row {
            store::get(&self.pool, id).await.unwrap().unwrap()
        }
        /// Waits until the fake native has started.
        async fn started(&self) {
            for _ in 0..400 {
                if self.rec("started").exists() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("fake native never started");
        }
    }

    fn answering() -> String {
        format!(
            "printf '%s' '{ANSWER}' > \"$out\"\n\
             echo '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":11,\"cached_input_tokens\":0,\"output_tokens\":7}}}}'"
        )
    }

    /// Signals start, waits for the test's `go`, rewrites the home login with
    /// a refresh of the same account, then answers.
    fn refreshing() -> String {
        format!(
            "touch \"$rec/started\"\ni=0\n\
             while [ ! -e \"$rec/go\" ] && [ $i -lt 400 ]; do sleep 0.025; i=$((i+1)); done\n\
             printf '%s' '{}' > \"$CODEX_HOME/auth.json\"\n{}",
            login("B"),
            answering()
        )
    }

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
    async fn codex_usage_limit_is_provider_unavailable_and_the_task_requeues_on_the_next_candidate()
    {
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

    #[tokio::test]
    async fn a_refreshed_login_is_copied_back_when_nothing_else_changed_it() {
        let f = Fixture::new(&refreshing()).await;
        let id = f.submit(&[codex_candidate()]).await;
        let other = async {
            f.started().await;
            std::fs::write(f.rec("go"), b"").unwrap();
        };
        let (claimed, ()) = tokio::join!(f.tick(), other);
        assert!(claimed);
        assert_eq!(f.row(id).await.status, "answered");
        assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("B"));
    }

    #[tokio::test]
    async fn codex_killed_at_the_candidate_budget_is_deadline_exceeded() {
        // A grandchild holds stdout open: only killing the session ends the run.
        let f = Fixture::new("sleep 30\nprintf '%s' 'late' > \"$out\"").await;
        let short = Request {
            deadline_ms: 1_000,
            ..request()
        };
        let id = f.submit_request(&short, &[codex_candidate()]).await;
        let started = std::time::Instant::now();
        assert!(f.tick().await);
        assert!(started.elapsed() < Duration::from_secs(10));
        let row = f.row(id).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.reason, Some(aq_internal_work::Reason::DeadlineExceeded));
        let left: Vec<_> = std::fs::read_dir(f.root().join("scratch"))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "scratch keeps {left:?}");
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
}
