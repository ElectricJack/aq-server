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
            agent: None,
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

    /// The outcome every agent fake saves, as `aq-playbooks save` would.
    const OUTCOME: &str = r#"{"kind":"saved","note":"done"}"#;

    /// Agent work: a workspace with two files, an outcome checked against a
    /// schema, and a host-wide deadline of a minute.
    fn agent_request() -> Request {
        Request {
            purpose: Purpose::PlaybookCompilation,
            instructions: "Turn the document into a playbook.".into(),
            input: String::new(),
            answer: AnswerShape::Json {
                schema: json!({"type": "object", "required": ["kind"],
                    "properties": {"kind": {"enum": ["saved", "question", "unable"]}}}),
            },
            blocking: false,
            affected: Affected {
                kind: "playbook_source".into(),
                id: "routing".into(),
                project_id: None,
            },
            agent: Some(aq_internal_work::AgentWork {
                files: [
                    ("doc.md".to_string(), "# Routing\nSay yes.".to_string()),
                    ("guide/language.md".to_string(), "The language.".to_string()),
                ]
                .into(),
                max_turns: 7,
            }),
            ..request()
        }
    }

    /// Shell that records the workspace (`$rec/<name>.tree`, the files'
    /// paths, and `<name>.doc`) and then saves [`OUTCOME`].
    fn saving(name: &str) -> String {
        format!(
            "find . -type f | sort > \"$rec/{name}.tree\"\ncat doc.md > \"$rec/{name}.doc\"\n\
             mkdir -p out\nprintf '%s' '{OUTCOME}' > out/outcome.json"
        )
    }

    fn left_in(f: &Fixture, dir: &str) -> Vec<std::fs::DirEntry> {
        std::fs::read_dir(f.root().join(dir))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

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
        let candidate = with_effort(claude_candidate(), Some("high"), &[]);
        let id = f.submit_request(&agent_request(), &[candidate]).await;
        assert!(f.tick().await);
        assert_eq!(f.row(id).await.status, "answered");
        let home = PathBuf::from(&f.claude_env()["HOME"]);
        let settings = home.join("settings.json");
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
        assert_eq!(keys, ["CODEX_HOME", "HOME", "PATH"]);
        assert_eq!(codex_env["PATH"], "/usr/local/bin:/usr/bin:/bin");
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
            ]
        );
        for (name, env) in [("codex", &codex_env), ("claude", &claude_env)] {
            for value in env.values() {
                assert!(!value.contains("postgres"), "{name}: {value}");
            }
            let tree = std::fs::read_to_string(f.rec(&format!("{name}.tree"))).unwrap();
            assert_eq!(tree, "./doc.md\n./guide/language.md\n", "{name}");
        }
        let argv = format!("{:?}{:?}", f.argv(), f.claude_argv());
        assert!(!argv.contains("postgres") && !argv.contains("DATABASE_URL"));
    }

    /// Spawns the host's claim loop.
    fn serving(f: &Fixture) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        let (config, pool) = (f.config.clone(), f.pool.clone());
        tokio::spawn(serve(config, pool, RUNNER.into(), || Ok(true)))
    }

    /// Fake Claude: agent work (stream-json output) signals start, counts
    /// itself in `$rec/agents` and hangs; an answer task answers.
    fn claude_agent_hangs() -> String {
        format!(
            "case \" $* \" in *\" stream-json \"*) echo x >> \"$rec/agents\"; \
             touch \"$rec/started\"; sleep 30;; esac\n{}",
            claude_printing(&claude_result(false, ANSWER), 0)
        )
    }

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
        let on_api = store::claim(&f.pool, Way::Api, &[], "server", now_ms())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(on_api.id, blocking);
        let failed = aq_internal_work::Reason::ProviderError;
        store::fail_candidate(&f.pool, blocking, 0, failed, "down", now_ms())
            .await
            .unwrap();
        assert!(f.tick().await);
        assert_eq!(f.row(blocking).await.status, "answered");
        assert_eq!(f.row(agent).await.status, "queued");
        assert!(f.tick().await);
        let row = f.row(agent).await;
        assert_eq!(row.status, "answered", "{:?}", row.attempts);
    }

    /// The transcript store of one task, by candidate.
    async fn transcripts(f: &Fixture, id: uuid::Uuid) -> Vec<(u32, serde_json::Value)> {
        store::transcripts(&f.pool, id).await.unwrap()
    }

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
}
