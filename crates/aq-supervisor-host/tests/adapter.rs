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
    use aq_supervisor_host::internal::{InternalConfig, recover_run_dirs, serve, tick};
    use serde_json::json;
    use std::{
        collections::BTreeMap,
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        path::{Path, PathBuf},
        time::Duration,
    };

    const RUNNER: &str = "internal-host:test";
    const ANSWER: &str = r#"{"choice":"yes","rationale":"the input says so"}"#;
    /// The fake Claude setup token; its file holds it with a trailing newline.
    const CLAUDE_TOKEN: &str = "sk-ant-oat01-fixture-token";

    fn login(tag: &str) -> String {
        login_at(tag, "2026-11-12T00:00:00Z")
    }

    fn login_at(tag: &str, last_refresh: &str) -> String {
        login_for(tag, last_refresh, "account-1")
    }

    fn login_for(tag: &str, last_refresh: &str, account: &str) -> String {
        json!({"auth_mode":"chatgpt","tokens":{"id_token":format!("id-{tag}"),
            "access_token":format!("access-{tag}"),"refresh_token":format!("refresh-{tag}"),
            "account_id":account},"last_refresh":last_refresh})
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

    /// A catalog reference as the server stores it: an identifier, never an
    /// effort. It must reach no native's argv or environment.
    const CODEX_REFERENCE: &str = "v1-dcedeb627973ef0987342daa2f63b4fe@1";
    const CLAUDE_REFERENCE: &str = "discovered-5749150bf4461778998a0d738a0a8f70@1";

    fn codex_candidate() -> Candidate {
        Candidate {
            provider: "openai".into(),
            model: "gpt-fixture".into(),
            reasoning: Some("high".into()),
            reasoning_reference: CODEX_REFERENCE.into(),
            options: BTreeMap::from([("reasoning_effort".into(), json!("high"))]),
            harness: Some("codex".into()),
            way: Way::Login,
            credential_index: 0,
        }
    }

    fn claude_candidate() -> Candidate {
        Candidate {
            provider: "anthropic".into(),
            model: "claude-fixture".into(),
            reasoning: Some("default".into()),
            reasoning_reference: CLAUDE_REFERENCE.into(),
            options: BTreeMap::new(),
            harness: Some("claude".into()),
            ..codex_candidate()
        }
    }

    /// `candidate` with the effort and options its model choice resolved to.
    fn with_effort(
        candidate: Candidate,
        effort: Option<&str>,
        options: &[(&str, &str)],
    ) -> Candidate {
        Candidate {
            reasoning: effort.map(String::from),
            options: options
                .iter()
                .map(|(key, value)| (key.to_string(), json!(value)))
                .collect(),
            ..candidate
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
        config: std::sync::Arc<InternalConfig>,
    }

    impl Fixture {
        /// `behaviour` is shell run by the fake native after it has recorded
        /// its argv (NUL separated), stdin, cwd and the cwd's entries; `$out`
        /// is its `--output-last-message` path and `$rec` the record directory.
        async fn new(behaviour: &str) -> Self {
            Self::with_digest(behaviour, None).await
        }

        async fn with_digest(behaviour: &str, digest: Option<Digest>) -> Self {
            Self::build(behaviour, digest, None).await
        }

        /// A host with Claude configured too. `claude` is shell run by the
        /// fake Claude after it has recorded its argv (NUL separated), its
        /// exact environment, stdin, cwd, the cwd's and `$HOME`'s entries and
        /// `$HOME/settings.json` under `$rec/claude.*`.
        async fn with_claude(codex: &str, claude: &str) -> Self {
            Self::build(codex, None, Some(claude)).await
        }

        async fn build(behaviour: &str, digest: Option<Digest>, claude: Option<&str>) -> Self {
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
            let claude = claude.map(|behaviour| {
                let dir = root.join("claude-login");
                std::fs::create_dir(&dir).unwrap();
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
                let token = dir.join("token");
                replace(&token, &format!("{CLAUDE_TOKEN}\n"));
                let script = format!(
                    "#!/bin/sh\numask 077\nrec='{rec}'\n\
                     for a in \"$@\"; do printf '%s\\0' \"$a\" >> \"$rec/claude.argv\"; done\n\
                     cat /proc/$$/environ > \"$rec/claude.env\"\n\
                     cat > \"$rec/claude.stdin\"\npwd > \"$rec/claude.pwd\"\n\
                     ls -A . > \"$rec/claude.listing\"\nls -A \"$HOME\" > \"$rec/claude.home\"\n\
                     cat \"$HOME/settings.json\" > \"$rec/claude.settings\"\n{behaviour}\n",
                    rec = rec.display()
                );
                let native = root.join("claude");
                std::fs::write(&native, &script).unwrap();
                std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o700)).unwrap();
                json!({"native": native, "native_digest": Digest::of(script.as_bytes()),
                    "secret_file": token})
            });
            let config: InternalConfig = serde_json::from_value(json!({
                "database_url": "postgres://unused",
                "homes": root.join("homes"),
                "scratch": root.join("scratch"),
                "poll_ms": 100,
                "codex": {"native": native, "native_digest": digest, "secret_file": secret},
                "claude": claude,
            }))
            .unwrap();
            Self {
                _db: db,
                pool,
                dir,
                config: std::sync::Arc::new(config.for_tests()),
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
        fn claude_rec(&self, name: &str) -> String {
            std::fs::read_to_string(self.rec(&format!("claude.{name}"))).unwrap()
        }
        fn claude_argv(&self) -> Vec<String> {
            let argv = self.claude_rec("argv");
            let argv = argv.strip_suffix('\0').unwrap();
            argv.split('\0').map(str::to_owned).collect()
        }
        fn claude_env(&self) -> BTreeMap<String, String> {
            self.claude_rec("env")
                .split('\0')
                .filter(|entry| !entry.is_empty())
                .map(|entry| {
                    let (key, value) = entry.split_once('=').unwrap();
                    (key.to_owned(), value.to_owned())
                })
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
    async fn codex_run_sets_the_resolved_effort_and_service_tier_and_never_the_reference() {
        let f = Fixture::new(&answering()).await;
        let candidate = with_effort(
            codex_candidate(),
            Some("low"),
            &[("reasoning_effort", "low"), ("service_tier", "fast")],
        );
        f.submit(&[candidate]).await;
        assert!(f.tick().await);
        let argv = f.argv();
        let efforts: Vec<_> = argv
            .iter()
            .filter(|a| a.starts_with("model_reasoning_effort="))
            .collect();
        assert_eq!(efforts, ["model_reasoning_effort=\"low\""]);
        assert!(
            argv.iter().any(|a| a == "service_tier=\"fast\""),
            "{argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains(CODEX_REFERENCE)),
            "{argv:?}"
        );
    }

    #[tokio::test]
    async fn codex_run_without_an_effort_sets_no_effort_and_no_service_tier() {
        let f = Fixture::new(&answering()).await;
        f.submit(&[with_effort(codex_candidate(), None, &[])]).await;
        assert!(f.tick().await);
        let argv = f.argv();
        assert!(
            !argv.iter().any(|a| a.starts_with("model_reasoning_effort")
                || a.starts_with("service_tier")
                || a.contains(CODEX_REFERENCE)),
            "{argv:?}"
        );
        assert_eq!(argv.last().unwrap(), "-");
    }

    #[tokio::test]
    async fn codex_candidate_with_an_unsupported_service_tier_fails_without_running() {
        let f = Fixture::new(&answering()).await;
        let candidate = with_effort(codex_candidate(), Some("low"), &[("service_tier", "turbo")]);
        let id = f.submit(&[candidate]).await;
        assert!(f.tick().await);
        let row = f.row(id).await;
        assert_eq!(row.status, "failed");
        assert_eq!(row.reason, Some(aq_internal_work::Reason::ProviderError));
        assert_eq!(row.attempts[0].detail, "unsupported service tier");
        assert!(!f.rec("argv").exists(), "the native ran");
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
        let entries: Vec<_> = std::fs::read_dir(f.root().join("login"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["auth.json"], "the lock creates nothing on disk");
    }

    /// Makes a directory read-only and restores it on drop, so the temp
    /// directory can be removed whatever the test's outcome.
    struct ReadOnly(PathBuf);
    impl ReadOnly {
        fn new(dir: PathBuf) -> Self {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            Self(dir)
        }
    }
    impl Drop for ReadOnly {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    #[tokio::test]
    async fn a_login_in_a_read_only_directory_serves_a_run_that_does_not_refresh_it() {
        let f = Fixture::new(&answering()).await;
        let _read_only = ReadOnly::new(f.root().join("login"));
        let id = f.submit(&[codex_candidate()]).await;
        assert!(f.tick().await);
        let row = f.row(id).await;
        assert_eq!(row.status, "answered", "{:?}", row.attempts);
        assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("A"));
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

    /// A native that never answers is stopped at its candidate budget and
    /// the runner records it, while a sweeper runs as the server's would; the
    /// next candidate's budget is what is left of the deadline.
    #[tokio::test]
    async fn a_native_that_never_answers_is_reported_by_the_runner_before_the_sweep() {
        use aq_internal_work::{Reason, SWEEP_GRACE_MS, candidate_budget_ms};
        let silent = "touch \"$rec/started\"\nsleep 30";
        let f = Fixture::with_claude(silent, silent).await;
        // Two candidates, 4 s: the floor gives the first all 4 s.
        let short = Request {
            deadline_ms: 4_000,
            ..request()
        };
        let id = f
            .submit_request(&short, &[codex_candidate(), claude_candidate()])
            .await;
        let done = std::sync::atomic::AtomicBool::new(false);
        let sweeper = async {
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                store::sweep(&f.pool, now_ms()).await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        // The first runner leaves at most a tenth of the 4 s. While it runs
        // (its stop was fixed when it was claimed), give the task 2 s more,
        // so a slow machine cannot let the second's remainder expire queued.
        let extend = async {
            f.started().await;
            sqlx::query("UPDATE internal_tasks SET deadline_at_ms=deadline_at_ms+2000 WHERE id=$1")
                .bind(id)
                .execute(&f.pool)
                .await
                .unwrap();
        };
        let runs = async {
            let mut claims = Vec::new();
            for index in 0..2 {
                if index == 1 {
                    let row = f.row(id).await;
                    assert_eq!((row.status.as_str(), row.candidate_index), ("queued", 1));
                }
                let deadline = f.row(id).await.deadline_at_ms;
                let claimed_at = now_ms();
                tick(&f.config, &f.pool, RUNNER, claimed_at).await.unwrap();
                claims.push((claimed_at, deadline));
            }
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            claims
        };
        let ((), claims, ()) = tokio::join!(sweeper, runs, extend);
        let row = f.row(id).await;
        for (index, (attempt, harness)) in row.attempts.iter().zip(["codex", "claude"]).enumerate()
        {
            let (claimed_at, deadline) = claims[index];
            let budget = candidate_budget_ms(deadline, claimed_at, index as u32, 2);
            if index == 1 {
                // The last candidate gets all that is left.
                assert_eq!(budget, deadline - claimed_at);
            }
            // The rule spelled out (budgets under 10 s): a tenth before the end.
            let stop = budget - budget / 10;
            assert_eq!(attempt.reason, Some(Reason::DeadlineExceeded));
            let seconds = (stop + 999) / 1000;
            let detail = format!("{harness} did not answer within {seconds} s");
            // The runner's detail, not the sweep's "runner did not report".
            assert_eq!(attempt.detail, detail, "candidate {index}");
            // Stopped at the stop, and reported before the sweep would have
            // taken it (on an idle machine also before the budget ends).
            let elapsed = attempt.elapsed_ms as i64;
            assert!(
                (stop..budget + SWEEP_GRACE_MS).contains(&elapsed),
                "candidate {index}: {elapsed} ms, stop {stop}, budget {budget}"
            );
        }
        assert_eq!(row.attempts.len(), 2, "{:?}", row.attempts);
        assert_eq!(row.status, "failed");
        for dir in ["scratch", "homes"] {
            let left: Vec<_> = std::fs::read_dir(f.root().join(dir)).unwrap().collect();
            assert!(left.is_empty(), "{dir} keeps {left:?}");
        }
    }

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

    /// A fake Codex whose provider rotates the refresh token on every
    /// refresh and rejects a token already used, as the real one does. The
    /// provider's current token is in `$rec/current` (the test writes
    /// `refresh-A`); a reuse leaves `$rec/reused`. A run on the model
    /// `gpt-slow` refreshes and then hangs, taking 0.5 s to exit on SIGTERM.
    fn rotating() -> String {
        format!(
            r#"until mkdir "$rec/provider.lock" 2>/dev/null; do sleep 0.01; done
tok=$(sed 's/.*"refresh_token":"\([^"]*\)".*/\1/' "$CODEX_HOME/auth.json")
if [ "$tok" != "$(cat "$rec/current")" ]; then
  rmdir "$rec/provider.lock"
  touch "$rec/reused"
  echo '{{"type":"turn.failed","error":{{"message":"refresh token was already used"}}}}'
  exit 1
fi
printf '%s' "${{tok}}x" > "$rec/current"
sed "s/\"$tok\"/\"${{tok}}x\"/" "$CODEX_HOME/auth.json" > "$CODEX_HOME/next.json"
mv "$CODEX_HOME/next.json" "$CODEX_HOME/auth.json"
rmdir "$rec/provider.lock"
touch "$rec/started"
case " $* " in *" gpt-slow "*) trap 'sleep 0.5; exit 0' TERM; sleep 30 & wait;; esac
sleep 0.5
{}"#,
            answering()
        )
    }

    /// Runs the host's claim loop until every task in `ids` has ended.
    async fn serve_until_done(f: &Fixture, ids: &[uuid::Uuid]) {
        let (config, pool) = (f.config.clone(), f.pool.clone());
        let host = tokio::spawn(serve(config, pool, RUNNER.into(), || Ok(true)));
        let ended = tokio::time::timeout(Duration::from_secs(20), async {
            for id in ids {
                while !matches!(f.row(*id).await.status.as_str(), "answered" | "failed") {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        })
        .await;
        host.abort();
        assert!(ended.is_ok(), "the tasks did not end");
    }

    /// The refresh token in the login file.
    fn refresh_token(f: &Fixture) -> String {
        let login: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(f.secret()).unwrap()).unwrap();
        login["tokens"]["refresh_token"]
            .as_str()
            .unwrap()
            .to_owned()
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
        let behaviour =
            "sleep 30 &\necho $! > \"$rec/grandchild\"\ntouch \"$rec/started\"\nsleep 30";
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

    /// A fake Codex that answers, except on the model `gpt-slow`, where it
    /// hangs after signalling start.
    fn slow_or_answering() -> String {
        format!(
            "touch \"$rec/started\"\ncase \" $* \" in *\" gpt-slow \"*) sleep 30;; esac\n{}",
            answering()
        )
    }

    fn slow_codex() -> Candidate {
        Candidate {
            model: "gpt-slow".into(),
            ..codex_candidate()
        }
    }

    /// Waits up to `seconds` for task `id` to be answered.
    async fn answered_within(f: &Fixture, id: uuid::Uuid, seconds: u64) -> store::Row {
        let wait = async {
            loop {
                let row = f.row(id).await;
                if row.status == "answered" {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(seconds), wait)
            .await
            .unwrap_or_else(|_| panic!("task {id} was not answered"))
    }

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

    /// A 3 s single-candidate request: the runner stops it at 2.7 s.
    fn short() -> Request {
        Request {
            deadline_ms: 3_000,
            ..request()
        }
    }

    /// Codex refreshed its login and then hung: the refresh is copied back
    /// after the runner has killed it and recorded the timeout.
    #[tokio::test]
    async fn a_login_refreshed_before_a_timeout_is_copied_back() {
        let behaviour = format!(
            "printf '%s' '{}' > \"$CODEX_HOME/auth.json\"\nsleep 30",
            login("B")
        );
        let f = Fixture::new(&behaviour).await;
        let id = f.submit_request(&short(), &[codex_candidate()]).await;
        assert!(f.tick().await);
        let row = f.row(id).await;
        assert_eq!(row.attempts[0].detail, "codex did not answer within 3 s");
        assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("B"));
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
        let f = Fixture::new(&behaviour).await;
        let id = f.submit_request(&short(), &[codex_candidate()]).await;
        assert!(f.tick().await);
        let row = f.row(id).await;
        assert_eq!(row.attempts[0].detail, "codex did not answer within 3 s");
        assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("C"));
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

    /// The object Claude Code prints with `--output-format json`.
    fn claude_result(is_error: bool, result: &str) -> String {
        json!({"type":"result","subtype":"success","is_error":is_error,"duration_ms":1840,
            "duration_api_ms":1712,"num_turns":1,"result":result,
            "session_id":"5f0c8d52-0e7a-4c55-9d43-3c1f2b8a7e10","total_cost_usd":0.0123,
            "usage":{"input_tokens":13,"cache_creation_input_tokens":0,
                "cache_read_input_tokens":0,"output_tokens":9}})
        .to_string()
    }

    /// Shell that prints `stdout`, with `@TOKEN@` replaced by the token the
    /// fake Claude found in its environment, and exits with `code`.
    fn claude_printing(stdout: &str, code: u8) -> String {
        format!("sed \"s/@TOKEN@/$CLAUDE_CODE_OAUTH_TOKEN/g\" <<'EOF'\n{stdout}\nEOF\nexit {code}")
    }

    #[tokio::test]
    async fn claude_run_passes_the_token_only_in_the_environment_and_never_in_argv_or_rows() {
        // The answer echoes the token the fake read from its environment.
        let answer = r#"{"choice":"yes","rationale":"token @TOKEN@"}"#;
        let f = Fixture::with_claude("exit 1", &claude_printing(&claude_result(false, answer), 0))
            .await;
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
        let f = Fixture::with_claude("exit 1", &claude_printing(&claude_result(false, ANSWER), 0))
            .await;
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
        let f =
            Fixture::with_claude("exit 1", &claude_printing(&claude_result(true, text), 0)).await;
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
            let f =
                Fixture::with_claude("exit 1", &claude_printing(&claude_result(false, ANSWER), 0))
                    .await;
            let options = [("thinking", "high")];
            let id = f
                .submit(&[with_effort(claude_candidate(), effort, &options)])
                .await;
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
            let seen = format!("{argv:?}{env:?}{}", f.claude_rec("settings"));
            assert!(!seen.contains(CLAUDE_REFERENCE), "{seen}");
        }
    }

    /// Spec 0012 §6: fallback between providers is real.
    #[tokio::test]
    async fn a_profile_listing_codex_then_claude_falls_back_to_claude_when_codex_is_unavailable() {
        let codex = "echo '{\"type\":\"turn.failed\",\"error\":{\"message\":\"You have hit your usage limit. Try again later.\"}}'\n\
                     exit 1";
        let f =
            Fixture::with_claude(codex, &claude_printing(&claude_result(false, ANSWER), 0)).await;
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
}
