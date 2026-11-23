use super::*;
use crate::playbook_host9::fixture;

pub(in crate::playbook_host9) struct Host {
    pub f: common::profiled::Fixture,
    pub service: Service,
    pub runtime: RuntimeOwner,
    pub grant: Uuid,
    pub scope: GrantScope,
    pub graph: Arc<FakeGraph>,
    pub routing: Arc<FakeRouting>,
    pub other_project: Uuid,
}
pub(super) fn offer(provider: &str, harness: &str) -> Offer {
    Offer {
        provider: provider.into(),
        harness: harness.into(),
    }
}
pub(in crate::playbook_host9) async fn host(
    installation_routing: bool,
    judgments: Option<Arc<dyn JudgmentProvider>>,
) -> Host {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    host_on(f, installation_routing, judgments).await
}
pub(in crate::playbook_host9) async fn host_on(
    f: common::profiled::Fixture,
    installation_routing: bool,
    judgments: Option<Arc<dyn JudgmentProvider>>,
) -> Host {
    let l = f.selection.lineage.clone();
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
        fixture::namespace(&f, "host9"),
        fixture::keys(),
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
    let grant = fixture::Grant::initialize(
        &f,
        &service,
        BTreeSet::from(HOST9_CAPABILITIES.map(String::from)),
        10_000_000,
    )
    .await;
    Host {
        f,
        service,
        runtime: grant.runtime,
        grant: grant.id,
        scope: grant.scope,
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
    pub(super) async fn read(&self, task: Uuid, revision: i64) -> HostResult {
        let input = json!({"subject": {"id": task.to_string(), "revision": revision}});
        self.run(&format!("read-{}", Uuid::new_v4()), "work.read", input)
            .await
    }
    pub(super) async fn route(&self, task: Uuid, revision: i64, change: Value) -> HostResult {
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
    pub(super) async fn emission(
        &self,
        task: Uuid,
    ) -> Option<(i64, Option<String>, Option<Value>)> {
        sqlx::query_as(
            "SELECT task_version,outcome,reasons FROM route_needed_emissions WHERE task_id=$1",
        )
        .bind(task)
        .fetch_optional(&self.f.db.pool().await)
        .await
        .unwrap()
    }
}
