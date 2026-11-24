use super::*;
pub(super) const TASK: Uuid = Uuid::from_u128(0x1234);

pub(super) fn target() -> DeliveryTarget {
    DeliveryTarget {
        git: GitTarget {
            repository_id: Uuid::from_u128(7),
            binding_generation: 3,
        },
        github: GitHubRepository {
            owner: "Acme".into(),
            name: "widgets".into(),
        },
        default_branch: "main".into(),
    }
}
pub(super) fn sha(c: char) -> String {
    c.to_string().repeat(40)
}
pub(super) fn pull(number: u64, extra: Value) -> Value {
    let mut v = json!({
        "number": number,
        "html_url": format!("https://github.com/Acme/widgets/pull/{number}"),
        "state": "open", "merged": false, "merged_at": null,
        "mergeable": true, "mergeable_state": "clean",
        "head": {"ref": format!("aq/{TASK}"), "sha": sha('a')},
        "base": {"ref": "main", "sha": sha('c')},
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v
}
pub(super) fn runs(runs: &[(&str, &str, Option<&str>)]) -> Value {
    let runs: Vec<Value> = runs
        .iter()
        .map(|(n, s, c)| json!({"name": n, "status": s, "conclusion": c}))
        .collect();
    json!({"total_count": runs.len(), "check_runs": runs})
}
pub(super) fn no_statuses() -> Value {
    json!({"state": "pending", "total_count": 0, "statuses": []})
}

/// A fake aq-git: answers each frame with `answer(request)` and keeps it.
pub(super) struct FakeGit {
    path: PathBuf,
    seen: Arc<Mutex<Vec<Value>>>,
    _dir: tempfile::TempDir,
}
impl FakeGit {
    pub(super) fn start(answer: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Self {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut line = String::new();
                let mut reader = BufReader::new(&mut stream);
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = answer(&request);
                log.lock().unwrap().push(request.clone());
                let mut frame = json!({"protocol_version": 1, "request_id": request["request_id"]});
                match result.get("error") {
                    Some(error) => {
                        frame["ok"] = json!(false);
                        frame["error"] = error.clone();
                    }
                    None => {
                        frame["ok"] = json!(true);
                        frame["result"] = result;
                    }
                }
                let mut bytes = serde_json::to_vec(&frame).unwrap();
                bytes.push(b'\n');
                let _ = stream.write_all(&bytes).await;
            }
        });
        Self {
            path,
            seen,
            _dir: dir,
        }
    }
    pub(super) fn git(&self) -> Arc<dyn GitPort> {
        Arc::new(SocketGit::new(&self.path))
    }
    pub(super) fn operations(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["operation"].as_str().unwrap().to_string())
            .collect()
    }
    pub(super) fn request(&self, index: usize) -> Value {
        self.seen.lock().unwrap()[index].clone()
    }
}
/// The aq-git epic 0 operations, answered as a healthy central git would.
pub(super) fn healthy(request: &Value) -> Value {
    match request["operation"].as_str().unwrap() {
        "repo.retain_ref" => {
            json!({"ref": request["params"]["ref"], "head": {"algorithm":"sha1","hex":sha('9')},
            "retention_handle": Uuid::from_u128(5)})
        }
        "publish" => {
            json!({"outcome": "applied", "target":request["params"]["target"], "old":request["params"]["expected_old"], "new":{"algorithm":"sha1","hex":sha('a')}})
        }
        "ref.delete" => json!({"outcome": "deleted"}),
        other => panic!("unexpected aq-git operation {other}"),
    }
}
pub(super) async fn delivery(git: &FakeGit) -> (Delivery, Mock) {
    let (app, mock) = app().await;
    mock.token("ghs_cmd", Utc::now() + Span::hours(1));
    (Delivery::new(git.git(), Arc::new(app)), mock)
}
pub(super) fn done(step: Step) -> Completion {
    match step {
        Step::Done(c) => c,
        Step::Retry(why) => panic!("expected a completion, got a retry: {why}"),
    }
}
pub(super) fn paths(mock: &Mock) -> Vec<(String, String)> {
    mock.seen()
        .iter()
        .skip(1)
        .map(|s| {
            (
                s.method.clone(),
                s.path
                    .trim_start_matches("/api/v3/repos/Acme/widgets")
                    .to_string(),
            )
        })
        .collect()
}

/// One poll with a required list: the PR, then the PR and check information.
pub(super) fn poll(mock: &Mock, state: Value, checked: Value, check_runs: Value) {
    mock.reply(200, vec![], state);
    mock.reply(200, vec![], checked);
    mock.reply(200, vec![], json!([]));
    mock.reply(200, vec![], json!({"name": "main", "protected": false}));
    mock.reply(200, vec![], check_runs);
    mock.reply(200, vec![], no_statuses());
}
pub(super) async fn step(delivery: &Delivery, now: i64, deadline: i64) -> Option<Completion> {
    poll_of(delivery, now, deadline).await.completion
}
pub(super) async fn poll_of(delivery: &Delivery, now: i64, deadline: i64) -> CheckPoll {
    let required = ["unit".to_string()];
    delivery
        .await_checks_step(
            &target(),
            CheckRequest::new(7, Some(&required[..]), 0),
            (now, deadline),
        )
        .await
}

pub(super) async fn poll_result(delivery: &Delivery, pr: u64, head: &str) -> Option<Completion> {
    delivery
        .step(
            FrozenCommand::new(
                Uuid::new_v4(),
                "pull_request.await_checks",
                &json!({
                    "pull_request": pr, "result_head": head,
                    "check_target": target(),
                    "required": ["check", "deployment"], "deadline_at_ms": 5_000
                }),
            ),
            &target(),
            1_000,
        )
        .await
        .unwrap()
        .step
        .map(done)
}
