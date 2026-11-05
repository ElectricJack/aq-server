//! D11 / GATE-05: the routing, watcher and report pilots and D9's coding
//! policy run end to end in the embedded host (pinned runner, protected
//! PostgreSQL storage) against the production catalog 2.1.0 (Host8) adapters on
//! disposable PostgreSQL.
//!
//! Designated fake external peers: a deterministic model provider
//! (judgments; authoring completes through the work lifecycle), a fake
//! notification destination (a loopback Discord webhook behind the real
//! notifications owner) and a local bare Git remote (behind the D7
//! integration port). Recorded extra fakes (D11.md "Limits"): the graph
//! (aq-graph is a separate service) and the sandbox check executor
//! (unbound in production).
mod common;
use aq_playbook_host::model::{Artifact, hash_bytes};
use aq_server::{
    delivery::{PublicationMode, PublicationPolicy},
    notifications::{config::Loaded, persistence::RouteRequest},
    playbooks::{
        coding::{CodingPolicy, CodingRequest, HumanDecision, RequiredCheck, pilots},
        domain::*,
        driver::Driver,
        host_adapters::*,
        review_domain::{CatalogProfile, HOST7_CAPABILITIES, HOST8_FINGERPRINT, HOST8_VERSION},
        service::*,
    },
    protected_storage::{KeyFile, Keyring, ProtectionConfig},
};
use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

// ------------------------------------------------------------------ graph

/// In-memory graph (aq-graph is a separate service). Replays by idempotency
/// key, enforces `update_node` expected versions, and counts applications.
#[derive(Default)]
struct FakeGraph {
    nodes: Mutex<BTreeMap<Uuid, Value>>,
    replies: Mutex<BTreeMap<String, (u16, Value)>>,
    applied: Mutex<BTreeMap<String, usize>>,
    /// Bump this node's version once, right after the next read of it.
    bump_after_read: Mutex<Option<Uuid>>,
    /// Apply the next mutation, then lose its reply.
    lose_next_reply: AtomicBool,
}
impl FakeGraph {
    fn patch(&self, id: Uuid, f: impl FnOnce(&mut Value)) {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(&id).unwrap();
        f(node);
        node["version"] = json!(node["version"].as_i64().unwrap() + 1);
    }
    fn by_key(&self, key: &str) -> Uuid {
        let nodes = self.nodes.lock().unwrap();
        *nodes
            .iter()
            .find(|(_, n)| n["metadata"]["aq:logical_key"] == key)
            .unwrap_or_else(|| panic!("no work {key}"))
            .0
    }
    fn node(&self, id: Uuid) -> Value {
        self.nodes.lock().unwrap()[&id].clone()
    }
    fn kinds(&self, kind: &str) -> usize {
        self.nodes
            .lock()
            .unwrap()
            .values()
            .filter(|n| n["metadata"]["aq:work_kind"] == kind)
            .count()
    }
    fn applied_changes(&self) -> Vec<usize> {
        self.applied
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.starts_with("aq-playbook-change-"))
            .map(|(_, n)| *n)
            .collect()
    }
}
#[async_trait::async_trait]
impl WorkGraph for FakeGraph {
    async fn node(&self, _: Uuid, id: Uuid) -> Result<Option<Value>> {
        let node = self.nodes.lock().unwrap().get(&id).cloned();
        let mut bump = self.bump_after_read.lock().unwrap();
        if *bump == Some(id) {
            *bump = None;
            drop(bump);
            self.patch(id, |n| n["metadata"]["aq:concurrent"] = json!(true));
        }
        Ok(node)
    }
    async fn mutate(&self, _: Uuid, body: Value) -> Result<(u16, Value)> {
        let key = body["idempotency_key"].as_str().unwrap().to_string();
        if let Some(reply) = self.replies.lock().unwrap().get(&key) {
            return Ok(reply.clone());
        }
        let mut results = vec![];
        let mut status = 200;
        for op in body["operations"].as_array().unwrap() {
            match op["op"].as_str().unwrap() {
                "create_node" => {
                    let id: Uuid = serde_json::from_value(op["id"].clone()).unwrap();
                    let mut node = op.clone();
                    node["version"] = json!(1);
                    self.nodes.lock().unwrap().insert(id, node.clone());
                    results.push(node);
                }
                "update_node" => {
                    let id: Uuid = serde_json::from_value(op["id"].clone()).unwrap();
                    let mut nodes = self.nodes.lock().unwrap();
                    let node = nodes.get_mut(&id).unwrap();
                    if node["version"] != op["expected_version"] {
                        status = 409;
                        break;
                    }
                    for (k, v) in op["patch"].as_object().unwrap() {
                        node[k] = v.clone();
                    }
                    node["version"] = json!(node["version"].as_i64().unwrap() + 1);
                    results.push(node.clone());
                }
                "add_edge" => results.push(op.clone()),
                other => panic!("unexpected op {other}"),
            }
        }
        let reply = (
            status,
            json!({"change_set_id": Uuid::new_v4(), "results": results}),
        );
        self.replies
            .lock()
            .unwrap()
            .insert(key.clone(), reply.clone());
        *self.applied.lock().unwrap().entry(key).or_default() += 1;
        if self.lose_next_reply.swap(false, Ordering::SeqCst) {
            return Err(Error::Unknown);
        }
        Ok(reply)
    }
}

// ------------------------------------------------------- designated fakes

/// Deterministic model provider: always the last offered choice.
#[derive(Default)]
struct FakeModel {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl JudgmentProvider for FakeModel {
    fn credential_profile(&self, _: Uuid, _: Uuid) -> Option<String> {
        Some("fake-model".into())
    }
    async fn complete(
        &self,
        request: &JudgmentRequest,
    ) -> std::result::Result<JudgmentReply, ProviderFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(JudgmentReply {
            provider: "fake".into(),
            model: "deterministic-1".into(),
            choice: request.choices.last().unwrap().clone(),
            rationale: "deterministic".into(),
            input_tokens: 10,
            output_tokens: 5,
            cost_microunits: 1,
        })
    }
}

/// Recorded extra fake: production leaves the sandbox executor unbound.
/// Every attempt exits 0 on its first observation.
#[derive(Default)]
struct FakeChecks {
    started: AtomicUsize,
}
fn validator() -> ArtifactPin {
    ArtifactPin {
        id: "rust.cargo-test".into(),
        version: "1".into(),
        digest: hash_bytes(b"validator"),
    }
}
#[async_trait::async_trait]
impl CheckExecutor for FakeChecks {
    async fn pinned(&self, _: Uuid, _: Uuid, subject: &str) -> Result<Option<PinnedOutcome>> {
        Ok(subject.starts_with("aq.candidate/").then(|| PinnedOutcome {
            revision: 1,
            required_checks: vec![PinnedCheck {
                id: "cargo-test".into(),
                validator: validator(),
            }],
        }))
    }
    async fn start(&self, _: &CheckAttempt) -> Result<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn observe(&self, _: Uuid) -> Result<CheckObservation> {
        Ok(CheckObservation::Finished(CheckRun {
            stop: CheckStop::Exited(0),
            inputs_complete: true,
            executor_version: "fake-checks-1".into(),
            inputs_digest: hash_bytes(b"inputs"),
            result_artifacts: vec![],
        }))
    }
    async fn cancel(&self, _: Uuid) -> Result<()> {
        Ok(())
    }
}

/// Local bare Git remote behind the D7 integration port: candidates are the
/// retained source commits; publication is a compare-and-swap update-ref.
struct BareRemote {
    _dir: tempfile::TempDir,
    git_dir: PathBuf,
    repository: Uuid,
    receipts: Mutex<BTreeMap<Uuid, String>>,
    published: Mutex<BTreeMap<Uuid, String>>,
    sources: AtomicUsize,
}
fn oid(hex: &str) -> ObjectId {
    ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: hex.into(),
    }
}
impl BareRemote {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join("remote.git");
        let remote = Self {
            _dir: dir,
            git_dir,
            repository: Uuid::new_v4(),
            receipts: Mutex::default(),
            published: Mutex::default(),
            sources: AtomicUsize::new(0),
        };
        remote.git(&["init", "--bare", "-q"], None).unwrap();
        let tree = remote.git(&["mktree"], Some("")).unwrap();
        let base = remote
            .git(&["commit-tree", &tree, "-m", "base"], None)
            .unwrap();
        remote
            .git(&["update-ref", "refs/heads/main", &base], None)
            .unwrap();
        remote
    }
    fn git(&self, args: &[&str], stdin: Option<&str>) -> std::result::Result<String, String> {
        use std::io::Write;
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "AQ")
            .env("GIT_AUTHOR_EMAIL", "aq@example.test")
            .env("GIT_COMMITTER_NAME", "AQ")
            .env("GIT_COMMITTER_EMAIL", "aq@example.test")
            .env("GIT_AUTHOR_DATE", "2026-11-04T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-11-04T00:00:00Z")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.unwrap_or("").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        if out.status.success() {
            Ok(String::from_utf8(out.stdout).unwrap().trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }
    fn head(&self) -> String {
        self.git(&["rev-parse", "refs/heads/main"], None).unwrap()
    }
    /// A commit on top of `parent`, as an implementation's retained source.
    fn commit_on(&self, parent: &str, name: &str) -> String {
        let blob = self
            .git(&["hash-object", "-w", "--stdin"], Some(name))
            .unwrap();
        let tree = self
            .git(
                &["mktree"],
                Some(&format!("100644 blob {blob}\t{name}.txt\n")),
            )
            .unwrap();
        self.git(&["commit-tree", &tree, "-p", parent, "-m", name], None)
            .unwrap()
    }
    /// Retains one implementation source on the current main; returns its
    /// receipt id.
    fn retain_source(&self) -> Uuid {
        let n = self.sources.fetch_add(1, Ordering::SeqCst);
        let commit = self.commit_on(&self.head(), &format!("change-{n}"));
        let receipt = Uuid::new_v4();
        self.receipts.lock().unwrap().insert(receipt, commit);
        receipt
    }
}
#[async_trait::async_trait]
impl Integration for BareRemote {
    async fn binding(&self, _: Uuid, _: Uuid) -> Result<Option<IntegrationBinding>> {
        Ok(Some(IntegrationBinding {
            repository_id: self.repository,
            binding_generation: 1,
            publication: PublicationPolicy {
                mode: PublicationMode::FastForward,
                target: "refs/heads/main".into(),
                wait_for_checks: None,
            },
            evidence: EvidencePolicy {
                checks: vec!["cargo-test".into()],
                review: true,
                judgment_choices: vec![],
            },
        }))
    }
    async fn prepare(&self, call: &PrepareCall) -> Result<PrepareReply> {
        let receipts = self.receipts.lock().unwrap().clone();
        let [source] = call.sources.as_slice() else {
            return Ok(PrepareReply::Refused {
                reason: "unsupported".into(),
            });
        };
        let Some(commit) = receipts.get(source) else {
            return Ok(PrepareReply::Refused {
                reason: "source_unknown".into(),
            });
        };
        if self
            .git(
                &["merge-base", "--is-ancestor", &call.base.hex, commit],
                None,
            )
            .is_err()
        {
            return Ok(PrepareReply::Conflict {
                step: Some(0),
                paths: vec![],
                truncated: false,
                count: 0,
            });
        }
        Ok(PrepareReply::Prepared {
            candidate: oid(commit),
        })
    }
    async fn publish(&self, call: &PublishCall) -> Result<PublishReply> {
        let new = &call.candidate.hex;
        let old = &call.expected_old.hex;
        if self
            .git(&["update-ref", "refs/heads/main", new, old], None)
            .is_ok()
        {
            self.published
                .lock()
                .unwrap()
                .insert(call.operation, new.clone());
            return Ok(PublishReply::Published {
                old: oid(old),
                new: oid(new),
            });
        }
        if &self.head() == new {
            return Ok(PublishReply::ObservedPresent { observed: oid(new) });
        }
        Ok(PublishReply::Refused {
            reason: "target_moved".into(),
        })
    }
    async fn reconcile(&self, call: &ReconcileCall) -> Result<ReconcileReply> {
        let published = self
            .published
            .lock()
            .unwrap()
            .get(&call.target_operation)
            .cloned();
        Ok(match published {
            Some(c) if c == self.head() => ReconcileReply::Present { observed: oid(&c) },
            _ => ReconcileReply::Unresolved,
        })
    }
    async fn review(&self, _: Uuid, _: Uuid, _: Uuid) -> Result<Option<ReviewEvidence>> {
        Ok(None)
    }
}

/// Fake notification destination: a loopback Discord webhook answering the
/// queued responses in order (the last one repeats).
struct Destination {
    url: String,
    calls: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}
impl Drop for Destination {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
impl Destination {
    async fn new(responses: Vec<(u16, &'static str)>) -> Self {
        use axum::{Router, response::IntoResponse, routing::post};
        let calls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let c = calls.clone();
        let responses = Arc::new(responses);
        let app = Router::new().route(
            "/api/v10/webhooks/{id}/{token}",
            post(move || {
                let c = c.clone();
                let responses = responses.clone();
                async move {
                    let i = c.fetch_add(1, Ordering::SeqCst);
                    let (code, body) = responses[i.min(responses.len() - 1)];
                    (axum::http::StatusCode::from_u16(code).unwrap(), body).into_response()
                }
            }),
        );
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://127.0.0.1:{port}/api/v10/webhooks/42/fixture_token_1234567890"),
            calls,
            handle,
        }
    }
}

// ---------------------------------------------------------------- harness

struct Pilot {
    f: common::profiled::Fixture,
    service: Service,
    driver: Option<Driver>,
    graph: Arc<FakeGraph>,
    model: Arc<FakeModel>,
    checks: Arc<FakeChecks>,
    remote: Arc<BareRemote>,
    grant: Uuid,
    /// A profiled attempt with a recorded intelligence selection.
    attempt: Uuid,
    installed: Vec<Artifact>,
    /// Whether the author lifecycle completes supervisor_authoring work.
    author: AtomicBool,
    /// Moves the remote's main before the next review completes.
    move_base_on_review: AtomicBool,
}

fn keyring() -> Arc<Keyring> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key");
    std::fs::write(&path, [9_u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let keys = Keyring::load(&ProtectionConfig {
        active_key_id: "test".into(),
        keys: vec![KeyFile {
            id: "test".into(),
            file: path,
        }],
    })
    .unwrap();
    Arc::new(keys)
}

fn protected_factory(
    f: &common::profiled::Fixture,
) -> impl FnOnce() -> std::result::Result<
    aq_playbook_host::postgres_storage::PostgresStorage,
    aq_playbook_host::Error,
> + Send
+ 'static {
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    url.set_path(f.db.options().get_database().unwrap());
    move || {
        let client = url
            .as_str()
            .parse::<aq_playbook_host::postgres_client::Config>()
            .unwrap()
            .connect(aq_playbook_host::postgres_client::NoTls)
            .map_err(|_| aq_playbook_host::Error::Unknown)?;
        let keys = aq_playbook_host::postgres_storage::ProtectionKeyring::new(
            "d11",
            BTreeMap::from([("d11".into(), [11; 32])]),
        )
        .unwrap();
        aq_playbook_host::protected::storage(
            client,
            keys,
            Duration::from_secs(2),
            Duration::from_millis(100),
        )
    }
}

fn scope(f: &common::profiled::Fixture) -> GrantScope {
    let l = &f.selection.lineage;
    GrantScope {
        workspace_id: l.workspace_id,
        project_id: l.project_id,
        recipients: vec![],
        capabilities: BTreeSet::from(HOST7_CAPABILITIES.map(String::from)),
    }
}

impl Pilot {
    async fn new() -> Self {
        let f = common::profiled::fixture().await;
        let attempt = common::profiled::insert(&f).await;
        let l = &f.selection.lineage;
        let graph = Arc::new(FakeGraph::default());
        let model = Arc::new(FakeModel::default());
        let checks = Arc::new(FakeChecks::default());
        let remote = Arc::new(BareRemote::new());
        // External notification routes need an https public origin (the
        // fixture's development origin only serves dashboard messages).
        let mut state = f.state.clone();
        let mut config = (*state.config).clone();
        config.server.public_origin = "https://aq.example".into();
        state.config = Arc::new(config);
        let service = Service::new_host8_integration(
            &state,
            Namespace {
                namespace_id: Uuid::new_v4(),
                generation: 1,
                name: format!("d11-{}", Uuid::new_v4()),
                workspace_id: l.workspace_id,
                project_id: l.project_id,
                owner_user_id: f.actor,
            },
            keyring(),
            graph.clone(),
            Some(checks.clone()),
            Some(model.clone()),
            Some(remote.clone()),
        )
        .await
        .unwrap();
        service.initialize(f.actor).await.unwrap();
        let grant = Uuid::new_v4();
        service
            .grant(f.actor, grant, scope(&f), Self::far(&f))
            .await
            .unwrap();
        let mut pilot = Self {
            f,
            service,
            driver: None,
            graph,
            model,
            checks,
            remote,
            grant,
            attempt,
            installed: vec![],
            author: AtomicBool::new(true),
            move_base_on_review: AtomicBool::new(false),
        };
        pilot.start().await;
        pilot
    }
    fn far(f: &common::profiled::Fixture) -> i64 {
        (f.state.clock)().timestamp_millis() + 86_400_000
    }
    fn now(&self) -> i64 {
        (self.f.state.clock)().timestamp_millis()
    }
    fn advance(&self, ms: i64) {
        self.f.time.fetch_add(ms, Ordering::SeqCst);
    }
    fn driver(&self) -> &Driver {
        self.driver.as_ref().unwrap()
    }
    async fn start(&mut self) {
        // Vendored playbooks 0173673 persists the whole namespace per
        // transition (pre-PB-04), so a debug-build tick of eight transitions
        // outgrows the driver's 5 s owner call as history accumulates.
        let driver = Driver::start(
            self.service.clone(),
            aq_playbook_host::Limits {
                transitions: 2,
                ..Default::default()
            },
            protected_factory(&self.f),
        )
        .await
        .unwrap();
        for a in &self.installed {
            driver.install(a.clone()).await.unwrap();
        }
        self.driver = Some(driver);
    }
    /// Process loss: stop the owner, then start a new one over the same
    /// storage and run explicit recovery.
    async fn restart(&mut self) {
        self.driver.take().unwrap().stop().await.unwrap();
        self.start().await;
        self.driver().recover().await.unwrap();
    }
    async fn install(&mut self, artifacts: &[&Artifact], binding: &str, grant: Uuid) -> u64 {
        for a in artifacts {
            self.driver().install((*a).clone()).await.unwrap();
            if !self.installed.iter().any(|i| i.digest == a.digest) {
                self.installed.push((*a).clone());
            }
        }
        let root = artifacts.last().unwrap();
        let view = self
            .driver()
            .activate(binding.into(), root.digest.clone(), grant, 1)
            .await
            .unwrap();
        view["revision"].as_u64().unwrap()
    }
    async fn admit(&self, binding: &str, revision: u64, grant: Uuid, input: Value) -> String {
        let request = PrepareRequest {
            request_id: Uuid::new_v4(),
            grant_id: grant,
            grant_revision: 1,
            binding: binding.into(),
            expected_activation_revision: revision,
            entry: "main".into(),
            input,
        };
        self.service
            .prepare(self.f.actor, request.clone())
            .await
            .unwrap();
        let admitted = self
            .driver()
            .admission(self.f.actor, request.request_id, false)
            .await
            .unwrap();
        admitted.receipt.unwrap().run_id
    }
    /// The task lifecycle outside the playbook: implementations retain a
    /// source and complete, reviews approve, authoring completes when
    /// enabled.
    fn lifecycle(&self) {
        let open: Vec<(Uuid, String)> = self
            .graph
            .nodes
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, n)| n["task_state"] == "active")
            .map(|(id, n)| {
                (
                    *id,
                    n["metadata"]["aq:work_kind"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        for (id, kind) in open {
            match kind.as_str() {
                "implementation" | "repair" => {
                    let receipt = self.remote.retain_source();
                    self.graph.patch(id, |n| {
                        n["metadata"]["aq:source_receipt"] = json!(receipt);
                        n["task_state"] = json!("completed");
                    });
                }
                "review" => {
                    if self.move_base_on_review.swap(false, Ordering::SeqCst) {
                        let moved = self.remote.commit_on(&self.remote.head(), "external");
                        self.remote
                            .git(&["update-ref", "refs/heads/main", &moved], None)
                            .unwrap();
                    }
                    self.graph.patch(id, |n| {
                        n["metadata"]["aq:review_disposition"] = json!("accepted");
                        n["task_state"] = json!("completed");
                    });
                }
                "supervisor_authoring" if self.author.load(Ordering::SeqCst) => {
                    self.graph
                        .patch(id, |n| n["task_state"] = json!("completed"));
                }
                _ => {}
            }
        }
    }
    async fn quantum(&self) {
        match self.driver().quantum().await {
            Ok(_) | Err(Error::Overloaded) => {}
            Err(e) => {
                let pool = self.f.db.pool().await;
                let outbox: Vec<(i64, String, Option<Value>)> = sqlx::query_as(
                    "SELECT sequence,status,engine_receipt FROM playbook_outbox ORDER BY sequence",
                )
                .fetch_all(&pool)
                .await
                .unwrap();
                let ops: Vec<(String, String, Value)> = sqlx::query_as(
                    "SELECT command_name,status,request FROM playbook_host_operations",
                )
                .fetch_all(&pool)
                .await
                .unwrap();
                panic!("quantum: {e:?}\noutbox={outbox:?}\nops={ops:?}")
            }
        }
    }
    async fn view(&self, run: &str) -> Value {
        let page = self.driver().inspect_run(run.into()).await.unwrap();
        page["items"][0].clone()
    }
    /// Drives quanta (with the lifecycle) until the run is terminal.
    async fn settle(&self, run: &str) -> Value {
        for _ in 0..400 {
            self.lifecycle();
            self.quantum().await;
            let view = self.view(run).await;
            if matches!(
                view["status"].as_str(),
                Some("completed" | "failed" | "cancelled")
            ) {
                return view;
            }
            // Spawned judgment completions run on the test runtime.
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("run {run} did not settle: {}", self.view(run).await);
    }
    /// Drives quanta until the run has dispatched a `command` effect.
    async fn until_effect(&self, run: &str, command: &str) {
        for _ in 0..200 {
            self.quantum().await;
            if !self.effects(run, command).await.is_empty() {
                return;
            }
        }
        panic!("run {run} never dispatched {command}");
    }
    /// Drives quanta until the run waits on an event (not on an effect).
    async fn until_waiting(&self, run: &str) {
        for _ in 0..200 {
            self.lifecycle();
            self.quantum().await;
            let view = self.view(run).await;
            if view["status"] == "waiting" && view["pending"]["kind"] == "event" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("run {run} never waited: {}", self.view(run).await);
    }
    async fn scalar(&self, sql: &str, run: &str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(self.service.namespace.namespace_id)
            .bind(run)
            .fetch_one(&self.f.db.pool().await)
            .await
            .unwrap()
    }
    /// `playbook_effects` rows of `command` for one run.
    async fn effects(&self, run: &str, command: &str) -> Vec<(String, Value)> {
        sqlx::query_as("SELECT status,receipt FROM playbook_effects WHERE namespace_id=$1 AND run_id=$2 AND command_name=$3 ORDER BY created_at_ms")
            .bind(self.service.namespace.namespace_id)
            .bind(run)
            .bind(command)
            .fetch_all(&self.f.db.pool().await)
            .await
            .unwrap()
    }
    /// Statuses of every `command` effect in the namespace, in order.
    async fn namespace_effects(&self, command: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT status FROM playbook_effects WHERE namespace_id=$1 AND command_name=$2 ORDER BY created_at_ms,intent_id")
            .bind(self.service.namespace.namespace_id)
            .bind(command)
            .fetch_all(&self.f.db.pool().await)
            .await
            .unwrap()
    }
    async fn notifications(&self) -> Vec<(Uuid, String)> {
        let rows: Vec<(Uuid, Value)> = sqlx::query_as(
            "SELECT message_id,definition FROM notification_messages WHERE project_id=$1 AND definition->>'summary' NOT LIKE 'Finding %' ORDER BY created_at_ms,message_id",
        )
        .bind(self.f.selection.lineage.project_id)
        .fetch_all(&self.f.db.pool().await)
        .await
        .unwrap();
        rows.into_iter()
            .map(|(id, d)| (id, d["summary"].as_str().unwrap().to_string()))
            .collect()
    }
}

const PARTITION: &str = "project-health";
impl Pilot {
    /// The trusted host evaluator: start (capturing prior episode revisions)
    /// and complete one health scan.
    async fn scan(&self, name: &str, complete: bool) -> BTreeMap<String, i64> {
        let baseline = self
            .driver()
            .start_health_scan(name, PARTITION, 60_000)
            .await
            .unwrap();
        self.driver()
            .complete_health_scan(name, &format!("eval-{name}"), complete)
            .await
            .unwrap();
        baseline
    }
    async fn episode(&self) -> (i64, bool) {
        sqlx::query_as("SELECT revision,present FROM playbook_finding_episodes WHERE namespace_id=$1 AND finding_key='stuck-work'")
            .bind(self.service.namespace.namespace_id)
            .fetch_one(&self.f.db.pool().await)
            .await
            .unwrap()
    }
    async fn finding_notes(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM playbook_finding_notifications WHERE namespace_id=$1",
        )
        .bind(self.service.namespace.namespace_id)
        .fetch_one(&self.f.db.pool().await)
        .await
        .unwrap()
    }
}

fn outcome(view: &Value) -> (&str, &str) {
    (
        view["status"].as_str().unwrap_or(""),
        view["outcome"].as_str().unwrap_or(""),
    )
}

fn coding_request(remote: &BareRemote, key: &str) -> CodingRequest {
    let head = remote.head();
    let main = format!("refs/heads/main@sha1:{head}");
    CodingRequest {
        subject: json!({"id": format!("issue-{key}"), "revision": 0}),
        context: "Implement the change".into(),
        base: json!({"id": main, "revision": 0}),
        destination: json!({"id": main, "revision": 0}),
        source: json!({"format": "sha1", "hex": head}),
        visibility: "project".into(),
        key: key.into(),
        decision_expires_at_ms: None,
    }
}

fn required_checks() -> Vec<RequiredCheck> {
    let v = validator();
    vec![RequiredCheck {
        id: "cargo-test".into(),
        validator_id: v.id,
        validator_version: v.version,
        validator_digest: v.digest,
    }]
}

/// One end-to-end pass; its (debug-build) future is large, so it runs on a
/// dedicated thread with a bigger stack.
#[test]
fn real_adapter_pilots_complete() {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(Box::pin(pilots_end_to_end()))
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn pilots_end_to_end() {
    let mut p = Pilot::new().await;
    let grant = p.grant;

    // Exactly twelve production commands, equal to the pinned registry the
    // service runs on (catalog 2.1.0, Host8; same command set as 2.0.0).
    let registry = CatalogProfile::Host8.registry();
    let registered: BTreeSet<&str> = HOST7_COMMANDS.iter().copied().collect();
    let pinned: BTreeSet<&str> = registry.commands.keys().map(String::as_str).collect();
    assert_eq!(HOST7_COMMANDS.len(), 12);
    assert_eq!(registered, pinned);
    let capabilities: BTreeSet<String> = registry
        .commands
        .values()
        .flat_map(|c| c.capabilities.clone())
        .collect();
    assert_eq!(
        capabilities,
        BTreeSet::from(HOST7_CAPABILITIES.map(String::from))
    );

    // ------------------------------------------------- coding (D9 policy)
    let policy = CodingPolicy::project_default(required_checks());
    let coding = policy.build().unwrap();
    let rev = p.install(&coding.install_order(), "coding", grant).await;
    let run = p
        .admit(
            "coding",
            rev,
            grant,
            policy.input(&coding_request(&p.remote, "coding-1")),
        )
        .await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "published"), "{view}");
    // Effects belong to the child attempt run; count them in the namespace.
    assert_eq!(
        p.namespace_effects("candidate.publish").await,
        ["published"]
    );
    assert_eq!(p.namespace_effects("candidate.prepare").await, ["prepared"]);
    let prepared: Value = sqlx::query_scalar("SELECT detail FROM playbook_integration_receipts WHERE namespace_id=$1 AND command_name='candidate.prepare' AND state='prepared'")
        .bind(p.service.namespace.namespace_id)
        .fetch_one(&p.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(
        p.remote.head(),
        prepared["candidate"]["hex"].as_str().unwrap()
    );
    assert_eq!(p.checks.started.load(Ordering::SeqCst), 1);
    let implementation = p.graph.by_key("coding-1/a0/work");

    // ------------------------------------------------------------ routing
    let routing = pilots::routing().unwrap();
    let rev = p.install(&[&routing], "routing", grant).await;
    let attempt = p.attempt;
    let work = |p: &Pilot| {
        let v = p.graph.node(implementation)["version"].clone();
        json!({"id": implementation.to_string(), "revision": v})
    };
    // Complete facts: the observed selection is pinned, no judgment.
    let run = p
        .admit(
            "routing",
            rev,
            grant,
            json!({"attempt": attempt.to_string(), "work": work(&p)}),
        )
        .await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "routed"), "{view}");
    assert_eq!(p.effects(&run, "judgment.request").await.len(), 0);
    let selection: String = sqlx::query_scalar(
        "SELECT selection_digest FROM intelligence_attempt_selections WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&p.f.db.pool().await)
    .await
    .unwrap();
    assert_eq!(
        p.graph.node(implementation)["metadata"]["aq:execution_constraints"],
        json!(selection)
    );
    // Missing facts: exactly one judgment, its deterministic choice applied.
    let run = p
        .admit(
            "routing",
            rev,
            grant,
            json!({"attempt": Uuid::new_v4().to_string(), "work": work(&p)}),
        )
        .await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "routed"), "{view}");
    assert_eq!(p.effects(&run, "judgment.request").await.len(), 1);
    assert_eq!(p.model.calls.load(Ordering::SeqCst), 1);
    let judged = p.graph.node(implementation)["metadata"]["aq:execution_constraints"].clone();
    let judgment = judged.as_str().unwrap().strip_prefix("judgment:").unwrap();
    let choice: Value = sqlx::query_scalar(
        "SELECT output->'choice' FROM playbook_host_operations WHERE operation_id=$1 AND status='assessed'",
    )
    .bind(Uuid::parse_str(judgment).unwrap())
    .fetch_one(&p.f.db.pool().await)
    .await
    .unwrap();
    assert_eq!(choice, json!("isolated"), "deterministic model choice");
    // Current-state check: a concurrent change after the read is a
    // version refusal, never a silent overwrite.
    *p.graph.bump_after_read.lock().unwrap() = Some(implementation);
    let before = p.graph.node(implementation)["metadata"]["aq:execution_constraints"].clone();
    let run = p
        .admit(
            "routing",
            rev,
            grant,
            json!({"attempt": attempt.to_string(), "work": work(&p)}),
        )
        .await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "stale"), "{view}");
    assert_eq!(
        p.graph.node(implementation)["metadata"]["aq:execution_constraints"],
        before
    );
    assert!(p.graph.applied_changes().iter().all(|n| *n == 1));

    // ------------------------------------------- GATE-05: lost response
    // The graph applies the change and the reply is lost: the effect stays
    // pending; explicit recovery resumes it with the same idempotency key.
    let changes = p.graph.applied_changes().len();
    p.graph.lose_next_reply.store(true, Ordering::SeqCst);
    let run = p
        .admit(
            "routing",
            rev,
            grant,
            json!({"attempt": attempt.to_string(), "work": work(&p)}),
        )
        .await;
    p.until_effect(&run, "work.change").await;
    assert_eq!(
        p.effects(&run, "work.change").await[0].0,
        "pending",
        "lost reply leaves the intent pending"
    );
    p.driver().recover().await.unwrap();
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "routed"), "{view}");
    assert_eq!(p.graph.applied_changes().len(), changes + 1);
    assert!(p.graph.applied_changes().iter().all(|n| *n == 1));

    // ---------- GATE-05: crash between the domain effect and the receipt
    // The AQ effect row commits, the runner never records it, the owner is
    // lost; a new owner recovers and observes the retained result.
    p.driver().test_lose_next_host_reply("work.change");
    let changes = p.graph.applied_changes().len();
    let run = p
        .admit(
            "routing",
            rev,
            grant,
            json!({"attempt": attempt.to_string(), "work": work(&p)}),
        )
        .await;
    p.until_effect(&run, "work.change").await;
    p.quantum().await;
    p.restart().await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "routed"), "{view}");
    assert_eq!(p.graph.applied_changes().len(), changes + 1);
    assert!(p.graph.applied_changes().iter().all(|n| *n == 1));
    let effects = p.effects(&run, "work.change").await;
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].0, "changed");

    // A fresh namespace per group keeps engine history (and so the
    // pre-PB-04 per-transition cost) bounded.
    p.driver.take().unwrap().stop().await.unwrap();
    let mut p = Pilot::new().await;
    let (actor, grant) = (p.f.actor, p.grant);
    let recipient = format!("user:{actor}");

    // Fake notification destination behind the real notifications owner:
    // the first delivery is rate limited, the retry is accepted.
    let destination = Destination::new(vec![
        (429, r#"{"retry_after":1,"global":false}"#),
        (200, r#"{"id":"1","webhook_id":"42"}"#),
    ])
    .await;
    let mut notify = aq_server::notifications::api::service(&p.f.state);
    notify.public_origin = "https://aq.example".into();
    notify.configuration = Some(Arc::new(Loaded::fixture(
        1,
        "discord-d11",
        1,
        &destination.url,
    )));
    let dispatcher = Uuid::new_v4();
    notify.install(dispatcher).await.unwrap();
    notify
        .set_route(
            actor,
            RouteRequest {
                operation_id: Uuid::new_v4(),
                expected_revision: 0,
                route: aq_notifications::Route {
                    route_id: Uuid::new_v4(),
                    revision: 1,
                    scope: aq_lifecycle_api::Scope {
                        project_id: p.f.selection.lineage.project_id,
                        workspace_id: p.f.selection.lineage.workspace_id,
                    },
                    recipient_user_id: actor,
                    endpoint_key: "discord-d11".into(),
                    endpoint_revision: 1,
                    enabled: true,
                    retry: aq_notifications::RetryPolicy {
                        max_attempts: 3,
                        max_elapsed_ms: 600_000,
                        initial_delay_ms: 250,
                        max_delay_ms: 2000,
                        allow_duplicate_risk: false,
                    },
                },
            },
        )
        .await
        .unwrap();

    // ------------------------------------------------------------- report
    let report = pilots::report(30_000).unwrap();
    let rev = p.install(&[&report], "report", grant).await;
    // Authored by the deadline.
    let run = p
        .admit(
            "report",
            rev,
            grant,
            json!({"identity": "report-window-1", "key": "author-1", "recipient": recipient}),
        )
        .await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "authored"), "{view}");
    let author = p.graph.by_key("author-1");
    let sent = p.effects(&run, "message.send").await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "queued");
    let messages = p.notifications().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].1, format!("aq.playbook.work/{author}"));
    // Deadline: the deterministic fallback; the late author is retained and
    // never sent a second time.
    p.author.store(false, Ordering::SeqCst);
    let run = p
        .admit(
            "report",
            rev,
            grant,
            json!({"identity": "report-window-2", "key": "author-2", "recipient": recipient}),
        )
        .await;
    p.until_waiting(&run).await;
    p.advance(31_000);
    // The fallback is sent at the deadline; the run then finishes only once
    // its owned author operation settles.
    for _ in 0..200 {
        p.quantum().await;
        if p.view(&run).await["step"] == "fallback_sent" {
            break;
        }
    }
    let view = p.view(&run).await;
    assert_eq!(view["step"], "fallback_sent", "{view}");
    assert_ne!(view["status"], "completed", "{view}");
    assert_eq!(p.effects(&run, "message.send").await.len(), 1);
    p.author.store(true, Ordering::SeqCst);
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "fallback"), "{view}");
    let late = p.graph.by_key("author-2");
    let retained = p
        .scalar(
            "SELECT count(*) FROM playbook_host_operations o JOIN playbook_effects e ON e.namespace_id=o.namespace_id AND e.intent_id=o.intent_id WHERE o.namespace_id=$1 AND e.run_id=$2 AND o.command_name='work.ensure' AND o.status='completed'",
            &run,
        )
        .await;
    assert_eq!(retained, 1, "late author completion retained");
    assert_eq!(p.graph.node(late)["task_state"], "completed");
    assert_eq!(p.effects(&run, "message.send").await.len(), 1);
    let messages = p.notifications().await;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].1, pilots::REPORT_FALLBACK);
    assert_eq!(p.graph.kinds("supervisor_authoring"), 2);

    // ------------------------------------------ GATE-05: notification retry
    let client = aq_notifications::discord::Discord::new().unwrap();
    for _ in 0..6 {
        notify.tick(dispatcher, &client).await.unwrap();
        p.advance(5_000);
    }
    let accepted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_deliveries d JOIN notification_messages m USING(message_id) WHERE m.project_id=$1",
    )
    .bind(p.f.selection.lineage.project_id)
    .fetch_one(&p.f.db.pool().await)
    .await
    .unwrap();
    assert_eq!(accepted, 2, "one delivery per report");
    for (message, _) in &messages {
        let view = notify.inspect(actor, *message).await.unwrap();
        assert!(view.to_string().contains("accepted_by_transport"), "{view}");
    }
    assert_eq!(
        destination.calls.load(Ordering::SeqCst),
        3,
        "one rate-limited attempt, its retry, and the second report"
    );

    // ----------------------------------------- GATE-05: revoked authority
    // A second grant's report run waits for its author; the grant is
    // revoked; the next effect (message.send) is refused before any effect
    // row exists.
    let revocable = Uuid::new_v4();
    p.service
        .grant(actor, revocable, scope(&p.f), Pilot::far(&p.f))
        .await
        .unwrap();
    let rev = p.install(&[&report], "report-revocable", revocable).await;
    p.author.store(false, Ordering::SeqCst);
    let run = p
        .admit(
            "report-revocable",
            rev,
            revocable,
            json!({"identity": "report-window-3", "key": "author-3", "recipient": recipient}),
        )
        .await;
    p.until_waiting(&run).await;
    p.service.revoke(actor, revocable).await.unwrap();
    p.author.store(true, Ordering::SeqCst);
    for _ in 0..12 {
        p.lifecycle();
        p.quantum().await;
    }
    let view = p.view(&run).await;
    assert!(
        !matches!(outcome(&view).1, "authored" | "fallback"),
        "{view}"
    );
    assert!(p.effects(&run, "message.send").await.is_empty());
    assert_eq!(p.notifications().await.len(), 2);

    // A fresh namespace per group keeps engine history (and so the
    // pre-PB-04 per-transition cost) bounded.
    p.driver.take().unwrap().stop().await.unwrap();
    let mut p = Pilot::new().await;
    let grant = p.grant;

    // ------------------------------------------------------------ watcher
    let watcher = pilots::watcher().unwrap();
    let rev = p.install(&[&watcher], "watcher", grant).await;
    let finding = |expected: i64, evidence: &str| json!([{"key": "stuck-work", "evidence": evidence, "expected_revision": expected}]);
    let watch = |name: &str, findings: Value, reminder: i64, page: i64| {
        json!({"partition": PARTITION, "observation": name, "evaluation": format!("eval-{name}"),
            "findings": findings, "reminder_ms": reminder, "page": page})
    };
    // Opens the episode (prior revision 0 captured by the evaluator).
    assert!(p.scan("scan-1", true).await.is_empty());
    let run = p
        .admit(
            "watcher",
            rev,
            grant,
            watch("scan-1", finding(0, "idle 2h"), 60_000, 0),
        )
        .await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "reconciled"), "{view}");
    assert_eq!(p.episode().await, (1, true));
    assert_eq!(p.finding_notes().await, 1);
    // A stale baseline (expected revision 0; the episode is at 1) is a
    // conflict and leaves the alert open.
    assert_eq!(p.scan("scan-2", true).await["stuck-work"], 1);
    let run = p
        .admit(
            "watcher",
            rev,
            grant,
            watch("scan-2", finding(0, "idle 3h"), 60_000, 0),
        )
        .await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "conflict"), "{view}");
    assert_eq!(p.episode().await, (1, true));
    // An incomplete scan reporting nothing never clears the alert.
    p.scan("scan-3", false).await;
    let run = p
        .admit("watcher", rev, grant, watch("scan-3", json!([]), 60_000, 0))
        .await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "reconciled"), "{view}");
    assert_eq!(p.episode().await, (1, true), "incomplete scan cannot clear");
    // A missing page never reaches findings.reconcile.
    p.scan("scan-4", true).await;
    let run = p
        .admit("watcher", rev, grant, watch("scan-4", json!([]), 60_000, 1))
        .await;
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "incomplete"), "{view}");
    assert!(p.effects(&run, "findings.reconcile").await.is_empty());
    assert_eq!(p.episode().await, (1, true));
    // Reminder cadence is the run's parameter: unchanged evidence notifies
    // again only once `reminder_ms` has elapsed since the last notice.
    let notes = p.finding_notes().await;
    p.advance(61_000);
    for (name, reminder, extra) in [("scan-5", 300_000, 0), ("scan-6", 60_000, 1)] {
        let revision = p.episode().await.0;
        p.scan(name, true).await;
        let run = p
            .admit(
                "watcher",
                rev,
                grant,
                watch(name, finding(revision, "idle 2h"), reminder, 0),
            )
            .await;
        let view = p.settle(&run).await;
        assert_eq!(outcome(&view), ("completed", "reconciled"), "{view}");
        assert_eq!(
            p.finding_notes().await,
            notes + extra,
            "reminder_ms={reminder}"
        );
    }
    // Only a complete current scan without the finding clears it.
    let revision = p.episode().await.0;
    p.scan("scan-7", true).await;
    let run = p
        .admit("watcher", rev, grant, watch("scan-7", json!([]), 60_000, 0))
        .await;
    assert_eq!(outcome(&p.settle(&run).await), ("completed", "reconciled"));
    assert_eq!(p.episode().await, (revision + 1, false));
    assert_eq!(p.finding_notes().await, notes + 2);

    // A fresh namespace per group keeps engine history (and so the
    // pre-PB-04 per-transition cost) bounded.
    p.driver.take().unwrap().stop().await.unwrap();
    let mut p = Pilot::new().await;
    let (actor, grant) = (p.f.actor, p.grant);
    let recipient = format!("user:{actor}");

    // --------------------------------------------- decision.request (Host8)
    let decision_policy = CodingPolicy {
        human_decision: Some(HumanDecision {
            question: "Ship it?".into(),
            responders: vec![recipient.clone()],
        }),
        ..policy.clone()
    };
    let decision = decision_policy.build().unwrap().decision.unwrap();
    let rev = p.install(&[&decision], "decision", grant).await;
    let run = p
        .admit(
            "decision",
            rev,
            grant,
            json!({"question": "Ship it?", "choices": ["approved", "rejected"],
                "responders": [recipient], "subject": {"id": "candidate-x", "revision": 1},
                "expires_at_ms": p.now() + 3_600_000}),
        )
        .await;
    p.until_waiting(&run).await;
    let handle: Uuid = sqlx::query_scalar("SELECT operation_id FROM playbook_host_operations WHERE namespace_id=$1 AND command_name='decision.request'")
        .bind(p.service.namespace.namespace_id)
        .fetch_one(&p.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(
        p.service
            .answer_decision(Uuid::new_v4(), handle, "approved")
            .await,
        Err(Error::Forbidden),
        "only a listed responder answers"
    );
    let answer = p
        .service
        .answer_decision(actor, handle, "approved")
        .await
        .unwrap();
    assert_eq!(answer.outcome, "approved");
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "approved"), "{view}");

    // --------------------------- GATE-05: changed base revision (coding)
    // The remote's main moves after the candidate is prepared: publication
    // is a conflict, the moved ref is never overwritten, and no repair
    // budget remains.
    let strict = CodingPolicy {
        max_repairs: 0,
        ..policy.clone()
    };
    let strict_artifacts = strict.build().unwrap();
    let rev = p
        .install(&strict_artifacts.install_order(), "coding-strict", grant)
        .await;
    p.move_base_on_review.store(true, Ordering::SeqCst);
    let run = p
        .admit(
            "coding-strict",
            rev,
            grant,
            strict.input(&coding_request(&p.remote, "coding-2")),
        )
        .await;
    let moved_from = p.remote.head();
    let view = p.settle(&run).await;
    assert_eq!(outcome(&view), ("completed", "exhausted"), "{view}");
    assert_ne!(p.remote.head(), moved_from, "external move happened");
    let external = p.remote.head();
    assert_eq!(p.namespace_effects("candidate.publish").await, ["conflict"]);
    assert_eq!(p.remote.head(), external, "moved base never overwritten");

    // ------------------------------------------------------------ evidence
    let artifacts: BTreeMap<&str, &str> = [
        ("aq.coding", coding.root.digest.as_str()),
        ("aq.coding.attempt", coding.attempt.digest.as_str()),
        ("aq.coding.work", coding.work.digest.as_str()),
        ("aq.coding.check", coding.check.digest.as_str()),
        ("aq.coding.decision", decision.digest.as_str()),
        (
            "aq.coding (max_repairs=0)",
            strict_artifacts.root.digest.as_str(),
        ),
        (pilots::ROUTING, routing.digest.as_str()),
        (pilots::WATCHER, watcher.digest.as_str()),
        (pilots::REPORT, report.digest.as_str()),
    ]
    .into_iter()
    .collect();
    let source: Value = serde_json::from_str(include_str!(
        "../../../vendor/aq-playbooks-source-0.1.3.json"
    ))
    .unwrap();
    let evidence = json!({
        "aq_server": env!("CARGO_PKG_VERSION"),
        "aq_playbook_host": source["source_revision"],
        "catalog": {"version": HOST8_VERSION, "fingerprint": HOST8_FINGERPRINT},
        "registered_commands": HOST7_COMMANDS,
        "artifacts": artifacts,
    });
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("d11-evidence.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    println!("D11 evidence ({}):\n{evidence:#}", path.display());
    p.driver.take().unwrap().stop().await.unwrap();
}
