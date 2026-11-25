use super::*;
/// An attempt row as admission leaves it, without the launch records around it:
/// the swarm read needs only its status, stop state, grant and manifest names.
struct Holder {
    node: Uuid,
    project: Uuid,
    status: &'static str,
    stop: &'static str,
    held: bool,
}
async fn hold(c: &mut sqlx::PgConnection, h: Holder) -> (Uuid, Uuid) {
    let now = common::profiled::NOW;
    let (attempt, task) = (Uuid::new_v4(), Uuid::new_v4());
    let grant = (h.status == "granted").then(|| json!({"expires_at_ms": now + 5_000}));
    let state = json!({"schema_version":1,"status":h.status,"stop":{"kind":h.stop},"revoked":false,
        "cancellation_id":null,"grant":grant,
        "offer":{"manifest":{"model":{"provider":"anthropic","model":"claude-sonnet-5"},
            "harness":{"harness":"claude-code"},"bindings":{"project_id":h.project,"task_revision":3}}}});
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state,holds_task,execution_deadline_ms) VALUES($1,$2,$3,$4,$5,'profiled_v1',$6,$7,$8)")
        .bind(attempt).bind(task).bind(h.node).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .bind(state).bind(h.held).bind(grant.is_some().then_some((now + 60_000) as i64))
        .execute(&mut *c).await.unwrap();
    (attempt, task)
}
/// The worker's started receipt, as the receipt table stores it.
async fn started(c: &mut sqlx::PgConnection, attempt: Uuid, node: Uuid, at_ms: u64) {
    let body = serde_json::to_vec(&json!({"recorded_at_ms": at_ms})).unwrap();
    sqlx::query("INSERT INTO profiled_started_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,receipt_bytes,before_record_revision) VALUES($1,$2,$3,$4,encode(sha256($4),'hex'),$4,1)")
        .bind(attempt).bind(node).bind(Uuid::new_v4()).bind(body)
        .execute(&mut *c).await.unwrap();
}
fn summary(role: &str) -> aq_role_settings::attempts::RanWith {
    aq_role_settings::attempts::RanWith {
        role: role.into(),
        level: "standard-high".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-5".into(),
        reasoning: "high".into(),
        harness: "claude-code".into(),
        execution_profile: "sandbox".into(),
        snapshot_digest: None,
    }
}

pub(super) struct SlotPopulation {
    first: Uuid,
    second: Uuid,
    project: Uuid,
    real: Uuid,
    granted: Uuid,
    preparing: Uuid,
    running: Uuid,
    running_task: Uuid,
    stopped: Uuid,
    elsewhere: Uuid,
    settled: Uuid,
    runtime: Uuid,
}
struct SwarmView<'a> {
    body: &'a Value,
}
impl SwarmView<'_> {
    fn node(&self, id: Uuid) -> &Value {
        self.body["data"]["machines"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["nodes"].as_array().unwrap())
            .find(|n| n["node"]["id"] == json!(id))
            .unwrap()
    }
    fn agent(&self, node: Uuid, attempt: Uuid) -> &Value {
        self.node(node)["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["attempt_id"] == json!(attempt))
            .unwrap_or_else(|| panic!("{attempt} missing: {}", self.body))
    }
}
fn at(ms: u64) -> Value {
    json!(aq_swarm::timestamp(ms).unwrap())
}
impl SlotPopulation {
    pub(super) async fn new(f: &mut common::profiled::Fixture) -> (Self, sqlx::PgConnection) {
        let now = common::profiled::NOW;
        make_running(f);
        // One real launch: offered nine seconds ago, started by the fixture's grant.
        let real = common::profiled::insert_offered(f, now - 9_000).await;
        let pool = f.db.pool().await;
        let (first, project) = (f.node.node_id, f.selection.lineage.project_id);
        let second = add_node(&pool, "second-machine", Some(now)).await;
        mark_seen(&pool, first, now).await;
        let mut report = windows_report();
        report.machine.key = Some("second-box".into());
        report.machine.label = Some("second-box".into());
        aq_swarm::machine::record(&pool, second, &report, now)
            .await
            .unwrap();
        // The receipt and terminal tables hang off launch records these rows do not have.
        let mut c = pool.acquire().await.unwrap().detach();
        sqlx::query("SET session_replication_role=replica")
            .execute(&mut c)
            .await
            .unwrap();
        let slots = Self::populate(&mut c, [first, second], project, real).await;
        (slots, c)
    }
    async fn populate(
        c: &mut sqlx::PgConnection,
        nodes: [Uuid; 2],
        project: Uuid,
        real: Uuid,
    ) -> Self {
        let [first, second] = nodes;
        let now = common::profiled::NOW;
        let row = |node, status, stop, held| Holder {
            node,
            project,
            status,
            stop,
            held,
        };
        let (preparing, preparing_task) = hold(c, row(first, "preparing", "unknown", true)).await;
        let (granted, _) = hold(c, row(first, "granted", "unknown", true)).await;
        let (running, running_task) = hold(c, row(first, "granted", "running", true)).await;
        let (stopped, _) = hold(c, row(first, "finished", "stopped", true)).await;
        let (elsewhere, _) = hold(c, row(second, "granted", "running", true)).await;
        let (settled, _) = hold(c, row(first, "finished", "stopped", false)).await;
        for (attempt, node, at) in [
            (running, first, now - 4_000),
            (stopped, first, now - 8_000),
            (elsewhere, second, now - 2_000),
        ] {
            started(c, attempt, node, at).await;
        }
        let runtime = Uuid::new_v4();
        sqlx::query("INSERT INTO terminal_sessions(terminal_id,attempt_id,node_id,boot_id,session_id,assignment_generation,terminal_epoch,identity,authority_revision,execution_spec_digest,launch_manifest_digest,input_mode,ledger_revision,runtime_id,protection_version,key_id,nonce,ciphertext,controller_generation,pending_bytes,operation_count,observed_at_ms,last_output_at_ms) SELECT $1,attempt_id,node_id,$1,session_id,1,2,'{}',1,repeat('a',64),repeat('a',64),'finite_v1',0,$3,1,'k',decode(repeat('00',12),'hex'),decode(repeat('00',16),'hex'),0,0,0,$4,$5 FROM execution_attempts WHERE attempt_id=$2")
        .bind(Uuid::new_v4()).bind(running).bind(runtime).bind(now as i64).bind((now - 1_000) as i64)
        .execute(&mut *c).await.unwrap();
        for (attempt, role) in [(real, "developer"), (running, "reviewer")] {
            aq_role_settings::attempts::record(c, attempt, &summary(role))
                .await
                .unwrap();
        }
        // Titles come from the graph; this fixture has none, so two are already known.
        aq_swarm::agents::remember(running_task, Some("Fix the flaky checkout test".into()));
        aq_swarm::agents::remember(preparing_task, Some("Add retry backoff".into()));

        Self {
            first,
            second,
            project,
            real,
            granted,
            preparing,
            running,
            running_task,
            stopped,
            elsewhere,
            settled,
            runtime,
        }
    }
    pub(super) fn assert_dashboard(&self, body: &Value) {
        let view = SwarmView { body };
        self.assert_phases(&view);
        self.assert_tasks(&view);
        self.assert_times(&view);
        self.assert_profiles(&view);
    }
    fn assert_phases(&self, view: &SwarmView<'_>) {
        for (node, expected) in [(self.first, 5), (self.second, 1)] {
            let row = view.node(node);
            assert_eq!(row["slots"]["occupied"], expected, "{row}");
            assert_eq!(row["agents"].as_array().unwrap().len(), expected, "{row}");
        }
        assert!(
            listed_agents(view.body)
                .iter()
                .all(|a| a["attempt_id"] != json!(self.settled))
        );
        let phases = [
            (self.preparing, "starting"),
            (self.granted, "starting"),
            (self.running, "running"),
            (self.real, "running"),
            (self.stopped, "finishing"),
        ];
        for (attempt, phase) in phases {
            assert_eq!(view.agent(self.first, attempt)["phase"], phase, "{attempt}");
        }
        assert_eq!(view.agent(self.second, self.elsewhere)["phase"], "running");
        assert_eq!(
            view.body["data"]["server"]["running_agents"], 3,
            "{}",
            view.body
        );
    }
    fn assert_tasks(&self, view: &SwarmView<'_>) {
        // Titles, falling back to the task id; project id and name; ids.
        let a = view.agent(self.first, self.running);
        assert_eq!(a["task"]["title"], "Fix the flaky checkout test");
        assert_eq!(a["task"]["id"], json!(self.running_task));
        assert_eq!(
            a["project"],
            json!({"id": self.project, "label": "Synthetic authority fixture"})
        );
        assert_eq!(
            view.agent(self.first, self.preparing)["task"]["title"],
            "Add retry backoff"
        );
        let untitled = view.agent(self.first, self.granted);
        assert_eq!(untitled["task"]["title"], untitled["task"]["id"]);
    }
    fn assert_times(&self, view: &SwarmView<'_>) {
        let now = common::profiled::NOW;
        let a = view.agent(self.first, self.running);
        // Since-times: the agent's start once it started, else when the slot was taken.
        assert_eq!(a["started_at"], at(now - 4_000));
        assert_eq!(
            view.agent(self.first, self.stopped)["started_at"],
            at(now - 8_000)
        );
        assert_eq!(
            view.agent(self.second, self.elsewhere)["started_at"],
            at(now - 2_000)
        );
        let launched = view.agent(self.first, self.real);
        assert_eq!(launched["held_since"], at(now - 9_000));
        assert_eq!(
            launched["started_at"],
            at(now - 9_000),
            "no start receipt yet"
        );
        assert!(view.agent(self.first, self.preparing)["started_at"].is_null());
    }
    fn assert_profiles(&self, view: &SwarmView<'_>) {
        let a = view.agent(self.first, self.running);
        let launched = view.agent(self.first, self.real);
        // `ran_with` where the attempt has a summary, null where it has none.
        assert_eq!(
            a["ran_with"],
            serde_json::to_value(summary("reviewer")).unwrap()
        );
        assert_eq!(launched["ran_with"]["role"], "developer");
        for attempt in [self.preparing, self.granted, self.stopped] {
            let row = view.agent(self.first, attempt);
            assert!(
                row["ran_with"].is_null() && row.get("ran_with").is_some(),
                "{row}"
            );
        }
        // This server has no terminal runtime, so it shows no terminal or output time.
        assert!(a["last_output_at"].is_null() && a["terminal"].is_null());
    }
    pub(super) async fn assert_batch(&self, c: &mut sqlx::PgConnection) {
        let now = common::profiled::NOW;
        // The batch read itself: one call for both nodes, whatever the attempt count,
        // and the output time of a terminal this runtime registered.
        let ids = [self.first, self.second];
        let query = aq_swarm::agents::Query {
            nodes: &ids,
            project: None,
            runtime: Some(self.runtime),
            summaries: true,
            now_ms: now,
            limit: aq_swarm::assemble::MAX_CANDIDATES,
        };
        let held = aq_swarm::agents::load(c, query).await.unwrap();
        assert_eq!((held.agents.len(), held.running, held.cut), (6, 3, false));
        let (node, row) = (held.agents.iter())
            .find(|(_, a)| a.attempt_id == self.running)
            .unwrap();
        assert_eq!(*node, self.first);
        assert_eq!(json!(row.last_output_at), at(now - 1_000));
        assert_eq!(json!(row.terminal.as_ref().unwrap().revision), json!("2"));
        assert!(
            held.agents
                .iter()
                .filter(|(_, a)| a.attempt_id != self.running)
                .all(|(_, a)| a.last_output_at.is_none())
        );
        // A project scope lists only that project's attempts.
        let other = aq_swarm::agents::Query {
            project: Some(Uuid::new_v4()),
            runtime: None,
            nodes: &ids,
            summaries: true,
            now_ms: now,
            limit: 8,
        };
        assert!(
            aq_swarm::agents::load(c, other)
                .await
                .unwrap()
                .agents
                .is_empty()
        );
    }
}
