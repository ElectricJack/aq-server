use crate::common::native_offer::{Case, assembled, poll, poll_offered, setup};
use aq_admission::{
    Label, SourceSelection,
    control::{ControlReply, decode_reply},
    startup::NativeOffer,
};
use aq_delivery::main_sync::{self, MainSync, Scope};
use aq_server::{agent_access, source_retention::Runtime};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    task::JoinHandle,
};
use uuid::Uuid;

const OLD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const MERGE: &str = "dddddddddddddddddddddddddddddddddddddddd";
const SECOND_MERGE: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const UNRELATED: &str = "ffffffffffffffffffffffffffffffffffffffff";

struct Fixture {
    case: Case,
    remote: Arc<Mutex<&'static str>>,
    requests: Arc<Mutex<Vec<Value>>>,
    // Reveal the blockers after the first context read to exercise the final fence.
    late_blockers: Arc<AtomicBool>,
    context_reads: Arc<AtomicUsize>,
    unavailable_on_old_base: Arc<AtomicBool>,
    blockers: [Uuid; 6],
    tasks: [JoinHandle<()>; 2],
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Fixture {
    async fn new(named_source: bool, sibling_ready: bool) -> Self {
        let mut case = setup().await;
        let lineage = &case.env.lineage;
        let (workspace, task, sibling) = (lineage.workspace_id, lineage.task_id, case.second_task);
        let repository = case.env.frozen.project.binding.repository_id;
        let root: Value = reqwest::get(format!(
            "{}/v1/workspaces/{workspace}/nodes/{task}",
            case.graph_url
        ))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        let blockers = std::array::from_fn(|_| Uuid::new_v4());
        let [first, second, other_repo, report, parent, outgoing] = blockers;
        let mut sibling_node = root.clone();
        sibling_node["id"] = json!(sibling);
        sibling_node["title"] = json!("Independent sibling");
        sibling_node["metadata"] = json!({"aq:priority": -1});
        let nodes: Vec<_> = blockers
            .iter()
            .map(|id| {
                let mut node = root.clone();
                node["id"] = json!(id);
                node["task_state"] = json!("completed");
                node["title"] = json!(format!("Completed graph task {id}"));
                node
            })
            .collect();
        let edge = |source: Uuid, target: Uuid, kind: &str| {
            json!({
                "id":Uuid::new_v4(), "workspace_id":workspace, "source":source, "target":target,
                "edge_type":kind, "source_revision":null, "target_revision":null, "version":1,
                "deleted":false, "created_at":"2026-01-01T00:00:00Z", "description":null,
            })
        };
        let edges = vec![
            edge(first, task, "blocks"),
            edge(second, task, "blocks"),
            edge(other_repo, task, "blocks"),
            edge(report, task, "blocks"),
            edge(parent, task, "parent-child"),
            edge(task, outgoing, "blocks"),
        ];
        let sibling_context = json!({"node":sibling_node, "nodes":[nodes[4]],
            "edges":[edge(parent, sibling, "parent-child")], "pinned_revisions":[], "truncated":false});
        let full_context = json!({"node":root, "nodes":nodes, "edges":edges,
            "pinned_revisions":[], "truncated":false});
        let initial_context = json!({"node":root, "nodes":[], "edges":[],
            "pinned_revisions":[], "truncated":false});
        let context_reads = Arc::new(AtomicUsize::new(0));
        let late_blockers = Arc::new(AtomicBool::new(false));
        let (peer_reads, peer_late) = (context_reads.clone(), late_blockers.clone());
        let graph =
            axum::Router::new().fallback(axum::routing::get(move |uri: axum::http::Uri| {
                let reply = if uri.path().ends_with("/ready") {
                    let mut items = vec![&root];
                    items.extend(sibling_ready.then_some(&sibling_node));
                    json!({"items":items,"next_cursor":null})
                } else if uri.path().ends_with(&format!("/nodes/{task}/context")) {
                    match peer_reads.fetch_add(1, Ordering::SeqCst) {
                        0 if peer_late.load(Ordering::SeqCst) => initial_context.clone(),
                        _ => full_context.clone(),
                    }
                } else if uri.path().ends_with(&format!("/nodes/{sibling}/context")) {
                    sibling_context.clone()
                } else if uri.path().ends_with("/nodes") {
                    json!({"items":[], "next_cursor":null})
                } else {
                    root.clone()
                };
                async move { ([("X-AQ-Graph-API-Version", "1")], axum::Json(reply)) }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let graph_task = tokio::spawn(async move { axum::serve(listener, graph).await.unwrap() });
        let state = &mut case.env.env.state;
        state.agent_access = Some(Arc::new(
            agent_access::Runtime::new(&agent_access::AgentAccessConfig {
                graph_url: format!("http://{addr}"),
                graph_token_file: case.env.env._dir.path().join("graph-token"),
                credential_ttl_ms: 10000,
                mutation_actor_id: None,
            })
            .await
            .unwrap(),
        ));
        if named_source {
            let mut config = (*state.config).clone();
            config.delivery = Some(aq_delivery::github::DeliveryConfig {
                project_allowlist: [lineage.project_id].into(),
                task_allowlist: Some([task].into()),
                git_http_socket: Some(case.env.env._dir.path().join("ancestry-git-http.sock")),
                ..Default::default()
            });
            let profiled = Arc::make_mut(config.profiled_execution.as_mut().unwrap());
            profiled.revision += 1;
            profiled.projects[0].source = SourceSelection::NamedRef {
                reference: Label::new("refs/heads/main").unwrap(),
            };
            state.config = Arc::new(config);
            aq_server::profiled_execution::policies::install(state)
                .await
                .unwrap();
        }

        let mut config = state.source_retention.as_ref().unwrap().config.clone();
        let original_socket = config.git_socket.clone();
        config.git_socket = case.env.env._dir.path().join("ancestry.sock");
        let listener = tokio::net::UnixListener::bind(&config.git_socket).unwrap();
        state.source_retention = Some(Arc::new(Runtime::new(config).unwrap()));
        let remote = Arc::new(Mutex::new(OLD));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let unavailable_on_old_base = Arc::new(AtomicBool::new(false));
        let (peer_remote, peer_requests, peer_unavailable) = (
            remote.clone(),
            requests.clone(),
            unavailable_on_old_base.clone(),
        );
        let clock = state.clock.clone();
        let now = clock().timestamp_millis() as u64;
        let git_task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(socket);
                let mut line = Vec::new();
                stream.read_until(b'\n', &mut line).await.unwrap();
                let request: Value = serde_json::from_slice(&line).unwrap();
                peer_requests.lock().unwrap().push(request.clone());
                let params = &request["params"];
                let mut reply =
                    json!({"protocol_version":1, "request_id":request["request_id"], "ok":true});
                if let Some(operation) = request.get("operation_id") {
                    reply["operation_id"] = operation.clone();
                }
                let result = match request["operation"].as_str().unwrap() {
                    "repo.observation" => {
                        assert_eq!(
                            params,
                            &json!({"refs":["refs/heads/main"],"max_age_ms":30000})
                        );
                        let now = clock().timestamp_millis();
                        json!({"observation_id":Uuid::new_v4(),"binding_generation":1,
                            "started_at_ms":now,"completed_at_ms":now,"refs":{"refs/heads/main":OLD},
                            "covered_refs":["refs/heads/main"],"last_refresh_error":null})
                    }
                    "repo.retain_ref" => {
                        assert_eq!(params, &json!({"ref":"refs/heads/main"}));
                        let head = *peer_remote.lock().unwrap();
                        json!({"ref":"refs/heads/main","head":oid(head),
                            "retention_handle":handle(repository, head),"synced_at_ms":clock().timestamp_millis()})
                    }
                    "contains" => {
                        assert_eq!(request["repository_id"], json!(repository));
                        assert_eq!(request["expected_binding_generation"], 1);
                        let ancestor = params["ancestor"]["hex"].as_str().unwrap();
                        let base = params["descendant"]["hex"].as_str().unwrap();
                        assert!(matches!(ancestor, MERGE | SECOND_MERGE), "{request}");
                        assert_eq!(
                            params,
                            &json!({"ancestor":oid(ancestor), "descendant":oid(base)})
                        );
                        if base == OLD && peer_unavailable.load(Ordering::SeqCst) {
                            reply["ok"] = json!(false);
                            reply["error"] = json!({"code":"object_missing","message":"merge is not local",
                                "details":{},"retry":"refresh_inputs"});
                            write_reply(stream, &reply).await;
                            continue;
                        }
                        json!({"contains":base == NEW,"semantics":"strict_commit_ancestry_v1"})
                    }
                    "inspect" => json!({"oid":params["oid"], "tree":oid(NEW)}),
                    "retention.inspect"
                        if [OLD, NEW]
                            .into_iter()
                            .any(|head| params["handle_id"] == json!(handle(repository, head))) =>
                    {
                        let head = [OLD, NEW]
                            .into_iter()
                            .find(|head| params["handle_id"] == json!(handle(repository, head)))
                            .unwrap();
                        let id = handle(repository, head);
                        json!({"version":1,"id":id,"repository_id":repository,"binding_generation":1,
                            "generation":1,"state":"active","refs":[{"name":format!("refs/aq-git/checkpoints/{id}"),"oid":head}],
                            "release_operation":null,"release_reason":null})
                    }
                    _ => {
                        let mut upstream =
                            BufReader::new(UnixStream::connect(&original_socket).await.unwrap());
                        upstream.get_mut().write_all(&line).await.unwrap();
                        line.clear();
                        upstream.read_until(b'\n', &mut line).await.unwrap();
                        stream.get_mut().write_all(&line).await.unwrap();
                        continue;
                    }
                };
                reply["result"] = result;
                write_reply(stream, &reply).await;
            }
        });
        if named_source {
            let scope = Scope::new(lineage.project_id, repository, 1, "refs/heads/main");
            main_sync::record(
                &case.env.env.pool,
                &MainSync {
                    schema_version: 1,
                    project_id: scope.project_id,
                    repository_id: scope.repository_id,
                    binding_generation: 1,
                    reference: scope.reference,
                    head: OLD.into(),
                    retention_handle: handle(repository, OLD),
                    synced_at_ms: now - 1,
                },
                now - 1,
            )
            .await
            .unwrap();
        }
        Self {
            case,
            remote,
            requests,
            late_blockers,
            context_reads,
            unavailable_on_old_base,
            blockers,
            tasks: [graph_task, git_task],
        }
    }

    async fn insert_merges(&self, same_repository_blockers: bool) {
        let repository = self.case.env.frozen.project.binding.repository_id;
        let project = self.case.env.lineage.project_id;
        let [first, second, other_repo, _, parent, outgoing] = self.blockers;
        for (task, repo, sha) in [
            (first, repository, MERGE),
            (second, repository, SECOND_MERGE),
            (other_repo, Uuid::new_v4(), UNRELATED),
            (parent, repository, UNRELATED),
            (outgoing, repository, UNRELATED),
        ] {
            if !same_repository_blockers && [first, second].contains(&task) {
                continue;
            }
            sqlx::query("INSERT INTO delivery_merge_evidence(operation_id,project_id,task_id,pull_request,target,target_ref,expected_head_sha,merge_sha,observed_at_ms)
                        VALUES($1,$2,$3,1,$4,'refs/heads/main',$5,$6,1)")
                .bind(Uuid::new_v4()).bind(project).bind(task)
                .bind(json!({"git":{"repository_id":repo,"binding_generation":1},"github":{"owner":"owner","name":"repo"},"default_branch":"main"}))
                .bind(UNRELATED).bind(sha).execute(&self.case.env.env.pool).await.unwrap();
        }
    }

    fn operations(&self, operation: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["operation"] == operation)
            .map(|request| request["params"].clone())
            .collect()
    }

    async fn refusal(&self) -> Option<Value> {
        aq_role_settings::refusals::shown(&self.case.env.env.pool, self.case.env.lineage.task_id)
            .await
    }

    async fn granted_base(&self, offer: &NativeOffer) -> Option<String> {
        sqlx::query_scalar("SELECT base FROM delivery_git_grants WHERE attempt_id=$1")
            .bind(offer.offered.lifecycle_binding.attempt_id)
            .fetch_optional(&self.case.env.env.pool)
            .await
            .unwrap()
    }

    fn unenroll_tasks(&mut self) {
        let mut config = (*self.case.env.env.state.config).clone();
        config.delivery.as_mut().unwrap().task_allowlist = Some(Default::default());
        self.case.env.env.state.config = Arc::new(config);
    }

    async fn offer(&self) -> NativeOffer {
        let (status, body) = poll_offered(&self.case).await;
        assert_eq!(status, 200, "{body}");
        let ControlReply::Poll(mut page) =
            decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
        else {
            panic!("{body}")
        };
        assert_eq!(page.offers.len(), 1, "{body}");
        page.offers.remove(0)
    }
}

fn oid(hex: &str) -> Value {
    json!({"algorithm":"sha1","hex":hex})
}

fn handle(repository: Uuid, head: &str) -> Uuid {
    Uuid::new_v5(&repository, head.as_bytes())
}

async fn write_reply(mut stream: BufReader<UnixStream>, reply: &Value) {
    let mut bytes = serde_json::to_vec(reply).unwrap();
    bytes.push(b'\n');
    stream.get_mut().write_all(&bytes).await.unwrap();
}

#[tokio::test]
async fn missing_merge_refreshes_fresh_cache_and_an_independent_sibling_can_offer() {
    let f = Fixture::new(true, true).await;
    f.insert_merges(true).await;
    let offer = f.offer().await;
    assert_eq!(offer.offered.lifecycle_binding.task_id, f.case.second_task);
    assert_eq!(f.granted_base(&offer).await, None);
    assert!(!f.operations("repo.observation").is_empty());
    assert_eq!(f.operations("repo.retain_ref").len(), 1);
    assert_eq!(
        f.operations("contains").len(),
        2,
        "checked both cached and refreshed head"
    );
    assert_eq!(
        f.operations("artifact.export").len(),
        1,
        "only the sibling exported"
    );
    let reason = f.refusal().await.unwrap();
    assert_eq!(reason["code"], "delivery_ancestry_wait");
    assert_eq!(reason["wait"]["reason"], "merge_missing_from_base");
    assert_eq!(reason["wait"]["base"], OLD);
}

#[tokio::test]
async fn cached_export_cannot_bypass_a_new_blocker_and_replay_keeps_the_verified_offer() {
    let mut f = Fixture::new(true, false).await;
    f.insert_merges(true).await;
    f.late_blockers.store(true, Ordering::SeqCst);
    for (expected_retains, expected_wait) in
        [(1, "merge_missing_from_base"), (2, "ancestry_unavailable")]
    {
        f.unavailable_on_old_base
            .store(expected_retains == 2, Ordering::SeqCst);
        let (status, body) = poll(&f.case).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["body"]["offers"], json!([]));
        assembled(&f.case).await;
        assert_eq!(f.operations("repo.retain_ref").len(), expected_retains);
        assert_eq!(f.operations("artifact.export").len(), 1);
        let refusal = f.refusal().await.unwrap();
        assert_eq!(refusal["wait"]["reason"], expected_wait);
        assert_eq!(refusal["wait"]["base"], OLD);
    }
    // Revisit the exact verified pre-export on the same head. The final Graph
    // read still introduces the blocker, and no new export is necessary.
    f.context_reads.store(0, Ordering::SeqCst);
    let (status, body) = poll(&f.case).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["body"]["offers"], json!([]));
    assembled(&f.case).await;
    assert_eq!(f.operations("artifact.export").len(), 1);
    let retained = || async {
        sqlx::query_as::<_, (String, String)>("SELECT state, convert_from(request_bytes,'UTF8')::jsonb #>> '{base,hex}' FROM profiled_source_exports ORDER BY 2")
            .fetch_all(&f.case.env.env.pool).await.unwrap()
    };
    assert_eq!(retained().await, [("verified".into(), OLD.into())]);

    // aq-git may not know the prerequisite object until it syncs from origin.
    f.unavailable_on_old_base.store(true, Ordering::SeqCst);
    *f.remote.lock().unwrap() = NEW;
    let offered = f.offer().await;
    assert_eq!(
        offered.offered.lifecycle_binding.task_id,
        f.case.env.lineage.task_id
    );
    assert_eq!(
        f.operations("artifact.export")
            .iter()
            .map(|params| &params["base"]["hex"])
            .collect::<Vec<_>>(),
        vec![&json!(OLD), &json!(NEW)]
    );
    assert_eq!(
        retained().await,
        [
            ("verified".into(), OLD.into()),
            ("verified".into(), NEW.into())
        ]
    );
    let offered_base: String = sqlx::query_scalar(
        "SELECT convert_from(e.request_bytes,'UTF8')::jsonb #>> '{base,hex}'
         FROM profiled_launch_source_exports l JOIN profiled_source_exports e USING(operation_id)
         WHERE l.attempt_id=$1",
    )
    .bind(offered.offered.lifecycle_binding.attempt_id)
    .fetch_one(&f.case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(offered_base, NEW);
    assert_eq!(f.granted_base(&offered).await.as_deref(), Some(NEW));
    for sha in [MERGE, SECOND_MERGE] {
        assert!(
            f.operations("contains")
                .contains(&json!({"ancestor":oid(sha), "descendant":oid(NEW)}))
        );
    }
    assert!(f.refusal().await.is_none());
    let before_replay = f.requests.lock().unwrap().len();
    f.unenroll_tasks();
    let replay = f.offer().await;
    assert_eq!(replay.offered.offer, offered.offered.offer);
    assert_eq!(f.granted_base(&replay).await.as_deref(), Some(NEW));
    assert_eq!(f.requests.lock().unwrap().len(), before_replay);
}

#[tokio::test]
async fn immutable_legacy_source_keeps_its_offer_and_export_identity() {
    let f = Fixture::new(false, false).await;
    f.insert_merges(false).await;
    let offered = f.offer().await;
    assert_eq!(
        offered.offered.lifecycle_binding.task_id,
        f.case.env.lineage.task_id
    );
    assert!(f.operations("contains").is_empty());
    assert!(f.operations("repo.retain_ref").is_empty());
    assert_eq!(f.operations("artifact.export")[0]["base"], oid(OLD));
    assert_eq!(f.offer().await.offered.offer, offered.offered.offer);
    assert_eq!(f.granted_base(&offered).await, None);
    assert_eq!(f.operations("artifact.export").len(), 1);
    assert!(f.refusal().await.is_none());
}

#[tokio::test]
async fn unenrolled_named_source_refreshes_its_legacy_base_without_a_git_grant() {
    let mut f = Fixture::new(true, false).await;
    f.unenroll_tasks();
    f.insert_merges(true).await;
    let (status, body) = poll(&f.case).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["body"]["offers"], json!([]));
    assembled(&f.case).await;
    assert_eq!(f.operations("repo.retain_ref").len(), 1);
    assert!(f.operations("artifact.export").is_empty());
    assert_eq!(f.refusal().await.unwrap()["wait"]["base"], OLD);

    *f.remote.lock().unwrap() = NEW;
    let offered = f.offer().await;
    assert_eq!(
        offered.offered.lifecycle_binding.task_id,
        f.case.env.lineage.task_id
    );
    assert_eq!(f.granted_base(&offered).await, None);
    assert_eq!(f.operations("artifact.export")[0]["base"], oid(NEW));
    for sha in [MERGE, SECOND_MERGE] {
        assert!(
            f.operations("contains")
                .contains(&json!({"ancestor":oid(sha), "descendant":oid(NEW)}))
        );
    }
    assert!(f.refusal().await.is_none());
}

#[tokio::test]
async fn unenrolled_stale_immutable_dependent_waits_without_refreshing_its_base() {
    let f = Fixture::new(false, true).await;
    f.insert_merges(true).await;
    let sibling = f.offer().await;
    assert_eq!(
        sibling.offered.lifecycle_binding.task_id,
        f.case.second_task
    );
    assert_eq!(f.granted_base(&sibling).await, None);
    assert!(f.operations("repo.retain_ref").is_empty());
    let checks = f.operations("contains");
    assert_eq!(
        checks.len(),
        1,
        "only the dependent needs an ancestry check"
    );
    assert_eq!(checks[0]["descendant"], oid(OLD));
    assert_eq!(
        f.operations("artifact.export").len(),
        1,
        "only the sibling exported"
    );
    let reason = f.refusal().await.unwrap();
    assert_eq!(reason["code"], "delivery_ancestry_wait");
    assert_eq!(reason["wait"]["reason"], "merge_missing_from_base");
    assert_eq!(reason["wait"]["base"], OLD);
}
