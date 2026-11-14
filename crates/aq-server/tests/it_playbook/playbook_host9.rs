//! Host9: AQ host catalog 2.2.0 (spec 0013 §6; plan tasks B2 to B5).
use aq_playbook_host::{
    DomainHost, HostMode, MemoryStorage, Owner, Request,
    model::{CommandContract, Registry, digest},
    runner::{Authority, DispatchResult, Host, Intent},
};
use aq_server::playbooks::{
    coding::{CodingPolicy, ROOT},
    domain::GrantScope,
    review_domain::{
        CatalogProfile, HOST7_CAPABILITIES, HOST8_FINGERPRINT, HOST9_CAPABILITIES, HOST9_COMMANDS,
        HOST9_FINGERPRINT, HOST9_VERSION,
    },
};
use serde_json::Value;
use std::{collections::BTreeSet, time::Duration};
use uuid::Uuid;

const DEADLINE: Duration = Duration::from_secs(5);
const ADDED: [&str; 4] = [
    "work.read",
    "work.route",
    "work.unroutable",
    "judgment.assess",
];

#[test]
fn host9_registry_matches_the_pinned_manifest() {
    let registry = CatalogProfile::Host9.registry();
    let manifest: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-host-catalog-2.2.0/manifest.json"
    ))
    .unwrap();
    assert_eq!(manifest["version"], HOST9_VERSION);
    assert_eq!(manifest["fingerprint"], HOST9_FINGERPRINT);
    assert_eq!(manifest["supersedes"]["fingerprint"], HOST8_FINGERPRINT);
    assert_eq!(
        digest(&(HOST9_VERSION, &registry)).unwrap(),
        HOST9_FINGERPRINT
    );
    assert_eq!(CatalogProfile::Host9.number(), 9);
    assert!(CatalogProfile::Host9.is_host2());
    let commands: BTreeSet<&str> = registry.commands.keys().map(String::as_str).collect();
    assert_eq!(commands, BTreeSet::from(HOST9_COMMANDS));
    for name in manifest["commands_added"].as_array().unwrap() {
        assert!(ADDED.contains(&name.as_str().unwrap()));
    }
    for event in ["aq.judgment.assessed", "aq.work.route_needed"] {
        assert!(registry.events.contains_key(event), "{event}");
    }
    // 2.2.0 only adds: every 2.1.0 contract is unchanged.
    let host8 = CatalogProfile::Host8.registry();
    for (name, contract) in &host8.commands {
        assert_eq!(registry.commands.get(name), Some(contract), "{name}");
    }
    for (name, contract) in &host8.events {
        assert_eq!(registry.events.get(name), Some(contract), "{name}");
    }
    // The pin lists the vendored files byte for byte.
    let pin: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-host-catalog-2.2.0-source.json"
    ))
    .unwrap();
    assert_eq!(pin["catalog_fingerprint"], HOST9_FINGERPRINT);
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/aq-host-catalog-2.2.0");
    let files = pin["files"].as_array().unwrap();
    assert_eq!(files.len(), std::fs::read_dir(&root).unwrap().count());
    for file in files {
        let bytes = std::fs::read(root.join(file["path"].as_str().unwrap())).unwrap();
        assert_eq!(file["bytes"], bytes.len());
        assert_eq!(
            file["sha256"].as_str().unwrap(),
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
        );
    }
}

struct Idle;
impl Host for Idle {
    fn authorize(&self, _: &Authority, _: &CommandContract) -> bool {
        true
    }
    fn dispatch(&mut self, _: &Intent) -> DispatchResult {
        DispatchResult::Unknown {
            reason: "no adapter in this test".into(),
        }
    }
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult {
        self.dispatch(intent)
    }
}
impl DomainHost for Idle {
    fn set_mode(&mut self, _: HostMode) {}
}

#[tokio::test]
async fn a_host8_coding_artifact_installs_in_a_host9_namespace() {
    let registry: Registry = CatalogProfile::Host9.registry();
    let owner = Owner::start_registered(
        "host9-coding".into(),
        aq_playbook_host::Limits::default(),
        registry,
        || Ok(MemoryStorage::default()),
        |_| Ok(Idle),
    )
    .await
    .unwrap();
    // Sealed against the Host8 registry (catalog 2.1.0).
    let artifacts = CodingPolicy::project_default(vec![]).build().unwrap();
    for artifact in artifacts.install_order() {
        let artifact = Box::new(artifact.clone());
        (owner.call(Request::Install { artifact }, DEADLINE).await).unwrap();
    }
    let authority = Authority {
        actor: "owner".into(),
        scope: "project".into(),
        capabilities: HOST7_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
    };
    let activate = Request::Activate {
        binding: ROOT.into(),
        artifact: artifacts.root.digest.clone(),
        authority,
    };
    owner.call(activate, DEADLINE).await.unwrap();
    owner.call(Request::Stop, DEADLINE).await.unwrap();
}

#[test]
fn host9_scope_admits_the_new_capabilities_and_nothing_else() {
    let scope = |capabilities: &[&str]| GrantScope {
        workspace_id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        recipients: vec![],
        capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
    };
    assert_eq!(HOST9_CAPABILITIES.len(), 16);
    assert_eq!(HOST9_CAPABILITIES[..12], HOST7_CAPABILITIES);
    assert_eq!(HOST9_CAPABILITIES[12..], ADDED);
    let host9 = CatalogProfile::Host9;
    assert!(host9.validate_scope(&scope(&HOST9_CAPABILITIES)).is_ok());
    for one in ADDED {
        assert!(host9.validate_scope(&scope(&[one])).is_ok(), "{one}");
        // Host8 keeps its twelve.
        assert!(
            CatalogProfile::Host8
                .validate_scope(&scope(&[one]))
                .is_err()
        );
    }
    for other in ["work.delete", "judgment.request", ""] {
        assert!(host9.validate_scope(&scope(&[other])).is_err(), "{other}");
    }
    assert!(host9.validate_scope(&scope(&[])).is_err());
    // Every catalog capability is one the profile admits, and no more.
    let declared: BTreeSet<String> = (host9.registry().commands.values())
        .flat_map(|c| c.capabilities.iter().cloned())
        .collect();
    let admitted: BTreeSet<String> = HOST9_CAPABILITIES.iter().map(|c| c.to_string()).collect();
    assert_eq!(declared, admitted);
}

#[test]
fn a_namespace_of_another_profile_is_refused_with_what_to_do() {
    let host9 = CatalogProfile::Host9;
    assert_eq!(host9.namespace_refusal(1, None), None);
    assert_eq!(host9.namespace_refusal(1, Some((9, 1))), None);
    for (number, name) in [(2, "human_review_v2"), (6, "monitoring_v6"), (8, "host_v8")] {
        assert_eq!(
            CatalogProfile::from_number(number).unwrap().config_name(),
            name
        );
        let why = host9.namespace_refusal(1, Some((number, 1))).unwrap();
        assert!(
            why.contains(&format!("exists with profile \"{name}\"")),
            "{why}"
        );
        assert!(why.contains("profile=\"host_v9\"") && why.contains("new namespace_id"));
    }
    // A generation change alone is refused the same way.
    assert!(host9.namespace_refusal(2, Some((9, 1))).is_some());
    assert_eq!(CatalogProfile::from_number(10), None);
}

// ------------------------------------------------------------------------
// B3 to B5: the Host9 adapters on disposable PostgreSQL through the
// production bridge service. The graph, the routing port and the judgment
// provider are in-memory fakes of the injected ports.
mod adapters {
    use crate::common;
    use aq_playbook_host::runner::{Intent, IntentStatus};
    use aq_routing::{
        CompactRoute, ProviderIntent, Route, Source, Written,
        classify::{Offer, Options},
    };
    use aq_server::{
        playbooks::{
            domain::*,
            host_adapters::{HostResult, JudgmentProvider, RoutingPort, WorkGraph},
            review_domain::{CatalogProfile, HOST9_CAPABILITIES},
            service::*,
        },
        protected_storage::{KeyFile, Keyring, ProtectionConfig},
    };
    use serde_json::{Value, json};
    use std::{
        collections::{BTreeMap, BTreeSet},
        os::unix::fs::PermissionsExt,
        sync::{Arc, Mutex},
    };
    use uuid::Uuid;

    #[derive(Default)]
    pub(super) struct FakeGraph {
        pub nodes: Mutex<BTreeMap<Uuid, Value>>,
    }
    #[async_trait::async_trait]
    impl WorkGraph for FakeGraph {
        async fn node(&self, _: Uuid, id: Uuid) -> Result<Option<Value>> {
            Ok(self.nodes.lock().unwrap().get(&id).cloned())
        }
        async fn mutate(&self, _: Uuid, _: Value) -> Result<(u16, Value)> {
            Err(Error::Invalid)
        }
    }
    impl FakeGraph {
        pub fn task(&self, title: &str, metadata: Value) -> Uuid {
            let id = Uuid::new_v4();
            let node = json!({"id": id, "kind": "task", "version": 3, "title": title,
                "body": format!("{title} body"), "metadata": metadata});
            self.nodes.lock().unwrap().insert(id, node);
            id
        }
        pub fn node_of(&self, id: Uuid) -> Value {
            self.nodes.lock().unwrap()[&id].clone()
        }
        pub fn stored(&self, id: Uuid) -> Option<CompactRoute> {
            CompactRoute::from_metadata(&self.node_of(id)["metadata"]).unwrap()
        }
    }

    /// The server side of routing: where tasks live, which projects are on
    /// the router, attempt counts, Settings offers, and the route writes
    /// (applied to the fake graph with the version check and owner rule of
    /// `routing::write`).
    pub(super) struct FakeRouting {
        pub graph: Arc<FakeGraph>,
        pub located: Mutex<BTreeMap<Uuid, (Uuid, Uuid)>>,
        pub on_router: Mutex<BTreeSet<Uuid>>,
        pub attempts: Mutex<(u32, u32)>,
        pub options: Mutex<Options>,
        pub writes: Mutex<Vec<(Uuid, Option<Route>)>>,
    }
    #[async_trait::async_trait]
    impl RoutingPort for FakeRouting {
        async fn located(&self, task: Uuid) -> Result<Option<(Uuid, Uuid)>> {
            Ok(self.located.lock().unwrap().get(&task).copied())
        }
        async fn on_router(&self, project: Uuid) -> Result<bool> {
            Ok(self.on_router.lock().unwrap().contains(&project))
        }
        async fn attempts(&self, _: Uuid) -> Result<(u32, u32)> {
            Ok(*self.attempts.lock().unwrap())
        }
        async fn options(&self) -> Result<Options> {
            Ok(self.options.lock().unwrap().clone())
        }
        async fn write(&self, at: (Uuid, Uuid, u64), route: Option<&Route>) -> Result<Written> {
            let (_, task, version) = at;
            let mut nodes = self.graph.nodes.lock().unwrap();
            let node = nodes.get_mut(&task).ok_or(Error::Unknown)?;
            let stored = CompactRoute::from_metadata(&node["metadata"]).unwrap_or(None);
            if node["version"] != json!(version)
                || !aq_routing::may_replace(stored.as_ref(), Source::Router)
            {
                return Ok(Written::Stale);
            }
            let mut writes = self.writes.lock().unwrap();
            let sequence = writes.len() as u64 + 1;
            let compact = route.map(|r| r.compact(sequence).unwrap());
            node["metadata"] = aq_routing::with_route(&node["metadata"], compact.as_ref()).unwrap();
            node["version"] = json!(version + 1);
            writes.push((task, route.cloned()));
            Ok(Written::Routed { sequence })
        }
    }

    pub(super) struct Host {
        pub f: common::profiled::Fixture,
        pub service: Service,
        pub runtime: RuntimeOwner,
        pub grant: Uuid,
        pub scope: GrantScope,
        pub now: i64,
        pub graph: Arc<FakeGraph>,
        pub routing: Arc<FakeRouting>,
        pub other_project: Uuid,
    }
    fn offer(provider: &str, harness: &str) -> Offer {
        Offer {
            provider: provider.into(),
            harness: harness.into(),
        }
    }
    pub(super) async fn host(
        installation_routing: bool,
        judgments: Option<Arc<dyn JudgmentProvider>>,
    ) -> Host {
        let f = common::profiled::fixture().await;
        common::profiled::insert(&f).await;
        host_on(f, installation_routing, judgments).await
    }
    pub(super) async fn host_on(
        f: common::profiled::Fixture,
        installation_routing: bool,
        judgments: Option<Arc<dyn JudgmentProvider>>,
    ) -> Host {
        let l = f.selection.lineage.clone();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key");
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
        let graph = Arc::new(FakeGraph::default());
        let other_project = Uuid::new_v4();
        let routing = Arc::new(FakeRouting {
            graph: graph.clone(),
            located: Mutex::default(),
            on_router: Mutex::new(BTreeSet::from([l.project_id, other_project])),
            attempts: Mutex::new((0, 0)),
            options: Mutex::new(Options {
                classes: BTreeMap::from([
                    (
                        "standard-high".parse().unwrap(),
                        vec![offer("anthropic", "claude"), offer("openai", "codex")],
                    ),
                    (
                        "complex-high".parse().unwrap(),
                        vec![offer("anthropic", "claude")],
                    ),
                ]),
            }),
            writes: Mutex::default(),
        });
        let mut service = Service::new_host2(
            &f.state,
            Namespace {
                namespace_id: Uuid::new_v4(),
                generation: 1,
                name: format!("host9-{}", Uuid::new_v4()),
                workspace_id: l.workspace_id,
                project_id: l.project_id,
                owner_user_id: f.actor,
            },
            Arc::new(keys),
            CatalogProfile::Host9,
            graph.clone(),
            None,
            judgments,
            None,
        )
        .await
        .unwrap()
        .with_routing(routing.clone());
        service.installation_routing = installation_routing;
        service.initialize(f.actor).await.unwrap();
        let now = (f.state.clock)().timestamp_millis();
        let scope = GrantScope {
            workspace_id: l.workspace_id,
            project_id: l.project_id,
            recipients: vec![],
            capabilities: BTreeSet::from(HOST9_CAPABILITIES.map(String::from)),
        };
        let grant = Uuid::new_v4();
        service
            .grant(f.actor, grant, scope.clone(), now + 10_000_000)
            .await
            .unwrap();
        let runtime = service.adopt_runtime(Uuid::new_v4(), 0).await.unwrap();
        Host {
            f,
            service,
            runtime,
            grant,
            scope,
            now,
            graph,
            routing,
            other_project,
        }
    }
    impl Host {
        pub fn intent(&self, id: &str, name: &str, input: Value) -> Intent {
            Intent {
                id: id.into(),
                run: "synthetic-run".into(),
                step: name.replace('.', "-"),
                invocation: 1,
                deadline_at_ms: self.now + 5000,
                command: CatalogProfile::Host9.registry().commands[name].clone(),
                input_digest: digest(&input).unwrap(),
                input,
                authority: CatalogProfile::Host9
                    .authority(self.grant, 1, &self.scope)
                    .unwrap(),
                status: IntentStatus::Dispatched,
                cancel_requested: false,
                cancel_dispatched: false,
                handle: None,
                result: None,
                reason: None,
                late_results: vec![],
                completion_event: None,
                dispatch_sequence: None,
            }
        }
        pub async fn run(&self, id: &str, name: &str, input: Value) -> HostResult {
            let intent = self.intent(id, name, input);
            let result = (self.service).host_dispatch(&self.runtime, self.grant, 1, &intent);
            let result = result.await.unwrap();
            // Every answer is one the catalog declares.
            (intent.command.outcomes[&result.outcome].validate(&result.output)).unwrap();
            result
        }
        async fn read(&self, task: Uuid, revision: i64) -> HostResult {
            let input = json!({"subject": {"id": task.to_string(), "revision": revision}});
            self.run(&format!("read-{}", Uuid::new_v4()), "work.read", input)
                .await
        }
        async fn route(&self, task: Uuid, revision: i64, change: Value) -> HostResult {
            let mut input = json!({"subject": {"id": task.to_string(), "revision": revision},
                "level": "standard-high", "harnesses": [], "providers": [], "prefer": [],
                "hold": false, "reasons": [{"code": "level_from_kind", "detail": "Bug fixes are Standard, high."}],
                "classification": null});
            for (key, value) in change.as_object().unwrap() {
                input[key] = value.clone();
            }
            self.run(&format!("route-{}", Uuid::new_v4()), "work.route", input)
                .await
        }
        async fn emission(&self, task: Uuid) -> Option<(i64, Option<String>, Option<Value>)> {
            sqlx::query_as(
                "SELECT task_version,outcome,reasons FROM route_needed_emissions WHERE task_id=$1",
            )
            .bind(task)
            .fetch_optional(&self.f.db.pool().await)
            .await
            .unwrap()
        }
    }

    #[tokio::test]
    async fn work_read_returns_the_facts_of_a_task_in_another_project_on_the_router() {
        let h = host(true, None).await;
        let task = h.graph.task(
            "Fix the parser",
            json!({"aq:task_type": "bugfix", "aq:intelligence_hint": "complex-high", "aq:priority": 9}),
        );
        // The server asked a route for it in another project (R1).
        (h.routing.located.lock().unwrap()).insert(task, (Uuid::new_v4(), h.other_project));
        *h.routing.attempts.lock().unwrap() = (1, 2);
        // The revision asked for does not matter: the task is read as it is.
        let read = h.read(task, 1).await;
        assert_eq!(read.outcome, "observed");
        assert_eq!(
            read.output["subject"],
            json!({"id": task.to_string(), "revision": 3})
        );
        assert_eq!(read.output["reference"], format!("{task}@3"));
        assert_eq!(
            read.output["facts"],
            json!({"kind": "bugfix", "title": "Fix the parser", "description": "Fix the parser body",
                "origin": "unknown", "level_hint": "complex-high", "charged_failures": 1,
                "ended_attempts": 2, "route": "none"})
        );
        // Without installation routing only the namespace's project is served.
        let own = host(false, None).await;
        let task = own.graph.task("Elsewhere", json!({}));
        (own.routing.located.lock().unwrap()).insert(task, (Uuid::new_v4(), own.other_project));
        let refused = own.read(task, 3).await;
        assert_eq!(
            (refused.outcome.as_str(), &refused.output["reason"]),
            ("refused", &json!("not_on_router"))
        );
        let mine = own.graph.task("Mine", json!({}));
        assert_eq!(own.read(mine, 3).await.outcome, "observed");
    }

    #[tokio::test]
    async fn work_read_refuses_a_task_whose_binding_is_pinned_to_a_profile() {
        let h = host(true, None).await;
        let task = h.graph.task("Pinned", json!({}));
        // The namespace's own project is not on the automatic router.
        h.routing.on_router.lock().unwrap().clear();
        let refused = h.read(task, 3).await;
        assert_eq!(refused.outcome, "refused");
        assert_eq!(refused.output["reason"], "not_on_router");
        let routed = h.route(task, 3, json!({})).await;
        assert_eq!(routed.output["reason"], "not_on_router");
        assert!(h.routing.writes.lock().unwrap().is_empty());
        // A task that does not exist, or an id that is no task id.
        (h.routing.on_router.lock().unwrap()).insert(h.scope.project_id);
        let missing = h.read(Uuid::new_v4(), 1).await;
        assert_eq!(missing.output["reason"], "unknown_task");
        let input = json!({"subject": {"id": "not-a-task", "revision": 1}});
        let odd = h.run("odd", "work.read", input).await;
        assert_eq!(
            (odd.outcome.as_str(), &odd.output["reason"]),
            ("refused", &json!("unknown_task"))
        );
    }

    #[tokio::test]
    async fn a_route_for_an_older_task_revision_is_stale_and_writes_nothing() {
        let h = host(true, None).await;
        let task = h
            .graph
            .task("Changed since the read", json!({"aq:priority": 4}));
        let stale = h.route(task, 2, json!({})).await;
        assert_eq!(stale.outcome, "stale");
        assert_eq!(
            stale.output["subject"],
            json!({"id": task.to_string(), "revision": 3})
        );
        assert!(h.routing.writes.lock().unwrap().is_empty());
        assert_eq!(h.graph.stored(task), None);
        assert_eq!(h.emission(task).await, None);
        // At the version read, it is written.
        let routed = h.route(task, 3, json!({})).await;
        assert_eq!(routed.outcome, "routed");
        assert_eq!(h.graph.node_of(task)["metadata"]["aq:priority"], 4);
        assert_eq!(h.routing.writes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_route_naming_an_unoffered_level_or_harness_is_refused() {
        let h = host(true, None).await;
        let task = h.graph.task("Nothing can take it", json!({}));
        for change in [
            json!({"level": "deep-high"}),
            json!({"harnesses": ["cursor"]}),
            json!({"providers": ["google"]}),
            json!({"level": "complex-high", "harnesses": ["codex"]}),
        ] {
            let refused = h.route(task, 3, change.clone()).await;
            assert_eq!(refused.outcome, "refused", "{change}");
            assert_eq!(refused.output["reason"], "not_offered", "{change}");
        }
        assert!(h.routing.writes.lock().unwrap().is_empty());
        // Recorded, so the server does not ask again until something changes.
        let (version, outcome, reasons) = h.emission(task).await.unwrap();
        assert_eq!((version, outcome.as_deref()), (3, Some("not_offered")));
        assert_eq!(reasons.unwrap()[0]["code"], "level_from_kind");
        // The host keeps what the level offers and reports it.
        let kept = h
            .route(
                task,
                3,
                json!({"harnesses": ["cursor", "codex"], "prefer": ["codex", "claude"]}),
            )
            .await;
        assert_eq!(kept.outcome, "routed");
        assert_eq!(kept.output["harnesses"], json!(["codex"]));
        assert_eq!(kept.output["prefer"], json!(["codex"]));
        assert_eq!(h.graph.stored(task).unwrap().h, ["codex"]);
    }

    #[tokio::test]
    async fn an_owner_route_is_never_replaced() {
        let h = host(true, None).await;
        let owner = json!({"aq:route": {"c": "complex-high", "l": null, "i": "any", "d": null, "s": "owner", "n": 1}});
        let task = h.graph.task("The owner chose", owner.clone());
        assert_eq!(h.read(task, 3).await.output["facts"]["route"], "owner");
        let refused = h.route(task, 3, json!({})).await;
        assert_eq!(
            (refused.outcome.as_str(), &refused.output["reason"]),
            ("refused", &json!("owner_route"))
        );
        let input = json!({"subject": {"id": task.to_string(), "revision": 3},
            "reasons": [{"code": "policy", "detail": "Nobody may take it."}]});
        let unroutable = h.run("unroutable", "work.unroutable", input).await;
        assert_eq!(unroutable.output["reason"], "owner_route");
        assert_eq!(h.graph.node_of(task)["metadata"], owner);
        assert!(h.routing.writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_route_records_the_artifact_digest_and_hold_pins_the_provider() {
        let h = host(true, None).await;
        let digest = format!("sha256:{}", "ab".repeat(32));
        sqlx::query("INSERT INTO playbook_activations(namespace_id,op_id,kind,binding,artifact,created_at_ms) VALUES($1,'a','activate','aq.routing',$2,1)")
            .bind(h.service.namespace.namespace_id)
            .bind(&digest)
            .execute(&h.f.db.pool().await)
            .await
            .unwrap();
        *h.routing.attempts.lock().unwrap() = (1, 2);
        let task = h.graph.task("Held for one agent", json!({}));
        let classification = json!({"reference": "judgment:7", "fields": ["risk"], "labels": ["high"],
            "flags": ["hard"], "rationale": "Touches billing."});
        let change = json!({"harnesses": ["claude"], "hold": true, "classification": classification,
            "reasons": [{"code": "raised_for_risk", "detail": "High risk is at least Standard, high."}, {"code": "held_for_harness"}]});
        let routed = h.route(task, 3, change).await;
        assert_eq!(routed.outcome, "routed");
        let stored = h.graph.stored(task).unwrap();
        assert_eq!(stored.d.as_deref(), Some(&digest[..16]));
        assert_eq!(
            (stored.s, stored.i, stored.a),
            (Source::Router, ProviderIntent::Pinned, 2)
        );
        let (_, route) = h.routing.writes.lock().unwrap()[0].clone();
        let route = route.unwrap();
        assert_eq!(route.policy_digest.as_deref(), Some(digest.as_str()));
        assert_eq!(route.judgment.as_ref().unwrap(), &classification);
        assert_eq!(route.reasons.len(), 2);
        assert_eq!(
            (
                route.reasons[1].code.as_str(),
                route.reasons[1].detail.as_str()
            ),
            ("held_for_harness", "")
        );
        assert_eq!(h.read(task, 4).await.output["facts"]["route"], "router");
    }

    #[tokio::test]
    async fn work_unroutable_leaves_the_task_unrouted_with_its_reasons() {
        let h = host(true, None).await;
        let task = h.graph.task("The document allows nobody", json!({}));
        assert_eq!(h.route(task, 3, json!({})).await.outcome, "routed");
        let unroutable = |revision: i64, reasons: Value| json!({"subject": {"id": task.to_string(), "revision": revision}, "reasons": reasons});
        let none = h
            .run("none", "work.unroutable", unroutable(4, json!([])))
            .await;
        assert_eq!(
            (none.outcome.as_str(), &none.output["reason"]),
            ("refused", &json!("no_reasons"))
        );
        let reasons = json!([{"code": "policy", "detail": "Never route payroll work."}]);
        let stale = h
            .run("stale", "work.unroutable", unroutable(3, reasons.clone()))
            .await;
        assert_eq!(stale.outcome, "stale");
        assert!(h.graph.stored(task).is_some());
        let recorded = h
            .run(
                "recorded",
                "work.unroutable",
                unroutable(4, reasons.clone()),
            )
            .await;
        assert_eq!(recorded.outcome, "recorded");
        // The router's route is gone, and the row is for the task as left.
        assert_eq!(h.graph.stored(task), None);
        assert_eq!(recorded.output["subject"]["revision"], 5);
        assert_eq!(
            h.emission(task).await.unwrap(),
            (5, Some("unroutable".into()), Some(reasons.clone()))
        );
        // The same intent again: the record, no second write.
        let again = h
            .run("recorded", "work.unroutable", unroutable(4, reasons))
            .await;
        assert_eq!(again, recorded);
        assert_eq!(h.routing.writes.lock().unwrap().len(), 2);
    }
    // -------------------------------------------------------- judgment.assess

    use aq_server::playbooks::host_adapters::{
        AssessReply, AssessRequest, JudgmentReply, JudgmentRequest, ProviderFailure,
    };

    /// Provider fake: one scripted answer; `held` makes it wait for `release`.
    #[derive(Default)]
    struct FakeAssessor {
        answer: Mutex<Option<std::result::Result<Value, &'static str>>>,
        requests: Mutex<Vec<AssessRequest>>,
        held: std::sync::atomic::AtomicBool,
        release: tokio::sync::Notify,
    }
    impl FakeAssessor {
        fn answering(answer: std::result::Result<Value, &'static str>) -> Arc<Self> {
            let fake = Self::default();
            *fake.answer.lock().unwrap() = Some(answer);
            Arc::new(fake)
        }
    }
    #[async_trait::async_trait]
    impl JudgmentProvider for FakeAssessor {
        fn credential_profile(&self, _: Uuid, _: Uuid) -> Option<String> {
            Some("profile/judgment-default".into())
        }
        async fn complete(
            &self,
            _: &JudgmentRequest,
        ) -> std::result::Result<JudgmentReply, ProviderFailure> {
            Err(ProviderFailure::Unavailable)
        }
        async fn assess(
            &self,
            request: &AssessRequest,
        ) -> std::result::Result<AssessReply, &'static str> {
            self.requests.lock().unwrap().push(request.clone());
            if self.held.load(std::sync::atomic::Ordering::SeqCst) {
                self.release.notified().await;
            }
            let answer = self.answer.lock().unwrap().clone().unwrap()?;
            Ok(AssessReply {
                answer,
                provider: "fixture".into(),
                model: "model-a".into(),
                input_tokens: 40,
                output_tokens: 9,
            })
        }
    }

    fn assess_input(task: Uuid) -> Value {
        json!({"subject": {"id": task.to_string(), "revision": 3},
            "question": "Classify this task for routing.",
            "context": {"items": [{"label": "Title", "text": "Fix billing rounding"}]},
            "fields": [
                {"id": "risk", "description": "Risk of the change.", "choices": ["low", "medium", "high", "unknown"]},
                {"id": "kind", "description": "What the task is.", "choices": ["bugfix", "feature"]}],
            "flags": [{"id": "narrow", "description": "Touches one module."},
                {"id": "hard", "description": "Unusually hard reasoning."}],
            "rationale_max_bytes": 200,
            "budget": {"deadline_ms": 120000, "max_output_tokens": 2048}})
    }
    impl Host {
        /// (status, output) of the operation behind a handle.
        async fn operation(&self, handle: &str) -> (String, Option<Value>) {
            sqlx::query_as(
                "SELECT status,output FROM playbook_host_operations WHERE operation_id=$1",
            )
            .bind(Uuid::parse_str(handle).unwrap())
            .fetch_one(&self.f.db.pool().await)
            .await
            .unwrap()
        }
        async fn completed(&self, handle: &str) -> (String, Value) {
            for _ in 0..400 {
                let (status, output) = self.operation(handle).await;
                if status != "running" {
                    return (status, output.unwrap());
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("judgment {handle} did not complete");
        }
        /// The sealed `aq.judgment.assessed` events queued for the engine.
        async fn events(&self) -> i64 {
            sqlx::query_scalar("SELECT count(*) FROM playbook_outbox WHERE namespace_id=$1")
                .bind(self.service.namespace.namespace_id)
                .fetch_one(&self.f.db.pool().await)
                .await
                .unwrap()
        }
        async fn assess(&self, id: &str, input: Value) -> String {
            let accepted = self.run(id, "judgment.assess", input).await;
            assert_eq!(accepted.outcome, "accepted", "{accepted:?}");
            accepted.output["handle"].as_str().unwrap().to_owned()
        }
    }
    /// What the catalog says the completion event carries for an outcome.
    fn completion_valid(outcome: &str, output: &Value) {
        let registry = CatalogProfile::Host9.registry();
        let contract = registry.commands["judgment.assess"]
            .asynchronous
            .clone()
            .unwrap();
        (contract
            .completion
            .validate(&json!({"outcome": outcome, "output": output})))
        .unwrap();
        registry.commands["judgment.assess"].outcomes[outcome]
            .validate(output)
            .unwrap();
    }

    #[tokio::test]
    async fn an_assessment_answers_by_name_and_reaches_the_playbook_as_labels_and_flags() {
        let answer = json!({"labels": {"kind": "bugfix", "risk": "high"},
            "flags": {"hard": true, "narrow": false}, "rationale": "Billing code, money at stake."});
        let judge = FakeAssessor::answering(Ok(answer));
        let h = host(true, Some(judge.clone())).await;
        let task = h.graph.task("Fix billing rounding", json!({}));
        let handle = h.assess("assess-1", assess_input(task)).await;
        let (outcome, output) = h.completed(&handle).await;
        assert_eq!(outcome, "assessed", "{output}");
        completion_valid(&outcome, &output);
        // One label per field, in field order; the flags that hold.
        assert_eq!(output["labels"], json!(["high", "bugfix"]));
        assert_eq!(output["flags"], json!(["hard"]));
        assert_eq!(output["rationale"], "Billing code, money at stake.");
        assert_eq!(output["reference"], format!("judgment:{handle}"));
        assert_eq!(
            output["subject"],
            json!({"id": task.to_string(), "revision": 3})
        );
        let p = &output["provenance"];
        assert_eq!(
            (&p["provider"], &p["model"]),
            (&json!("fixture"), &json!("model-a"))
        );
        assert_eq!(
            p["usage"],
            json!({"input_tokens": 40, "output_tokens": 9, "cost_microunits": 0})
        );
        // The model was asked by name, with the choices as the schema.
        let request = judge.requests.lock().unwrap()[0].clone();
        assert_eq!(request.purpose, aq_internal_work::Purpose::PlaybookCall);
        assert_eq!(
            request.schema["properties"]["labels"]["properties"]["risk"]["enum"],
            json!(["low", "medium", "high", "unknown"])
        );
        assert!(
            request
                .instructions
                .contains("Classify this task for routing.")
        );
        assert!(request.input.contains("Fix billing rounding"));
        assert_eq!(
            (request.deadline_ms, request.max_output_bytes),
            (120_000, 8192)
        );
        // Exactly one final answer: one event, and neither a second dispatch
        // of the intent nor the deadline sweep adds another.
        assert_eq!(h.events().await, 1);
        let again = h
            .run("assess-1", "judgment.assess", assess_input(task))
            .await;
        assert_eq!(again.output["handle"], handle.as_str());
        h.f.time
            .fetch_add(600_000, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            h.service.expire_assessments(&h.runtime, 2).await.unwrap(),
            0
        );
        assert_eq!(
            (h.events().await, judge.requests.lock().unwrap().len()),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn an_answer_outside_the_choices_fails_as_invalid_output() {
        for (answer, reason) in [
            (
                Ok(json!({"labels": {"kind": "bugfix", "risk": "severe"},
                "flags": {"hard": true, "narrow": false}, "rationale": "r"})),
                "indeterminate",
            ),
            (
                Ok(
                    json!({"labels": {"risk": "low"}, "flags": {"hard": true, "narrow": false}, "rationale": "r"}),
                ),
                "indeterminate",
            ),
            (Err("provider_error"), "provider_error"),
            (Err("provider_unavailable"), "provider_unavailable"),
        ] {
            let h = host(true, Some(FakeAssessor::answering(answer))).await;
            let task = h.graph.task("Unclear", json!({}));
            let handle = h.assess("assess", assess_input(task)).await;
            let (outcome, output) = h.completed(&handle).await;
            assert_eq!(
                (outcome.as_str(), &output["reason"]),
                ("failed", &json!(reason)),
                "{output}"
            );
            completion_valid(&outcome, &output);
            assert_eq!(h.events().await, 1);
        }
        // No provider at all: accepted, then failed, never left unanswered.
        let h = host(true, None).await;
        let task = h.graph.task("Nobody to ask", json!({}));
        let handle = h.assess("assess", assess_input(task)).await;
        let (outcome, output) = h.completed(&handle).await;
        assert_eq!(
            (outcome.as_str(), &output["reason"]),
            ("failed", &json!("provider_unavailable"))
        );
    }

    #[tokio::test]
    async fn a_request_over_sixteen_fields_is_refused() {
        let judge = FakeAssessor::answering(Err("provider_error"));
        let h = host(true, Some(judge.clone())).await;
        let task = h.graph.task("Too many questions", json!({}));
        // Seventeen fields, or more than 65536 bytes, are outside the
        // contract: the bridge refuses the intent itself, as the engine does
        // before it dispatches one.
        let mut many = assess_input(task);
        many["fields"] = (0..17)
            .map(|i| json!({"id": format!("f{i}"), "description": "d", "choices": ["a"]}))
            .collect();
        let mut large = assess_input(task);
        large["context"]["items"] = (0..9)
            .map(|n| json!({"label": format!("part {n}"), "text": "x".repeat(8000)}))
            .collect();
        for (id, input) in [("many", many), ("large", large)] {
            let intent = h.intent(id, "judgment.assess", input);
            let refused = (h.service).host_dispatch(&h.runtime, h.grant, 1, &intent);
            assert_eq!(refused.await, Err(Error::Invalid), "{id}");
        }
        // Requests the contract admits and the host does not run.
        type Change = fn(&mut Value);
        let changes: [(&str, Change); 4] = [
            ("duplicate", |i| i["fields"][1]["id"] = json!("risk")),
            ("no-choices", |i| i["fields"][0]["choices"] = json!([])),
            ("no-fields", |i| i["fields"] = json!([])),
            ("duplicate-flag", |i| i["flags"][1]["id"] = json!("narrow")),
        ];
        for (id, change) in changes {
            let mut input = assess_input(task);
            change(&mut input);
            let refused = h.run(id, "judgment.assess", input).await;
            assert_eq!(refused.outcome, "refused", "{id}");
            assert_eq!(refused.output["reason"], "invalid_request", "{id}");
        }
        // Nothing was asked, queued or left running.
        assert!(judge.requests.lock().unwrap().is_empty());
        assert_eq!(h.events().await, 0);
        let running: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_host_operations")
            .fetch_one(&h.f.db.pool().await)
            .await
            .unwrap();
        assert_eq!(running, 0);
    }

    #[tokio::test]
    async fn an_accepted_judgment_without_an_answer_fails_at_its_deadline_exactly_once() {
        let answer = json!({"labels": {"kind": "bugfix", "risk": "low"},
            "flags": {"hard": false, "narrow": true}, "rationale": "late"});
        let judge = FakeAssessor::answering(Ok(answer));
        judge.held.store(true, std::sync::atomic::Ordering::SeqCst);
        let h = host(true, Some(judge.clone())).await;
        let task = h.graph.task("The provider hangs", json!({}));
        let handle = h.assess("assess", assess_input(task)).await;
        // Before the deadline (and its grace) the judgment stays open.
        h.f.time
            .fetch_add(120_000, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            h.service.expire_assessments(&h.runtime, 2).await.unwrap(),
            0
        );
        assert_eq!(h.operation(&handle).await.0, "running");
        h.f.time
            .fetch_add(5_000, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            h.service.expire_assessments(&h.runtime, 2).await.unwrap(),
            1
        );
        let (outcome, output) = h.completed(&handle).await;
        assert_eq!(
            (outcome.as_str(), &output["reason"]),
            ("failed", &json!("deadline_exceeded"))
        );
        completion_valid(&outcome, &output);
        assert_eq!(
            output["subject"],
            json!({"id": task.to_string(), "revision": 3})
        );
        // The provider's late answer changes nothing: still one final answer.
        judge.release.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(h.operation(&handle).await.0, "failed");
        assert_eq!(h.events().await, 1);
        assert_eq!(
            h.service.expire_assessments(&h.runtime, 2).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_routing_playbook_judgment_runs_with_the_routing_classification_purpose() {
        use crate::common::fake_api;
        use aq_server::playbooks::internal_judgments::InternalJudgments;
        // The production provider (internal work) against a local endpoint.
        let answer = r#"{"labels": {"risk": "medium", "kind": "feature"}, "flags": {"narrow": true, "hard": false}, "rationale": "A contained feature."}"#;
        let (endpoint, seen) = fake_api::serve(200, fake_api::completed(answer)).await;
        let f = common::profiled::fixture().await;
        common::profiled::insert(&f).await;
        let (work, _dir) =
            crate::playbook_host_requests::internal_work(&f, endpoint, Some("engineering")).await;
        let (_stop, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(work.clone().serve(rx));
        let judgments = InternalJudgments::new(work, aq_internal_work::Purpose::PlaybookCall);
        let h = host_on(f, true, Some(Arc::new(judgments))).await;
        let pool = h.f.db.pool().await;
        // The grant the server holds for the routing source (R2).
        sqlx::query("INSERT INTO playbook_sources(id,kind,binding,scope,purpose,judgment_purpose,commands,events,capabilities,facts,revision,updated_at_ms) VALUES('routing','builtin','aq.routing','installation','Route every task','routing_classification','[]','[]','[]','[]',1,1)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO playbook_source_grants(source_id,grant_id,revision,capabilities,created_at_ms) VALUES('routing',$1,1,'[]',1)")
            .bind(h.grant)
            .execute(&pool)
            .await
            .unwrap();
        let task = h.graph.task("Add an export button", json!({}));
        let handle = h.assess("assess", assess_input(task)).await;
        let (outcome, output) = h.completed(&handle).await;
        assert_eq!(outcome, "assessed", "{output}");
        completion_valid(&outcome, &output);
        assert_eq!(output["labels"], json!(["medium", "feature"]));
        assert_eq!(output["flags"], json!(["narrow"]));
        let p = &output["provenance"];
        assert_eq!(
            (&p["provider"], &p["model"]),
            (&json!("fixture-a"), &json!("model-a"))
        );
        assert_eq!(p["credential_profile"], "internal-work");
        assert_eq!(seen.lock().unwrap().bodies.len(), 1, "one provider request");
        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT purpose,affected_kind,affected_id FROM internal_tasks")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            [(
                "routing_classification".to_string(),
                "playbook_operation".to_string(),
                handle
            )]
        );
    }
}
