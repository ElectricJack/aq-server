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
    for (number, name) in [
        (2, "human_review_v2"),
        (6, "monitoring_v6"),
        (8, "host_v8"),
        (11, "host_v11"),
    ] {
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
    assert_eq!(CatalogProfile::from_number(12), None);
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
                described: BTreeMap::new(),
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
                deadline_at_ms: (self.f.state.clock)().timestamp_millis() + 5000,
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
        h.activated(&digest).await;
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
    // --------------------------------------------------- aq.work.route_needed

    impl Host {
        /// A routing artifact is active: what the owner last activated on `aq.routing`.
        async fn activated(&self, digest: &str) {
            sqlx::query("INSERT INTO playbook_engine_bindings(namespace_id,generation,binding,artifact,at_ms) VALUES($1,1,'aq.routing',$2,1) ON CONFLICT(namespace_id,generation,binding) DO UPDATE SET artifact=$2")
                .bind(self.service.namespace.namespace_id)
                .bind(digest)
                .execute(&self.f.db.pool().await)
                .await
                .unwrap();
        }
        fn clock(&self) -> i64 {
            (self.f.state.clock)().timestamp_millis()
        }
        /// What placement records for a ready task that waits for a route.
        async fn wanted(&self, task: Uuid, version: i64, project: Uuid) {
            sqlx::query("INSERT INTO route_needed_emissions(task_id,task_version,emitted_at_ms,wanted_version,wanted_at_ms,workspace_id,project_id,wanted_since_ms) VALUES($1,0,0,$2,$3,$4,$5,$3) ON CONFLICT(task_id) DO UPDATE SET wanted_since_ms=CASE WHEN route_needed_emissions.wanted_version=$2 THEN route_needed_emissions.wanted_since_ms ELSE $3 END,wanted_version=$2,wanted_at_ms=$3")
                .bind(task)
                .bind(version)
                .bind(self.clock())
                .bind(self.scope.workspace_id)
                .bind(project)
                .execute(&self.f.db.pool().await)
                .await
                .unwrap();
        }
        async fn asked(&self) -> usize {
            (self.service.collect_route_needed(&self.runtime, 8).await).unwrap()
        }
        /// Time passes; placement keeps seeing the task wait.
        async fn later(&self, ms: i64, task: Uuid, version: i64) {
            self.f
                .time
                .fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
            self.wanted(task, version, self.scope.project_id).await;
        }
    }

    #[tokio::test]
    async fn an_unrouted_ready_task_emits_one_event_and_not_again_within_two_minutes() {
        let h = host(true, None).await;
        h.activated("sha256:routing-1").await;
        let task = h.graph.task("Waiting for a route", json!({}));
        h.wanted(task, 3, h.scope.project_id).await;
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.events().await, 1);
        // The sealed event is the catalog's, for the task version.
        let page = h.service.outbox(&h.runtime, 0, 8).await.unwrap();
        let event = &page[0].2;
        assert_eq!(event.contract.name, "aq.work.route_needed");
        assert_eq!(
            event.payload,
            json!({"subject": {"id": task.to_string(), "revision": 3}})
        );
        assert_eq!(h.emission(task).await.unwrap().0, 3);
        // Not again while placement keeps seeing it wait, until two minutes passed.
        assert_eq!(h.asked().await, 0);
        h.later(119_000, task, 3).await;
        assert_eq!(h.asked().await, 0);
        h.later(1_000, task, 3).await;
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.events().await, 2);
        // A task placement no longer sees waiting (routed, or not ready) is left alone.
        h.f.time
            .fetch_add(600_000, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(h.asked().await, 0);
    }

    #[tokio::test]
    async fn a_new_task_version_emits_at_once() {
        let h = host(true, None).await;
        h.activated("sha256:routing-1").await;
        let task = h.graph.task("Edited while waiting", json!({}));
        h.wanted(task, 3, h.scope.project_id).await;
        assert_eq!(h.asked().await, 1);
        h.later(1_000, task, 4).await;
        assert_eq!(h.asked().await, 1);
        assert_eq!(
            (h.events().await, h.emission(task).await.unwrap().0),
            (2, 4)
        );
        assert_eq!(h.asked().await, 0);
    }

    #[tokio::test]
    async fn a_task_nothing_can_take_is_not_asked_about_again_until_something_changes() {
        let h = host(true, None).await;
        h.activated("sha256:routing-1").await;
        let task = h.graph.task("No level for it", json!({}));
        h.wanted(task, 3, h.scope.project_id).await;
        assert_eq!(h.asked().await, 1);
        // The playbook's route is refused: Settings offer nothing at the level.
        let refused = h.route(task, 3, json!({"level": "deep-high"})).await;
        assert_eq!(refused.output["reason"], "not_offered");
        h.later(600_000, task, 3).await;
        assert_eq!(h.asked().await, 0);
        // Settings change: asked again, once.
        (h.routing.options.lock().unwrap().classes).insert(
            "deep-high".parse().unwrap(),
            vec![offer("anthropic", "claude")],
        );
        h.later(120_000, task, 3).await;
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.emission(task).await.unwrap().1, None);
        // The document allows nobody: not again for Settings, only for a new artifact.
        let input = json!({"subject": {"id": task.to_string(), "revision": 3},
            "reasons": [{"code": "policy", "detail": "Never."}]});
        assert_eq!(
            h.run("never", "work.unroutable", input).await.outcome,
            "recorded"
        );
        h.routing.options.lock().unwrap().classes.clear();
        h.later(600_000, task, 3).await;
        assert_eq!(h.asked().await, 0);
        h.activated("sha256:routing-2").await;
        h.later(120_000, task, 3).await;
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.events().await, 3);
    }

    #[tokio::test]
    async fn nothing_is_emitted_without_an_active_routing_playbook_or_for_another_project() {
        // No routing artifact is active: there is nothing to route with.
        let h = host(true, None).await;
        let task = h.graph.task("Nobody to ask", json!({}));
        h.wanted(task, 3, h.scope.project_id).await;
        assert_eq!((h.asked().await, h.events().await), (0, 0));
        h.activated("sha256:routing-1").await;
        assert_eq!(h.asked().await, 1);
        // Without installation routing, only the namespace's project is asked about.
        let own = host(false, None).await;
        own.activated("sha256:routing-1").await;
        let elsewhere = own.graph.task("Another project", json!({}));
        own.wanted(elsewhere, 3, own.other_project).await;
        assert_eq!((own.asked().await, own.events().await), (0, 0));
        let wide = host(true, None).await;
        wide.activated("sha256:routing-1").await;
        wide.wanted(elsewhere, 3, wide.other_project).await;
        assert_eq!(wide.asked().await, 1);
    }

    /// Answers the first field `low` (or its first choice) and no flag.
    struct LowRisk;
    #[async_trait::async_trait]
    impl JudgmentProvider for LowRisk {
        fn credential_profile(&self, _: Uuid, _: Uuid) -> Option<String> {
            Some("profile/judge".into())
        }
        async fn complete(
            &self,
            _: &JudgmentRequest,
        ) -> std::result::Result<JudgmentReply, ProviderFailure> {
            Err(ProviderFailure::Unavailable)
        }
        async fn assess(
            &self,
            r: &AssessRequest,
        ) -> std::result::Result<AssessReply, &'static str> {
            let properties = |part: &str| {
                r.schema["properties"][part]["properties"]
                    .as_object()
                    .unwrap()
                    .clone()
            };
            let labels: serde_json::Map<String, Value> = (properties("labels").into_iter())
                .map(|(id, field)| {
                    let choices = field["enum"].as_array().unwrap();
                    let low = choices.iter().find(|c| *c == "low").unwrap_or(&choices[0]);
                    (id, low.clone())
                })
                .collect();
            let flags: serde_json::Map<String, Value> = (properties("flags").into_iter())
                .map(|(id, _)| (id, json!(false)))
                .collect();
            Ok(AssessReply {
                answer: json!({"labels": labels, "flags": flags, "rationale": "Only documentation changes."}),
                provider: "fixture".into(),
                model: "model-a".into(),
                input_tokens: 1,
                output_tokens: 1,
            })
        }
    }

    // The whole path: placement records a waiting task, the server sends
    // aq.work.route_needed, the event starts a run of the installed artifact
    // that triggers on it (the engine's worked example, sealed here), and
    // the run reads the task, asks the judgment and writes the route.
    #[tokio::test]
    async fn the_event_starts_a_run_of_an_installed_artifact_that_triggers_on_it() {
        use aq_playbooks_compiler::draft;
        use aq_server::playbooks::driver::Driver;
        let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/aq-playbooks-c744301/crates/aq-playbooks-compiler/authoring-examples/escalate-on-judgment");
        let workspace = draft::Workspace::read(&example).unwrap();
        let artifact = draft::seal(&workspace).unwrap();
        let h = host(true, Some(Arc::new(LowRisk))).await;
        (h.routing.options.lock().unwrap().classes).insert(
            "simple-high".parse().unwrap(),
            vec![offer("anthropic", "claude")],
        );
        h.service.retire_runtime(&h.runtime).await.unwrap();
        // The engine on protected PostgreSQL, in the fixture's database.
        let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
        url.set_path(h.f.db.options().get_database().unwrap());
        let factory = move || {
            let config = url
                .as_str()
                .parse::<aq_playbook_host::postgres_client::Config>();
            let client = (config.unwrap()).connect(aq_playbook_host::postgres_client::NoTls);
            let keys = aq_playbook_host::postgres_storage::ProtectionKeyring::new(
                "host9",
                BTreeMap::from([("host9".into(), [9; 32])]),
            );
            aq_playbook_host::protected::storage(
                client.map_err(|_| aq_playbook_host::Error::Unknown)?,
                keys.unwrap(),
                std::time::Duration::from_secs(5),
                std::time::Duration::from_millis(500),
            )
        };
        let limits = aq_playbook_host::Limits::default();
        let driver = Driver::start(h.service.clone(), limits, factory)
            .await
            .unwrap();
        driver.install(artifact.clone()).await.unwrap();
        // Codex review 4: activated through the driver alone (as a configured
        // deployment or the authoring path does), with no ledger row: the
        // server's record of the engine binding is what the event source reads.
        let active = driver.activate("aq.routing".into(), artifact.digest.clone(), h.grant, 1);
        assert_eq!(active.await.unwrap()["artifact"], artifact.digest.as_str());
        let task = h
            .graph
            .task("Fix a typo in the install guide", json!({"aq:priority": 2}));
        h.wanted(task, 3, h.scope.project_id).await;
        for _ in 0..200 {
            driver.quantum().await.unwrap();
            if h.graph.stored(task).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stored = h.graph.stored(task).expect("the run routed the task");
        assert_eq!(
            (stored.c.to_string().as_str(), stored.s),
            ("simple-high", Source::Router)
        );
        assert_eq!(stored.d.as_deref(), Some(&artifact.digest[..16]));
        let (_, route) = h.routing.writes.lock().unwrap()[0].clone();
        let route = route.unwrap();
        assert_eq!(route.judgment.as_ref().unwrap()["labels"], json!(["low"]));
        assert!(!route.reasons.is_empty());
        // One cause, one run: more quanta start nothing and write nothing.
        for _ in 0..10 {
            driver.quantum().await.unwrap();
        }
        let runs = driver
            .inspect_runs(h.f.actor, Default::default())
            .await
            .unwrap();
        assert_eq!(runs.items.len(), 1);
        assert_eq!(h.routing.writes.lock().unwrap().len(), 1);
        driver.stop().await.unwrap();
    }
    // --------------------------------------------- routing failures (0011 W6)

    impl Host {
        /// What was raised about a task: (reason, cause, escalation, detail, project).
        async fn raised(&self, task: Uuid) -> Vec<(String, String, String, String, Option<Uuid>)> {
            sqlx::query_as("SELECT reason,request->>'input',escalation,detail,project_id FROM internal_tasks WHERE purpose='routing_classification' AND affected_kind='task' AND affected_id=$1 AND escalate ORDER BY created_at_ms,request->>'input'")
                .bind(task.to_string())
                .fetch_all(&self.f.db.pool().await)
                .await
                .unwrap()
        }
        async fn causes(&self, task: Uuid) -> Vec<String> {
            (self.raised(task).await.into_iter()).map(|r| r.1).collect()
        }
    }

    #[tokio::test]
    async fn a_task_routing_cannot_route_is_raised_once_per_cause_and_again_for_a_new_version() {
        let h = host(true, None).await;
        let task = h.graph.task("Needs a level nobody offers", json!({}));
        let unoffered = json!({"level": "deep-high", "harnesses": ["claude"],
            "reasons": [{"code": "deep_reserved", "detail": "Hard analysis needs Deep."}]});
        assert_eq!(
            h.route(task, 3, unoffered.clone()).await.output["reason"],
            "not_offered"
        );
        let raised = h.raised(task).await;
        assert_eq!(raised.len(), 1);
        let (reason, cause, escalation, detail, project) = raised[0].clone();
        assert_eq!(
            (reason.as_str(), cause.as_str(), escalation.as_str()),
            ("no_candidate", "not_offered@3", "none")
        );
        assert_eq!(project, Some(h.scope.project_id));
        // In words the owner can act on, with the playbook's own reasons.
        assert!(
            detail.contains("chose level deep-high, harnesses claude"),
            "{detail}"
        );
        assert!(
            detail.contains("Settings > Intelligence")
                && detail.ends_with("Hard analysis needs Deep.")
        );
        // The same refusal again (another run, the same task version): not raised twice.
        assert_eq!(
            h.route(task, 3, unoffered.clone()).await.output["reason"],
            "not_offered"
        );
        h.f.time
            .fetch_add(600_000, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            h.route(task, 3, unoffered.clone()).await.output["reason"],
            "not_offered"
        );
        assert_eq!(h.causes(task).await, ["not_offered@3"]);
        // It follows the internal work failure path: a supervisor first.
        let pool = h.f.db.pool().await;
        let moved = aq_internal_work::store::escalate_due(&pool, h.clock())
            .await
            .unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(
            (
                moved[0].affected_kind.as_str(),
                moved[0].affected_id.clone()
            ),
            ("task", task.to_string())
        );
        assert_eq!(h.raised(task).await[0].2, "supervisor");
        // Even resolved, the same cause at the same version is never raised again.
        aq_internal_work::store::resolve(&pool, moved[0].id)
            .await
            .unwrap();
        assert_eq!(
            h.route(task, 3, unoffered.clone()).await.output["reason"],
            "not_offered"
        );
        assert_eq!(h.causes(task).await, ["not_offered@3"]);
        // A different cause for the same version is raised.
        let nobody = json!({"subject": {"id": task.to_string(), "revision": 3},
            "reasons": [{"code": "policy", "detail": "Never route payroll work."}]});
        assert_eq!(
            h.run("nobody", "work.unroutable", nobody).await.outcome,
            "recorded"
        );
        assert_eq!(h.causes(task).await, ["not_offered@3", "unroutable@3"]);
        let words = h.raised(task).await[1].3.clone();
        assert!(
            words.contains("allows no agent") && words.ends_with("Never route payroll work."),
            "{words}"
        );
        // A new task version: the same refusal is raised again, once.
        h.graph.nodes.lock().unwrap().get_mut(&task).unwrap()["version"] = json!(4);
        for _ in 0..2 {
            assert_eq!(
                h.route(task, 4, unoffered.clone()).await.output["reason"],
                "not_offered"
            );
        }
        let mut causes = h.causes(task).await;
        causes.sort();
        assert_eq!(causes, ["not_offered@3", "not_offered@4", "unroutable@3"]);
        // A route that is written raises nothing.
        let fine = h.graph.task("Routable", json!({}));
        assert_eq!(h.route(fine, 3, json!({})).await.outcome, "routed");
        assert!(h.raised(fine).await.is_empty());
    }

    #[tokio::test]
    async fn a_task_unrouted_for_ten_minutes_is_raised_once_and_again_for_a_new_version() {
        let h = host(true, None).await;
        h.activated("sha256:routing-1").await;
        let task = h.graph.task("Its routing run never routes it", json!({}));
        h.wanted(task, 3, h.scope.project_id).await;
        assert_eq!(h.asked().await, 1);
        // Asked again every two minutes; nothing is raised before ten.
        for _ in 0..4 {
            h.later(120_000, task, 3).await;
            assert_eq!(h.asked().await, 1);
        }
        assert!(h.raised(task).await.is_empty());
        h.later(120_000, task, 3).await;
        h.asked().await;
        let raised = h.raised(task).await;
        assert_eq!(raised.len(), 1);
        assert_eq!(
            (raised[0].0.as_str(), raised[0].1.as_str()),
            ("deadline_exceeded", "unrouted@3")
        );
        assert!(
            raised[0].3.contains("more than 10 minutes"),
            "{}",
            raised[0].3
        );
        assert_eq!(raised[0].4, Some(h.scope.project_id));
        // Still waiting later: not raised twice for this version.
        for _ in 0..6 {
            h.later(120_000, task, 3).await;
            h.asked().await;
        }
        assert_eq!(h.causes(task).await, ["unrouted@3"]);
        // A new version waits its own ten minutes, then is raised once.
        h.later(1_000, task, 4).await;
        h.asked().await;
        assert_eq!(h.causes(task).await, ["unrouted@3"]);
        for _ in 0..6 {
            h.later(120_000, task, 4).await;
            h.asked().await;
        }
        assert_eq!(h.causes(task).await, ["unrouted@3", "unrouted@4"]);
        // A task already raised as not_offered is not also raised as waiting.
        let other = h.graph.task("Refused, then waiting", json!({}));
        h.wanted(other, 3, h.scope.project_id).await;
        h.asked().await;
        assert_eq!(
            h.route(other, 3, json!({"level": "deep-high"}))
                .await
                .output["reason"],
            "not_offered"
        );
        for _ in 0..7 {
            h.later(120_000, other, 3).await;
            h.asked().await;
        }
        assert_eq!(h.causes(other).await, ["not_offered@3"]);
    }
    // ------------------------------------------------ fix round (Codex review)

    impl Host {
        async fn waiting(&self, task: Uuid) -> Option<i64> {
            sqlx::query_scalar("SELECT wanted_at_ms FROM route_needed_emissions WHERE task_id=$1")
                .bind(task)
                .fetch_one(&self.f.db.pool().await)
                .await
                .unwrap()
        }
    }

    // Finding 6: a written route ends the waiting episode. Placement's last
    // sighting of the task waiting stays fresh for minutes; it must not bring
    // another event due once the task has its route.
    #[tokio::test]
    async fn a_routed_task_is_not_asked_about_again() {
        let h = host(true, None).await;
        h.activated("sha256:routing-1").await;
        let task = h.graph.task("Routed by its first run", json!({}));
        h.wanted(task, 3, h.scope.project_id).await;
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.route(task, 3, json!({})).await.outcome, "routed");
        assert_eq!(h.waiting(task).await, None);
        for _ in 0..3 {
            h.f.time
                .fetch_add(120_000, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(h.asked().await, 0);
        }
        assert_eq!(h.events().await, 1);
        assert!(h.raised(task).await.is_empty());
    }

    // Finding 3: a run that read the task holds back a new ask only while it
    // can still be running. Its end may never be readable (run history is a
    // rolling record), so after the bound it no longer counts.
    #[tokio::test]
    async fn a_run_whose_end_is_not_on_record_stops_holding_back_the_ask() {
        let h = host(true, None).await;
        h.activated("sha256:routing-1").await;
        let task = h.graph.task("Its run left no history", json!({}));
        h.wanted(task, 3, h.scope.project_id).await;
        assert_eq!(h.asked().await, 1);
        // A run (not in any history) read the task and never routed it.
        assert_eq!(h.read(task, 3).await.outcome, "observed");
        for _ in 0..7 {
            h.later(120_000, task, 3).await;
            assert_eq!(h.asked().await, 0, "the run may still be running");
        }
        h.later(120_000, task, 3).await;
        assert_eq!(
            h.asked().await,
            1,
            "fifteen minutes on, it no longer holds the ask back"
        );
    }

    // Finding 5: a refusal is settled under the artifact that was asked. A
    // run that began under an earlier artifact does not speak for the one
    // activated since: that one is asked.
    #[tokio::test]
    async fn an_old_run_does_not_settle_the_task_for_a_newly_activated_artifact() {
        let h = host(true, None).await;
        h.activated("sha256:routing-1").await;
        let task = h.graph.task("Judged under the old document", json!({}));
        h.wanted(task, 3, h.scope.project_id).await;
        assert_eq!(h.asked().await, 1);
        // The owner's new document is activated while the old run is waiting.
        h.activated("sha256:routing-2").await;
        let input = json!({"subject": {"id": task.to_string(), "revision": 3},
            "reasons": [{"code": "policy", "detail": "The old document allowed nobody."}]});
        assert_eq!(
            h.run("old", "work.unroutable", input.clone()).await.outcome,
            "recorded"
        );
        let stamped: Option<String> = sqlx::query_scalar(
            "SELECT artifact_digest FROM route_needed_emissions WHERE task_id=$1",
        )
        .bind(task)
        .fetch_one(&h.f.db.pool().await)
        .await
        .unwrap();
        assert_eq!(stamped.as_deref(), Some("sha256:routing-1"));
        h.later(120_000, task, 3).await;
        assert_eq!(
            h.asked().await,
            1,
            "the new artifact is asked about the unchanged task"
        );
        // Its own refusal settles the task for it.
        assert_eq!(
            h.run("new", "work.unroutable", input).await.outcome,
            "recorded"
        );
        h.later(600_000, task, 3).await;
        assert_eq!(h.asked().await, 0);
        // And a route records the artifact that was asked, not a later one.
        let other = h
            .graph
            .task("Routed by a run of the old document", json!({}));
        h.wanted(other, 3, h.scope.project_id).await;
        assert_eq!(h.asked().await, 1);
        h.activated("sha256:routing-3").await;
        assert_eq!(h.route(other, 3, json!({})).await.outcome, "routed");
        let (_, route) = h.routing.writes.lock().unwrap().last().unwrap().clone();
        assert_eq!(
            route.unwrap().policy_digest.as_deref(),
            Some("sha256:routing-2")
        );
    }

    // Finding 2: the route is written before the intent's result is recorded.
    // If the server stops between the two, the intent is dispatched again:
    // the task has moved to its next version, so the repeat is `stale` and
    // writes nothing more. The route is written once.
    #[tokio::test]
    async fn a_route_write_repeated_after_a_crash_is_stale_and_writes_nothing_more() {
        let h = host(true, None).await;
        let task = h.graph.task("Written, then the server stopped", json!({}));
        // The write of the lost dispatch, as the adapter makes it.
        let lost = aq_server::playbooks::domain::host9::route_from_input(
            &json!({"level": "standard-high", "reasons": []}),
            &h.routing.options.lock().unwrap().clone(),
            Default::default(),
        );
        let at = (h.scope.workspace_id, task, 3);
        assert!(matches!(
            h.routing.write(at, lost.as_ref()).await.unwrap(),
            Written::Routed { .. }
        ));
        let repeat = h.route(task, 3, json!({})).await;
        assert_eq!(repeat.outcome, "stale");
        assert_eq!(h.routing.writes.lock().unwrap().len(), 1);
        assert_eq!(h.graph.stored(task).unwrap().n, 1);
        assert!(h.raised(task).await.is_empty());
    }
}

// ------------------------------------------------------------------------
// Epic 0 (spec 0016 section 6): host catalog 2.3.0 (profile 10), the event
// `aq.work.result_ready` and the result facts of `work.read`.
#[cfg(feature = "delivery_playbook")]
mod delivery {
    use super::adapters::{FakeGraph, FakeRouting};
    use crate::common;
    use aq_playbook_host::{
        model::digest,
        runner::{Intent, IntentStatus},
    };
    use aq_role_settings::delivery::DeliveryLimits;
    use aq_routing::classify::Options;
    use aq_server::{
        playbooks::{
            domain::{
                GrantScope, Result,
                delivery_decisions::Settled,
                delivery_store::{DeliveryPort, Latest, Pending},
            },
            host_adapters::DeliverySettlement,
            host_adapters::HostResult,
            review_domain::{
                CatalogProfile, DELIVERY_COMMANDS, HOST9_CAPABILITIES, HOST10_CAPABILITIES,
                HOST10_DELIVERY_CAPABILITIES, HOST10_DELIVERY_COMMANDS, HOST10_FINGERPRINT,
                HOST10_VERSION,
            },
            service::*,
        },
        protected_storage::{KeyFile, Keyring, ProtectionConfig},
    };
    use serde_json::{Value, json};
    use std::{
        collections::{BTreeMap, BTreeSet},
        os::unix::fs::PermissionsExt,
        sync::{Arc, Mutex, atomic::Ordering},
    };
    use uuid::Uuid;

    /// Everything a Host10 grant may name.
    fn host10_capabilities() -> BTreeSet<String> {
        HOST10_CAPABILITIES
            .iter()
            .chain(&HOST10_DELIVERY_CAPABILITIES)
            .map(|c| c.to_string())
            .collect()
    }

    #[test]
    fn host10_is_the_pinned_catalog_and_adds_the_event_and_the_result_facts() {
        let registry = CatalogProfile::Host10.registry();
        let manifest: Value = serde_json::from_str(include_str!(
            "../../../../vendor/aq-host-catalog-2.3.0/manifest.json"
        ))
        .unwrap();
        assert_eq!(manifest["version"], HOST10_VERSION);
        assert_eq!(manifest["fingerprint"], HOST10_FINGERPRINT);
        assert_eq!(
            manifest["supersedes"]["fingerprint"],
            super::HOST9_FINGERPRINT
        );
        assert_eq!(
            digest(&(HOST10_VERSION, &registry)).unwrap(),
            HOST10_FINGERPRINT
        );
        let host10 = CatalogProfile::Host10;
        assert_eq!((host10.number(), host10.config_name()), (10, "host_v10"));
        assert_eq!(CatalogProfile::from_number(10), Some(host10));
        assert!(host10.is_host2() && host10.has_routing() && host10.is_delivery());
        assert!(!CatalogProfile::Host9.is_delivery());
        assert!(registry.events.contains_key("aq.work.result_ready"));
        assert_eq!(registry.commands["work.read"].version, "2.3.0");
        // Only work.read changed from 2.2.0; the routing contracts are Host9's.
        let host9 = CatalogProfile::Host9.registry();
        for (name, contract) in &host9.commands {
            if name != "work.read" {
                assert_eq!(registry.commands.get(name), Some(contract), "{name}");
            }
        }
        for (name, contract) in &host9.events {
            assert_eq!(registry.events.get(name), Some(contract), "{name}");
        }
        // The Host9 grant scope is admitted, and the served commands are Host9's
        // names and the six asynchronous delivery commands.
        let scope = GrantScope {
            workspace_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            recipients: vec![],
            capabilities: HOST9_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        };
        assert!(host10.validate_scope(&scope).is_ok());
        // Host10 serves Host9's commands and the delivery commands implemented so far.
        let served: BTreeSet<&str> = host10.commands().iter().copied().collect();
        let mut expected: BTreeSet<&str> =
            CatalogProfile::Host9.commands().iter().copied().collect();
        expected.extend(HOST10_DELIVERY_COMMANDS);
        expected.extend(DELIVERY_COMMANDS);
        assert_eq!(served, expected);
        for name in HOST10_DELIVERY_COMMANDS {
            let contract = &registry.commands[name];
            assert_eq!(contract.capabilities, BTreeSet::from([name.to_string()]));
        }
        for name in DELIVERY_COMMANDS {
            let contract = &registry.commands[name];
            assert!(contract.asynchronous.is_some(), "{name}");
            assert!(contract.capabilities.contains(name), "{name}");
        }
        // Their capabilities are admitted in a Host10 grant and not in a Host9 one.
        let delivery = GrantScope {
            capabilities: host10_capabilities(),
            ..scope.clone()
        };
        assert!(host10.validate_scope(&delivery).is_ok());
        assert!(CatalogProfile::Host9.validate_scope(&delivery).is_err());
        // A Host9 namespace is not reused.
        assert!(host10.namespace_refusal(1, Some((9, 1))).is_some());
        // The pin lists the vendored files byte for byte.
        let pin: Value = serde_json::from_str(include_str!(
            "../../../../vendor/aq-host-catalog-2.3.0-source.json"
        ))
        .unwrap();
        assert_eq!(pin["catalog_fingerprint"], HOST10_FINGERPRINT);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/aq-host-catalog-2.3.0");
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

    /// What the server records of results: pending results, the latest result
    /// of a task and which projects it runs.
    #[derive(Default)]
    struct Port {
        pending: Mutex<Vec<Pending>>,
        latest: Mutex<BTreeMap<Uuid, Latest>>,
        bound: Mutex<BTreeSet<Uuid>>,
        /// Every (task, result) recorded, the latest and earlier ones.
        known: Mutex<BTreeSet<(Uuid, Uuid)>>,
    }
    #[async_trait::async_trait]
    impl DeliveryPort for Port {
        async fn pending(&self, project: Option<Uuid>, limit: u32) -> Result<Vec<Pending>> {
            let all = self.pending.lock().unwrap();
            let of = |p: &&Pending| project.is_none_or(|id| id == p.project_id);
            Ok(all
                .iter()
                .filter(of)
                .take(limit as usize)
                .copied()
                .collect())
        }
        async fn latest(&self, task: Uuid) -> Result<Option<Latest>> {
            Ok(self.latest.lock().unwrap().get(&task).cloned())
        }
        async fn bound(&self, project: Uuid) -> Result<bool> {
            Ok(self.bound.lock().unwrap().contains(&project))
        }
        async fn has_result(&self, task: Uuid, result: Uuid) -> Result<bool> {
            Ok(self.known.lock().unwrap().contains(&(task, result)))
        }
    }

    /// The server's settlement as `work.complete` and `work.return` call it:
    /// answers what the test sets, and counts the calls.
    pub(super) struct Settle {
        pub answer: Mutex<Settled>,
        /// (`complete` or the return reason, result) per call.
        pub calls: Mutex<Vec<(String, Uuid)>>,
    }
    impl Default for Settle {
        fn default() -> Self {
            Self {
                answer: Mutex::new(Settled::Completed),
                calls: Mutex::default(),
            }
        }
    }
    #[async_trait::async_trait]
    impl DeliverySettlement for Settle {
        async fn complete(&self, result: Uuid) -> Result<Settled> {
            self.calls.lock().unwrap().push(("complete".into(), result));
            Ok(*self.answer.lock().unwrap())
        }
        async fn send_back(&self, result: Uuid, reason: &str) -> Result<Settled> {
            self.calls.lock().unwrap().push((reason.into(), result));
            Ok(*self.answer.lock().unwrap())
        }
    }

    struct D {
        f: common::profiled::Fixture,
        service: Service,
        runtime: RuntimeOwner,
        grant: Uuid,
        scope: GrantScope,
        graph: Arc<FakeGraph>,
        routing: Arc<FakeRouting>,
        port: Arc<Port>,
        settle: Arc<Settle>,
        other_project: Uuid,
    }
    async fn delivery(installation_routing: bool) -> D {
        let f = common::profiled::fixture().await;
        common::profiled::insert(&f).await;
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
            // Nothing is on the automatic router: delivery does not depend on it.
            on_router: Mutex::default(),
            attempts: Mutex::new((1, 2)),
            options: Mutex::new(Options::default()),
            writes: Mutex::default(),
        });
        let port = Arc::new(Port::default());
        let settle = Arc::new(Settle::default());
        port.bound
            .lock()
            .unwrap()
            .extend([l.project_id, other_project]);
        let mut service = Service::new_host2(
            &f.state,
            Namespace {
                namespace_id: Uuid::new_v4(),
                generation: 1,
                name: format!("host10-{}", Uuid::new_v4()),
                workspace_id: l.workspace_id,
                project_id: l.project_id,
                owner_user_id: f.actor,
            },
            Arc::new(keys),
            CatalogProfile::Host10,
            graph.clone(),
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_routing(routing.clone())
        .with_delivery(port.clone())
        .with_settlement(settle.clone());
        service.installation_routing = installation_routing;
        service.initialize(f.actor).await.unwrap();
        let now = (f.state.clock)().timestamp_millis();
        let scope = GrantScope {
            workspace_id: l.workspace_id,
            project_id: l.project_id,
            recipients: vec![],
            capabilities: host10_capabilities(),
        };
        let grant = Uuid::new_v4();
        // The longest grant `Service::grant` admits (24 h): the tests move the clock by hours.
        service
            .grant(f.actor, grant, scope.clone(), now + 86_400_000)
            .await
            .unwrap();
        let runtime = service.adopt_runtime(Uuid::new_v4(), 0).await.unwrap();
        D {
            f,
            service,
            runtime,
            grant,
            scope,
            graph,
            routing,
            port,
            settle,
            other_project,
        }
    }

    impl D {
        async fn pool(&self) -> sqlx::PgPool {
            self.f.db.pool().await
        }
        fn clock(&self) -> i64 {
            (self.f.state.clock)().timestamp_millis()
        }
        fn later(&self, ms: i64) {
            self.f.time.fetch_add(ms, Ordering::SeqCst);
        }
        async fn read(&self, task: Uuid) -> HostResult {
            let input = json!({"subject": {"id": task.to_string(), "revision": 1}});
            self.run(&format!("read-{}", Uuid::new_v4()), "work.read", input)
                .await
        }
        async fn run(&self, id: &str, name: &str, input: Value) -> HostResult {
            let task = input["task"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok());
            let mut intent = self.intent(id, name, input);
            if DELIVERY_COMMANDS.contains(&name)
                && let Some(task) =
                    task.filter(|task| self.port.latest.lock().unwrap().contains_key(task))
            {
                intent.run = self.event_run(task).await;
            }
            let result = self.try_run(&intent).await.unwrap();
            // Every answer is one the catalog declares.
            (intent.command.outcomes[&result.outcome].validate(&result.output)).unwrap();
            result
        }
        async fn event_run(&self, task: Uuid) -> String {
            self.activated().await;
            self.asked().await;
            let (_, id, event) = self
                .service
                .outbox(&self.runtime, 0, 16)
                .await
                .unwrap()
                .into_iter()
                .find(|(_, _, event)| {
                    event.contract.name == "aq.work.result_ready"
                        && event.payload["task_id"] == task.to_string()
                })
                .unwrap();
            let key = digest(&(&event.producer, 0_u64, &event.event_id)).unwrap();
            let run = format!("event-{key}");
            self.service
                .acknowledge_event(
                    &self.runtime,
                    id,
                    aq_playbook_host::runner::EventDisposition {
                        key,
                        duplicate: false,
                        destinations: 1,
                    },
                    false,
                )
                .await
                .unwrap();
            self.service
                .record_delivery_runs(&self.runtime, id, &event, &json!([run]))
                .await
                .unwrap();
            run
        }

        /// Dispatches `intent` (a replay when it was dispatched before).
        async fn try_run(&self, intent: &Intent) -> Result<HostResult> {
            (self.service)
                .host_dispatch(&self.runtime, self.grant, 1, intent)
                .await
        }
        fn intent(&self, id: &str, name: &str, input: Value) -> Intent {
            let registry = CatalogProfile::Host10.registry();
            Intent {
                id: id.into(),
                run: "synthetic-run".into(),
                step: name.replace('.', "-"),
                invocation: 1,
                deadline_at_ms: self.clock() + 5000,
                command: registry.commands[name].clone(),
                input_digest: digest(&input).unwrap(),
                input,
                authority: CatalogProfile::Host10
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
        /// An artifact is active on the delivery binding.
        async fn activated(&self) {
            sqlx::query("INSERT INTO playbook_engine_bindings(namespace_id,generation,binding,artifact,at_ms) VALUES($1,1,'aq.delivery','sha256:delivery-1',1) ON CONFLICT DO NOTHING")
                .bind(self.service.namespace.namespace_id)
                .execute(&self.pool().await)
                .await
                .unwrap();
        }
        async fn limits(&self, limits: DeliveryLimits) {
            let now = self.clock();
            let saved = aq_role_settings::save_delivery_limits(
                &self.pool().await,
                self.f.actor,
                &limits,
                0,
                Uuid::new_v4(),
                now,
            )
            .await;
            saved.unwrap();
        }
        /// A task whose current attempt stopped with a recorded result of `project`.
        fn stopped(&self, project: Uuid, outcome: &str) -> (Uuid, Pending) {
            let task = self.graph.task("Stopped with a result", json!({}));
            let pending = Pending {
                task_id: task,
                workspace_id: self.scope.workspace_id,
                project_id: project,
                attempt_id: Uuid::new_v4(),
                result_id: Uuid::new_v4(),
            };
            let latest = Latest {
                workspace_id: pending.workspace_id,
                project_id: project,
                attempt_id: pending.attempt_id,
                result_id: pending.result_id,
                outcome: outcome.into(),
                changed: true,
                head: "a".repeat(40),
                base: "b".repeat(40),
            };
            self.port.pending.lock().unwrap().push(pending);
            self.port.latest.lock().unwrap().insert(task, latest);
            (self.port.known.lock().unwrap()).insert((task, pending.result_id));
            (task, pending)
        }
        async fn asked(&self) -> usize {
            (self.service.collect_result_ready(&self.runtime, 8).await).unwrap()
        }
        async fn events(&self) -> Vec<Value> {
            let page = self.service.outbox(&self.runtime, 0, 16).await.unwrap();
            page.into_iter()
                .filter(|e| e.2.contract.name == "aq.work.result_ready")
                .map(|e| e.2.payload)
                .collect()
        }
        /// main moved to `head` in `project`.
        async fn main_moved(&self, project: Uuid, head: &str) {
            let sql = "INSERT INTO delivery_main_heads(project_id,head,observed_at_ms) VALUES($1,$2,$3) ON CONFLICT(project_id) DO UPDATE SET head=$2,observed_at_ms=$3";
            let query = sqlx::query(sql).bind(project).bind(head).bind(self.clock());
            query.execute(&self.pool().await).await.unwrap();
        }
        /// The task's pull request, as the pull_request commands record it.
        async fn pull_request(&self, task: Uuid, checks: &str, failed: Value) {
            let sql = "INSERT INTO delivery_pull_requests(task_id,number,url,state,mergeable,checks_state,checks_failed,updated_at_ms) VALUES($1,7,'https://example.test/pr/7','open','mergeable',$2,$3,$4) ON CONFLICT(task_id) DO UPDATE SET checks_state=$2,checks_failed=$3,updated_at_ms=$4";
            let query = sqlx::query(sql).bind(task).bind(checks).bind(failed);
            query
                .bind(self.clock())
                .execute(&self.pool().await)
                .await
                .unwrap();
        }
        async fn decided(&self, task: Uuid, result: Uuid, decision: &str) {
            let sql = "INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES($1,$2,$3,NULL,$4)";
            let query = sqlx::query(sql).bind(result).bind(task).bind(decision);
            query
                .bind(self.clock())
                .execute(&self.pool().await)
                .await
                .unwrap();
        }
    }

    // `work.complete`, `work.return`, `work.hold` and `supervisor.notify`.
    mod decisions;

    #[tokio::test]
    async fn a_stopped_result_is_announced_once_and_again_after_the_decision_repeat_setting() {
        let h = delivery(false).await;
        // The repeat is Settings' (`decision_repeat_ms`), not a constant.
        h.limits(DeliveryLimits {
            decision_repeat_ms: 30_000,
            ..Default::default()
        })
        .await;
        let (task, pending) = h.stopped(h.scope.project_id, "success");
        // No artifact is active on the delivery binding: nothing is sent.
        assert_eq!((h.asked().await, h.events().await.len()), (0, 0));
        h.activated().await;
        assert_eq!(h.asked().await, 1);
        let events = h.events().await;
        assert_eq!(
            events,
            [
                json!({"task_id": task.to_string(), "project_id": pending.project_id.to_string(),
                "attempt_id": pending.attempt_id.to_string(), "result_id": pending.result_id.to_string(),
                "version": 3})
            ]
        );
        // Level-triggered: not again until the setting has passed.
        assert_eq!(h.asked().await, 0);
        h.later(29_999);
        assert_eq!(h.asked().await, 0);
        h.later(1);
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.events().await.len(), 2);
    }

    #[tokio::test]
    async fn a_result_with_a_decision_is_not_announced_and_a_new_result_is_at_once() {
        let h = delivery(false).await;
        h.activated().await;
        let (task, pending) = h.stopped(h.scope.project_id, "success");
        assert_eq!(h.asked().await, 1);
        // A playbook decided on the result (complete, return or hold): never again.
        h.decided(task, pending.result_id, "held").await;
        h.later(10_000_000);
        assert_eq!(h.asked().await, 0);
        // The task is sent back and its next attempt stops with another result.
        let next = Pending {
            result_id: Uuid::new_v4(),
            attempt_id: Uuid::new_v4(),
            ..pending
        };
        *h.port.pending.lock().unwrap() = vec![pending, next];
        assert_eq!(h.asked().await, 1);
        let events = h.events().await;
        assert_eq!(events[1]["result_id"], next.result_id.to_string());
        assert_eq!(h.asked().await, 0);
    }

    #[tokio::test]
    async fn nothing_is_announced_while_a_run_that_read_the_task_is_unfinished() {
        let h = delivery(false).await;
        h.activated().await;
        let (task, _) = h.stopped(h.scope.project_id, "success");
        assert_eq!(h.asked().await, 1);
        // The run the event started reads the task and has not finished.
        assert_eq!(h.read(task).await.outcome, "observed");
        // Neither the repeat interval nor main moving announces it again...
        h.later(120_000);
        h.main_moved(h.scope.project_id, &"c".repeat(40)).await;
        assert_eq!(h.asked().await, 0);
        // ...until the run is old enough (the longest check wait and the repeat, Settings) to be given up.
        h.later(3_700_000);
        assert_eq!(h.asked().await, 1);
    }

    #[tokio::test]
    async fn a_change_of_main_or_of_the_pull_request_announces_again_at_once() {
        let h = delivery(false).await;
        h.activated().await;
        let (task, _) = h.stopped(h.scope.project_id, "success");
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.asked().await, 0);
        // Main moves.
        h.main_moved(h.scope.project_id, &"c".repeat(40)).await;
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.asked().await, 0);
        // A pull request appears, and its checks finish.
        h.pull_request(task, "pending", json!([])).await;
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.asked().await, 0);
        h.pull_request(task, "failed", json!(["unit"])).await;
        assert_eq!(h.asked().await, 1);
        assert_eq!(h.events().await.len(), 4);
    }

    #[tokio::test]
    async fn only_the_namespaces_project_is_announced_unless_installation_routing_is_on() {
        for installation in [false, true] {
            let h = delivery(installation).await;
            h.activated().await;
            h.stopped(h.scope.project_id, "success");
            h.stopped(h.other_project, "success");
            let announced = h.asked().await;
            assert_eq!(announced, if installation { 2 } else { 1 });
        }
    }

    #[tokio::test]
    async fn work_read_gives_the_result_facts_for_a_routed_and_a_non_routed_project() {
        let h = delivery(false).await;
        let own = h.scope.project_id;
        // The router serves the namespace's project only; the other is not on it.
        h.routing.on_router.lock().unwrap().insert(own);
        let (task, _) = h.stopped(own, "success");
        let (elsewhere, _) = h.stopped(h.other_project, "failed");
        h.main_moved(own, &"c".repeat(40)).await;
        h.main_moved(h.other_project, &"d".repeat(40)).await;
        let sql = "INSERT INTO delivery_tasks(task_id,returns) VALUES($1,2)";
        sqlx::query(sql)
            .bind(task)
            .execute(&h.pool().await)
            .await
            .unwrap();
        for (task, outcome, base_main) in [(task, "success", 'c'), (elsewhere, "failed", 'd')] {
            let read = h.read(task).await;
            assert_eq!(read.outcome, "observed", "{}", read.output);
            let facts = &read.output["facts"];
            assert_eq!(facts["outcome"], outcome);
            assert_eq!(facts["changed"], true);
            assert_eq!(facts["branch"], format!("aq/{task}"));
            assert_eq!(facts["head"], "a".repeat(40));
            assert_eq!(facts["base"], "b".repeat(40));
            assert_eq!(facts["main_head"], base_main.to_string().repeat(40));
            assert_eq!(facts["pull_request"], Value::Null);
            // The facts of 2.2.0 are still there.
            assert_eq!(facts["title"], "Stopped with a result");
            assert_eq!(
                (&facts["charged_failures"], &facts["ended_attempts"]),
                (&json!(1), &json!(2))
            );
        }
        assert_eq!(h.read(task).await.output["facts"]["returns"], 2);
        assert_eq!(h.read(elsewhere).await.output["facts"]["returns"], 0);
        // A pull request, once one is known.
        h.pull_request(task, "failed", json!(["unit", "lint"]))
            .await;
        let read = h.read(task).await;
        assert_eq!(
            read.output["facts"]["pull_request"],
            json!({"number": 7, "url": "https://example.test/pr/7", "state": "open",
                "mergeable": "mergeable", "checks": {"state": "failed", "failed": ["unit", "lint"]}})
        );
    }

    #[tokio::test]
    async fn work_read_of_a_task_without_a_result_has_empty_result_facts_and_refuses_what_it_cannot_serve()
     {
        let h = delivery(false).await;
        let task = h.graph.task("Not run yet", json!({}));
        let facts = h.read(task).await.output["facts"].clone();
        assert_eq!(
            (
                &facts["outcome"],
                &facts["changed"],
                &facts["branch"],
                &facts["head"],
                &facts["base"]
            ),
            (
                &json!(""),
                &json!(false),
                &json!(""),
                &json!(""),
                &json!("")
            )
        );
        assert_eq!(
            (&facts["returns"], &facts["pull_request"]),
            (&json!(0), &Value::Null)
        );
        // A task of a project the server does not run, one that does not exist, an id that is no id.
        let stranger = h.graph.task("Elsewhere", json!({}));
        h.routing
            .located
            .lock()
            .unwrap()
            .insert(stranger, (Uuid::new_v4(), Uuid::new_v4()));
        let refused = h.read(stranger).await;
        assert_eq!(
            (refused.outcome.as_str(), &refused.output["reason"]),
            ("refused", &json!("not_on_router"))
        );
        let missing = h.read(Uuid::new_v4()).await;
        assert_eq!(missing.output["reason"], "unknown_task");
        let odd = h
            .run(
                "odd",
                "work.read",
                json!({"subject": {"id": "not-a-task", "revision": 1}}),
            )
            .await;
        assert_eq!(
            (odd.outcome.as_str(), &odd.output["reason"]),
            ("refused", &json!("unknown_task"))
        );
    }

    // ------------------------------------------------------------------
    // The asynchronous delivery commands: main.sync, branch.publish and
    // pull_request.*. A fake forge and central git stand behind the port; the
    // adapters over GitHub and aq-git are tested in `aq-delivery`.
    mod commands {
        use super::*;
        use aq_delivery::{
            Checks, ChecksState, GitHubAdapter, GitHubRepository, MergeMethod, MergeOutcome,
            Mergeability, PullRequest, PullRequestState,
            commands::{
                Delivery, DeliveryTarget, GitFailure, GitPort, GitTarget, RefDeletion,
                RefPublication, RetainedRef,
            },
            github::GitHubResult,
        };
        use aq_server::playbooks::host_adapters::DeliveryOps;

        fn oid(c: char) -> String {
            c.to_string().repeat(40)
        }

        /// Both the central git and the forge, scripted.
        struct Fake {
            log: Mutex<Vec<String>>,
            checks: Mutex<(ChecksState, Vec<String>)>,
            pr_state: Mutex<PullRequestState>,
            delete_unavailable: Mutex<bool>,
        }
        #[async_trait::async_trait]
        impl GitPort for Fake {
            async fn retain_ref(
                &self,
                _: Uuid,
                _: &GitTarget,
                reference: &str,
            ) -> std::result::Result<RetainedRef, GitFailure> {
                self.log.lock().unwrap().push(format!("retain {reference}"));
                Ok(RetainedRef {
                    head: oid('9'),
                    retention_handle: Uuid::new_v4(),
                })
            }
            async fn publish_ref(
                &self,
                _: Uuid,
                _: &GitTarget,
                reference: &str,
                expected_old: Option<&str>,
                _expected_source: Option<&str>,
            ) -> std::result::Result<RefPublication, GitFailure> {
                let lease = expected_old.unwrap_or("absent");
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("publish {reference} lease {lease}"));
                Ok(RefPublication::Published { head: oid('a') })
            }
            async fn delete_ref(
                &self,
                _: Uuid,
                _: &GitTarget,
                reference: &str,
                expected_head: Option<&str>,
            ) -> std::result::Result<RefDeletion, GitFailure> {
                assert_eq!(expected_head, Some(oid('a').as_str()));
                self.log.lock().unwrap().push(format!("delete {reference}"));
                if *self.delete_unavailable.lock().unwrap() {
                    return Err(GitFailure {
                        code: "transport".into(),
                        message: "cleanup unavailable".into(),
                        transient: true,
                    });
                }
                Ok(RefDeletion::Deleted)
            }
        }
        fn pull(state: PullRequestState) -> PullRequest {
            PullRequest {
                number: 7,
                url: "https://example.test/pr/7".into(),
                head_sha: oid('a'),
                state,
            }
        }
        #[async_trait::async_trait]
        impl GitHubAdapter for Fake {
            async fn open_or_find(
                &self,
                _: &GitHubRepository,
                _: &str,
                _: &str,
                title: &str,
                _: &str,
            ) -> GitHubResult<PullRequest> {
                self.log.lock().unwrap().push(format!("open {title}"));
                Ok(pull(self.pr_state.lock().unwrap().clone()))
            }
            async fn pull_request(
                &self,
                _: &GitHubRepository,
                _: u64,
            ) -> GitHubResult<PullRequest> {
                Ok(pull(self.pr_state.lock().unwrap().clone()))
            }
            async fn checks(
                &self,
                _: &GitHubRepository,
                _: u64,
                _: Option<&[String]>,
            ) -> GitHubResult<Checks> {
                let (state, failed) = self.checks.lock().unwrap().clone();
                Ok(Checks {
                    state,
                    failed,
                    required: vec![],
                    head_sha: oid('a'),
                })
            }
            async fn mergeability(
                &self,
                _: &GitHubRepository,
                _: u64,
            ) -> GitHubResult<Mergeability> {
                Ok(Mergeability::Clean)
            }
            async fn merge(
                &self,
                _: &GitHubRepository,
                _: u64,
                method: MergeMethod,
                expected_head_sha: &str,
                _: &str,
            ) -> GitHubResult<MergeOutcome> {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("merge {} {expected_head_sha}", method.as_str()));
                Ok(MergeOutcome::Merged { sha: oid('d') })
            }
            async fn close(&self, _: &GitHubRepository, _: u64) -> GitHubResult<PullRequest> {
                self.log.lock().unwrap().push("close".into());
                Ok(pull(PullRequestState::Closed))
            }
            async fn delete_branch(
                &self,
                _: &GitHubRepository,
                _: &str,
            ) -> GitHubResult<aq_delivery::BranchDeletion> {
                Ok(aq_delivery::BranchDeletion::Deleted)
            }
        }
        struct Ops(Arc<Fake>);
        #[async_trait::async_trait]
        impl DeliveryOps for Ops {
            async fn target(&self, _: Uuid) -> Result<Option<DeliveryTarget>> {
                Ok(Some(DeliveryTarget {
                    git: GitTarget {
                        repository_id: Uuid::from_u128(1),
                        binding_generation: 1,
                    },
                    github: GitHubRepository {
                        owner: "Acme".into(),
                        name: "widgets".into(),
                    },
                    default_branch: "main".into(),
                }))
            }
            async fn commands(&self) -> Option<Delivery> {
                Some(Delivery::new(self.0.clone(), self.0.clone()))
            }
            async fn pull_request_text(&self, _: Uuid, _: Uuid) -> Result<(String, String)> {
                Ok(("Task title".into(), "The report".into()))
            }
        }

        async fn harness() -> (D, Arc<Fake>) {
            let mut h = delivery(false).await;
            let fake = Arc::new(Fake {
                log: Mutex::default(),
                checks: Mutex::new((ChecksState::Pending, vec![])),
                pr_state: Mutex::new(PullRequestState::Open),
                delete_unavailable: Mutex::new(false),
            });
            // By value: the port of a shared (cloned) service is not replaced.
            h.service = h.service.with_delivery_ops(Arc::new(Ops(fake.clone())));
            (h, fake)
        }
        impl D {
            /// A command that answers `accepted {handle}`: its handle.
            async fn accepted(&self, name: &str, input: Value) -> String {
                let result = self
                    .run(&format!("{name}-{}", Uuid::new_v4()), name, input)
                    .await;
                assert_eq!(result.outcome, "accepted", "{name}: {}", result.output);
                result.output["handle"].as_str().unwrap().to_owned()
            }
            async fn pass(&self) -> usize {
                (self
                    .service
                    .collect_delivery_operations(&self.runtime, 4)
                    .await)
                    .unwrap()
            }
            /// Every `aq.delivery.completed` sent so far: (operation, payload).
            async fn completions(&self) -> Vec<(String, Value)> {
                let page = self.service.outbox(&self.runtime, 0, 16).await.unwrap();
                page.into_iter()
                    .filter(|e| e.2.contract.name == "aq.delivery.completed")
                    .map(|e| {
                        // The event names no subject or revision.
                        assert_eq!((&e.2.subject, &e.2.revision), (&None, &None));
                        (e.2.correlation["operation"].clone(), e.2.payload)
                    })
                    .collect()
            }
        }

        #[tokio::test]
        async fn main_sync_completes_once_with_the_head_and_main_moves_in_the_facts() {
            let (h, fake) = harness().await;
            let (task, _) = h.stopped(h.scope.project_id, "success");
            let handle = h
                .accepted(
                    "main.sync",
                    json!({"project": h.scope.project_id.to_string()}),
                )
                .await;
            assert!(
                h.completions().await.is_empty(),
                "nothing is done at dispatch"
            );
            assert_eq!(h.pass().await, 1);
            assert_eq!(
                h.completions().await,
                [(
                    handle,
                    json!({"outcome": "synced", "output": {"head": oid('9')}})
                )]
            );
            assert_eq!(*fake.log.lock().unwrap(), ["retain refs/heads/main"]);
            assert_eq!(h.read(task).await.output["facts"]["main_head"], oid('9'));
            // A completed operation is not run or completed again.
            assert_eq!(h.pass().await, 0);
            assert_eq!(h.completions().await.len(), 1);
            // An unknown project is refused at once.
            let refused = h
                .run(
                    "sync-unknown",
                    "main.sync",
                    json!({"project": "not-a-project"}),
                )
                .await;
            assert_eq!(
                (refused.outcome.as_str(), &refused.output["reason"]),
                ("refused", &json!("unknown_project"))
            );
        }

        #[tokio::test]
        async fn a_result_is_published_opened_checked_merged_and_its_branch_deleted() {
            let (h, fake) = harness().await;
            let (task, _) = h.stopped(h.scope.project_id, "success");
            let task_id = json!({"task": task.to_string()});
            let branch = format!("refs/heads/aq/{task}");

            let publish = h.accepted("branch.publish", task_id.clone()).await;
            let open = h.accepted("pull_request.open", task_id.clone()).await;
            assert_eq!(h.pass().await, 2);
            let done = h.completions().await;
            assert_eq!(
                done[0],
                (
                    publish,
                    json!({"outcome": "published", "output": {"head": oid('a')}})
                )
            );
            assert_eq!(
                done[1],
                (
                    open,
                    json!({"outcome": "opened", "output": {"number": 7, "url": "https://example.test/pr/7"}})
                )
            );
            // The first publication leases on an absent branch.
            assert_eq!(
                fake.log.lock().unwrap()[..2],
                [
                    format!("publish {branch} lease absent"),
                    "open Task title".to_string()
                ]
            );
            let facts = h.read(task).await.output["facts"].clone();
            assert_eq!(facts["pull_request"]["state"], "open");
            assert_eq!(facts["pull_request"]["number"], 7);

            // Checks are pending: the wait runs, and is not completed.
            let wait = h
                .accepted(
                    "pull_request.await_checks",
                    json!({"task": task.to_string(), "deadline_ms": 600_000}),
                )
                .await;
            assert_eq!(h.pass().await, 0);
            assert_eq!(h.completions().await.len(), 2);
            assert_eq!(
                h.read(task).await.output["facts"]["pull_request"]["checks"]["state"],
                "pending"
            );
            // They pass: exactly one completion, however many passes follow.
            *fake.checks.lock().unwrap() = (ChecksState::Passed, vec![]);
            assert_eq!(h.pass().await, 1);
            assert_eq!(h.pass().await, 0);
            let done = h.completions().await;
            assert_eq!(done.len(), 3);
            assert_eq!(done[2], (wait, json!({"outcome": "passed", "output": {}})));

            let merge = h
                .accepted(
                    "pull_request.merge",
                    json!({"task": task.to_string(), "method": "merge", "expected_head_sha": oid('a')}),
                )
                .await;
            assert_eq!(h.pass().await, 1);
            let done = h.completions().await;
            assert_eq!(done[3], (merge, json!({"outcome": "merged", "output": {}})));
            let log = fake.log.lock().unwrap().clone();
            assert_eq!(
                log[log.len() - 2..],
                [
                    format!("merge merge {}", oid('a')),
                    format!("delete {branch}")
                ]
            );
            assert_eq!(
                h.read(task).await.output["facts"]["pull_request"]["state"],
                "merged"
            );
        }

        #[tokio::test]
        async fn merge_evidence_survives_cleanup_retry() {
            let (h, fake) = harness().await;
            let (task, result) = h.stopped(h.scope.project_id, "success");
            h.pull_request(task, "passed", json!([])).await;
            let handle = h
                .accepted(
                    "pull_request.merge",
                    json!({
                        "task": task, "method": "merge", "expected_head_sha": oid('a')
                    }),
                )
                .await;

            *fake.delete_unavailable.lock().unwrap() = true;
            assert_eq!(h.pass().await, 0);
            assert!(h.completions().await.is_empty());

            let pool = h.pool().await;
            let operation = Uuid::parse_str(&handle).unwrap();
            let evidence = || {
                sqlx::query_scalar::<_, Value>(
                    "SELECT to_jsonb(e) FROM delivery_merge_evidence e WHERE operation_id=$1",
                )
                .bind(operation)
                .fetch_one(&pool)
            };
            let saved = evidence().await.unwrap();
            assert_eq!(saved["task_id"], json!(task));
            assert_eq!(saved["project_id"], json!(h.scope.project_id));
            assert_eq!(saved["attempt_id"], json!(result.attempt_id));
            assert_eq!(saved["result_id"], json!(result.result_id));
            assert_eq!(
                saved["target"]["git"]["repository_id"],
                json!(Uuid::from_u128(1))
            );
            assert_eq!(
                saved["target"]["github"],
                json!({"owner": "Acme", "name": "widgets"})
            );
            assert_eq!(saved["target_ref"], "refs/heads/main");
            assert_eq!(saved["pull_request"], 7);
            assert_eq!(saved["expected_head_sha"], oid('a'));
            assert_eq!(saved["merge_sha"], oid('d'));

            *fake.delete_unavailable.lock().unwrap() = false;
            h.later(1);
            assert_eq!(h.pass().await, 1);
            assert_eq!(h.pass().await, 0);
            assert_eq!(
                h.completions().await,
                [(handle, json!({"outcome": "merged", "output": {}}))]
            );
            assert_eq!(evidence().await.unwrap(), saved);
            let delete = format!("delete refs/heads/aq/{task}");
            assert_eq!(
                *fake.log.lock().unwrap(),
                [format!("merge merge {}", oid('a')), delete.clone(), delete]
            );
        }

        #[tokio::test]
        async fn stale_result_with_same_revision_and_head_never_publishes_or_merges() {
            let (h, fake) = harness().await;
            let (task, original) = h.stopped(h.scope.project_id, "success");
            let input = json!({"task": task});
            let run = h.event_run(task).await;
            h.pull_request(task, "passed", json!([])).await;
            let mut merge = h.intent(
                "fenced-merge",
                "pull_request.merge",
                json!({
                    "task": task, "method": "merge", "expected_head_sha": oid('a'),
                }),
            );
            merge.run.clone_from(&run);
            assert_eq!(h.try_run(&merge).await.unwrap().outcome, "accepted");
            let mut before = h.intent("fenced-before", "branch.publish", input.clone());
            before.run.clone_from(&run);
            assert_eq!(h.try_run(&before).await.unwrap().outcome, "accepted");
            assert_ne!(h.supersede_report(task), original.result_id);
            assert_eq!(h.read(task).await.output["facts"]["head"], oid('a'));

            let mut after = h.intent("fenced-after", "branch.publish", input);
            after.run = run;
            assert!(matches!(
                h.try_run(&after).await,
                Err(aq_server::playbooks::domain::Error::Conflict)
            ));
            assert_eq!(h.pass().await, 2);
            let completions = h.completions().await;
            assert_eq!(completions.len(), 2);
            assert!(
                completions
                    .iter()
                    .all(|(_, answer)| answer["outcome"] == "refused")
            );
            assert!(fake.log.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn a_tracked_closed_or_merged_pr_is_never_recreated() {
            let (h, fake) = harness().await;
            let (task, original) = h.stopped(h.scope.project_id, "success");
            h.pull_request(task, "pending", json!([])).await;
            let task_input = json!({"task": task});
            *fake.pr_state.lock().unwrap() = PullRequestState::Closed;
            h.accepted("branch.publish", task_input.clone()).await;
            assert_eq!(h.pass().await, 1);
            assert_eq!(h.completions().await[0].1["outcome"], "refused");
            let checks = json!({"task": task, "deadline_ms": 1000});
            h.accepted("pull_request.await_checks", checks.clone())
                .await;
            assert_eq!(h.pass().await, 1);
            assert_eq!(h.completions().await[1].1["outcome"], "closed");
            assert!(fake.log.lock().unwrap().is_empty());

            *fake.pr_state.lock().unwrap() = PullRequestState::Merged;
            *fake.checks.lock().unwrap() = (ChecksState::Passed, vec![]);
            h.accepted("branch.publish", task_input).await;
            assert_eq!(h.pass().await, 1);
            assert_eq!(h.completions().await[2].1["outcome"], "published");
            h.accepted("pull_request.await_checks", checks).await;
            assert_eq!(h.pass().await, 1);
            assert_eq!(h.completions().await[3].1["outcome"], "passed");
            h.accepted(
                "pull_request.merge",
                json!({
                    "task": task, "method": "merge", "expected_head_sha": oid('a'),
                }),
            )
            .await;
            assert_eq!(h.pass().await, 1);
            assert_eq!(h.completions().await[4].1["outcome"], "merged");
            let receipt: (i64, Uuid, String) = sqlx::query_as(
                "SELECT pull_request,result_id,merge_sha FROM delivery_merge_evidence WHERE task_id=$1"
            ).bind(task).fetch_one(&h.pool().await).await.unwrap();
            assert_eq!(receipt, (7, original.result_id, oid('d')));
            assert_eq!(
                *fake.log.lock().unwrap(),
                [
                    format!("merge merge {}", oid('a')),
                    format!("delete refs/heads/aq/{task}"),
                ]
            );
        }

        #[tokio::test]
        async fn failed_checks_and_the_deadline_each_complete_the_wait_once() {
            let (h, fake) = harness().await;
            // The wait is bounded by Settings, not by what the playbook asked for.
            h.limits(DeliveryLimits {
                longest_check_wait_ms: 60_000,
                ..Default::default()
            })
            .await;
            let (failing, _) = h.stopped(h.scope.project_id, "success");
            let (slow, _) = h.stopped(h.scope.project_id, "success");
            for task in [failing, slow] {
                h.pull_request(task, "pending", json!([])).await;
            }
            let wait = |task: Uuid| json!({"task": task.to_string(), "deadline_ms": 604_800_000});
            let failed = h.accepted("pull_request.await_checks", wait(failing)).await;
            *fake.checks.lock().unwrap() = (ChecksState::Failed, vec!["unit".into()]);
            assert_eq!(h.pass().await, 1);
            assert_eq!(
                h.completions().await,
                [(
                    failed,
                    json!({"outcome": "failed", "output": {"names": ["unit"]}})
                )]
            );
            let timed_out = h.accepted("pull_request.await_checks", wait(slow)).await;
            *fake.checks.lock().unwrap() = (ChecksState::Pending, vec![]);
            h.later(59_999);
            assert_eq!(h.pass().await, 0);
            h.later(1);
            assert_eq!(h.pass().await, 1);
            assert_eq!(h.pass().await, 0);
            let done = h.completions().await;
            assert_eq!(done.len(), 2);
            assert_eq!(
                done[1],
                (timed_out, json!({"outcome": "timeout", "output": {}}))
            );
        }

        #[tokio::test]
        async fn close_closes_and_deletes_the_branch_and_unknown_tasks_are_answered_by_the_catalog()
        {
            let (h, fake) = harness().await;
            let (task, _) = h.stopped(h.scope.project_id, "success");
            h.pull_request(task, "pending", json!([])).await;
            let close = h
                .accepted("pull_request.close", json!({"task": task.to_string()}))
                .await;
            assert_eq!(h.pass().await, 1);
            assert_eq!(
                h.completions().await,
                [(close, json!({"outcome": "closed", "output": {}}))]
            );
            assert_eq!(
                fake.log.lock().unwrap().clone(),
                ["close".to_string(), format!("delete refs/heads/aq/{task}")]
            );
            // A task the host does not know: refused where the catalog has a
            // refusal, `closed` where it has none.
            let unknown = json!({"task": Uuid::new_v4().to_string()});
            for (name, reason) in [
                ("branch.publish", "unknown_task"),
                ("pull_request.open", "unknown_task"),
            ] {
                let r = h
                    .run(&format!("{name}-unknown"), name, unknown.clone())
                    .await;
                assert_eq!(
                    (r.outcome.as_str(), &r.output["reason"]),
                    ("refused", &json!(reason))
                );
            }
            let merge = json!({"task": Uuid::new_v4().to_string(), "method": "merge", "expected_head_sha": oid('a')});
            let r = h.run("merge-unknown", "pull_request.merge", merge).await;
            assert_eq!(r.output["reason"], "unknown_task");
            let before = h.completions().await.len();
            h.accepted("pull_request.close", unknown.clone()).await;
            h.accepted(
                "pull_request.await_checks",
                json!({"task": Uuid::new_v4().to_string(), "deadline_ms": 5000}),
            )
            .await;
            assert_eq!(h.pass().await, 2);
            let done = h.completions().await;
            assert_eq!(done.len(), before + 2);
            assert!(done[before..].iter().all(|d| d.1["outcome"] == "closed"));
            // A merge with no open pull request, and a publication of a task
            // that has no result branch, are refused.
            let (unpublished, _) = h.stopped(h.scope.project_id, "success");
            let merge = json!({"task": unpublished.to_string(), "method": "squash", "expected_head_sha": oid('a')});
            let r = h.run("merge-none", "pull_request.merge", merge).await;
            assert_eq!(r.output["reason"], "no_pull_request");
            h.port
                .latest
                .lock()
                .unwrap()
                .get_mut(&unpublished)
                .unwrap()
                .head
                .clear();
            let r = h
                .run(
                    "publish-none",
                    "branch.publish",
                    json!({"task": unpublished.to_string()}),
                )
                .await;
            assert_eq!(
                (r.outcome.as_str(), &r.output["reason"]),
                ("refused", &json!("no_branch"))
            );
            // operation.get reports a running delivery operation, and only that.
            let (running, _) = h.stopped(h.scope.project_id, "success");
            let handle = h
                .accepted("branch.publish", json!({"task": running.to_string()}))
                .await;
            let get = h
                .run("get-delivery", "operation.get", json!({"handle": handle}))
                .await;
            assert_eq!(get.outcome, "running");
        }
    }
}
