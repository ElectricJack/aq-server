//! Spec 0017 §6.3-6.4: host catalog 2.5.0 (profile 12) and its environment
//! commands, `environment.build` and `environment.admit`.
use aq_playbook_host::model::digest;
use aq_server::playbooks::{domain::GrantScope, review_domain::*};
use serde_json::Value;
use std::collections::BTreeSet;
use uuid::Uuid;

#[test]
fn host12_is_catalog_2_5_0_and_serves_the_delivery_record() {
    let registry = CatalogProfile::Host12.registry();
    let manifest: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-host-catalog-2.5.0/manifest.json"
    ))
    .unwrap();
    assert_eq!(manifest["version"], HOST12_VERSION);
    assert_eq!(manifest["fingerprint"], HOST12_FINGERPRINT);
    assert_eq!(manifest["supersedes"]["fingerprint"], HOST10_FINGERPRINT);
    assert_eq!(manifest["includes"]["fingerprint"], HOST11_FINGERPRINT);
    assert_eq!(
        digest(&(HOST12_VERSION, &registry)).unwrap(),
        HOST12_FINGERPRINT
    );
    let host12 = CatalogProfile::Host12;
    assert_eq!((host12.number(), host12.config_name()), (12, "host_v12"));
    assert_eq!(CatalogProfile::from_number(12), Some(host12));
    assert!(host12.is_delivery() && host12.has_routing() && host12.has_notes());
    assert!(host12.has_environments() && !CatalogProfile::Host10.has_environments());
    // A namespace of another profile is not reused.
    assert!(host12.namespace_refusal(1, Some((10, 1))).is_some());
    // It serves Host10's commands, work.activate and the environment commands.
    let served: BTreeSet<&str> = host12.commands().iter().copied().collect();
    let mut expected: BTreeSet<&str> = HOST10_COMMANDS.into_iter().collect();
    expected.extend([WORK_ACTIVATE]);
    expected.extend(ENVIRONMENT_COMMANDS);
    assert_eq!(served, expected);
    for name in ENVIRONMENT_COMMANDS {
        let contract = &registry.commands[name];
        let asynchronous = contract.asynchronous.as_ref().unwrap();
        assert_eq!(asynchronous.completion_event, "aq.environment.completed");
        assert_eq!(contract.capabilities, BTreeSet::from([name.to_string()]));
    }
    assert!(registry.events.contains_key("aq.environment.base_changed"));
    // A grant may name them; a Host10 one may not.
    let scope = GrantScope {
        workspace_id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        recipients: vec![],
        capabilities: HOST10_CAPABILITIES
            .iter()
            .chain(&HOST10_DELIVERY_CAPABILITIES)
            .chain(&ENVIRONMENT_COMMANDS)
            .chain(&[WORK_ACTIVATE])
            .map(|c| c.to_string())
            .collect(),
    };
    assert!(host12.validate_scope(&scope).is_ok());
    assert!(CatalogProfile::Host10.validate_scope(&scope).is_err());
    // Every contract the built-in delivery record is authored against is
    // accepted, its 2.9.0 work.read included; 2.2.0's and 2.3.0's are not.
    let record = aq_playbook_authoring::builtin::delivery(true);
    let pinned = aq_playbook_authoring::request::pinned_catalog(&record).unwrap();
    let kept = aq_playbook_authoring::request::catalog_for(&record, &pinned).unwrap();
    assert_eq!(kept.commands["work.read"].version, "2.9.0");
    for contract in kept.commands.values() {
        assert!(
            aq_playbook_host::accepts_command(&registry, contract),
            "{}",
            contract.name
        );
    }
    for older in [CatalogProfile::Host9, CatalogProfile::Host10] {
        assert!(!aq_playbook_host::accepts_command(
            &registry,
            &older.registry().commands["work.read"]
        ));
    }
    // The pin lists the vendored files byte for byte.
    let pin: Value = serde_json::from_str(include_str!(
        "../../../../vendor/aq-host-catalog-2.5.0-source.json"
    ))
    .unwrap();
    assert_eq!(pin["catalog_fingerprint"], HOST12_FINGERPRINT);
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/aq-host-catalog-2.5.0");
    let mut on_disk = vec![];
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                on_disk.push(path);
            }
        }
    }
    let files = pin["files"].as_array().unwrap();
    assert_eq!(files.len(), on_disk.len());
    for file in files {
        let bytes = std::fs::read(root.join(file["path"].as_str().unwrap())).unwrap();
        assert_eq!(file["bytes"], bytes.len());
        assert_eq!(
            file["sha256"].as_str().unwrap(),
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes))
        );
    }
}

// ----------------------------------------------------------------------
// The environment commands, on a Host12 namespace. The project's recipes on
// aq-git and its binding stand behind a scripted port; the builder's job
// results are recorded as the complete route records them.
mod commands {
    use super::*;
    use crate::common;
    use aq_environments::commands::{self as environments, Bound, EnvironmentOps};
    use aq_playbook_host::runner::{Intent, IntentStatus};
    use aq_server::playbooks::{
        domain::{
            Error, Result,
            delivery_store::{DeliveryPort, Latest, Pending},
        },
        host_adapters::{HostResult, WorkGraph},
        service::*,
    };
    use aq_server::protected_storage::{KeyFile, Keyring, ProtectionConfig};
    use serde_json::json;
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Arc, sync::Mutex};

    /// Distinct hex per letter: `sha256:<64 hex>`, and a 40-hex git id.
    fn hex(c: char) -> String {
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(
            c.to_string().as_bytes(),
        ))
    }
    fn digest(c: char) -> String {
        format!("sha256:{}", hex(c))
    }
    fn oid(c: char) -> String {
        hex(c)[..40].to_owned()
    }
    fn base(c: char) -> environments::Base {
        environments::Base {
            id: "aq-base".into(),
            image: format!("registry.example/aq-base@{}", digest(c)),
            harnesses: vec![json!({"harness": "claude", "version": "2.1.0"})],
        }
    }
    /// A Build as the builder reports it.
    fn build(name: &str, d: char, default: bool) -> Value {
        json!({"name": name, "digest": digest(d), "image": format!("registry.example/acme@{}", digest(d)),
            "recipe_sha256": "5".repeat(64), "provides": ["node", "pnpm"],
            "toolchain": [{"command": "node --version", "output": "v24.1.0\nnpm 11"}],
            "warm": ["pnpm install --frozen-lockfile"], "harnesses": [{"harness": "claude", "version": "2.1.0"}],
            "default": default})
    }

    /// A binding change: the project, the environments, the default.
    type Bind = (Uuid, Vec<String>, Option<String>);
    /// The recipes at each commit, and every binding change asked for.
    #[derive(Default)]
    struct Ops {
        trees: Mutex<BTreeMap<String, Vec<(String, String)>>>,
        binds: Mutex<Vec<Bind>>,
        /// How the binding answers: 0 done, 1 retry later, 2 refused.
        answer: std::sync::atomic::AtomicU8,
    }
    #[async_trait::async_trait]
    impl EnvironmentOps for Ops {
        async fn recipes(&self, _: Uuid, commit: &str) -> Option<Vec<(String, String)>> {
            self.trees.lock().unwrap().get(commit).cloned()
        }
        async fn bind(
            &self,
            _: Uuid,
            project: Uuid,
            environments: &[String],
            default: Option<&str>,
            _: Uuid,
        ) -> Bound {
            let call = (project, environments.to_vec(), default.map(Into::into));
            self.binds.lock().unwrap().push(call);
            match self.answer.load(std::sync::atomic::Ordering::SeqCst) {
                0 => Bound::Done,
                1 => Bound::Retry,
                _ => Bound::Refused,
            }
        }
    }
    #[derive(Default)]
    struct Port(Mutex<BTreeMap<Uuid, Latest>>);
    #[async_trait::async_trait]
    impl DeliveryPort for Port {
        async fn pending(&self, _: Option<Uuid>, _: u32) -> Result<Vec<Pending>> {
            Ok(vec![])
        }
        async fn latest(&self, task: Uuid) -> Result<Option<Latest>> {
            Ok(self.0.lock().unwrap().get(&task).cloned())
        }
        async fn bound(&self, _: Uuid) -> Result<bool> {
            Ok(true)
        }
        async fn has_result(&self, _: Uuid, _: Uuid) -> Result<bool> {
            Ok(false)
        }
    }
    struct NoGraph;
    #[async_trait::async_trait]
    impl WorkGraph for NoGraph {
        async fn node(&self, _: Uuid, _: Uuid) -> Result<Option<Value>> {
            Ok(None)
        }
        async fn mutate(&self, _: Uuid, _: Value) -> Result<(u16, Value)> {
            Err(Error::Unknown)
        }
    }

    struct H {
        f: common::profiled::Fixture,
        service: Service,
        runtime: RuntimeOwner,
        grant: Uuid,
        scope: GrantScope,
        ops: Arc<Ops>,
        port: Arc<Port>,
        project: Uuid,
        pool: sqlx::PgPool,
    }
    /// A Host12 namespace for project `acme` (bound, with the AQ base `1`).
    async fn harness(installation_routing: bool) -> H {
        let f = common::profiled::fixture().await;
        let pool = f.db.pool().await;
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
        let (ops, port, project) = (
            Arc::new(Ops::default()),
            Arc::new(Port::default()),
            Uuid::new_v4(),
        );
        let workspace = f.selection.lineage.workspace_id;
        let namespace = Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: format!("host12-{}", Uuid::new_v4()),
            workspace_id: workspace,
            project_id: project,
            owner_user_id: f.actor,
        };
        let mut service = Service::new_host2(
            &f.state,
            namespace,
            Arc::new(keys),
            CatalogProfile::Host12,
            Arc::new(NoGraph),
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_delivery(port.clone())
        .with_environment_ops(ops.clone());
        service.installation_routing = installation_routing;
        bind(&pool, project, "acme").await;
        sqlx::query(
            "UPDATE project_execution_bindings SET binding=binding||$2 WHERE project_id=$1",
        )
        .bind(project)
        .bind(json!({"enabled": true, "graph_workspace_id": workspace}))
        .execute(&pool)
        .await
        .unwrap();
        service.initialize(f.actor).await.unwrap();
        let scope = GrantScope {
            workspace_id: workspace,
            project_id: project,
            recipients: vec![],
            capabilities: ENVIRONMENT_COMMANDS
                .iter()
                .chain(&["operation.get"])
                .map(|c| c.to_string())
                .collect(),
        };
        let grant = Uuid::new_v4();
        let now = (f.state.clock)().timestamp_millis();
        service
            .grant(f.actor, grant, scope.clone(), now + 86_400_000)
            .await
            .unwrap();
        let runtime = service.adopt_runtime(Uuid::new_v4(), 0).await.unwrap();
        assert!(environments::record_base(&pool, &base('1')).await.unwrap());
        H {
            f,
            service,
            runtime,
            grant,
            scope,
            ops,
            port,
            project,
            pool,
        }
    }
    async fn bind(pool: &sqlx::PgPool, project: Uuid, name: &str) {
        sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')")
            .bind(project)
            .bind(json!({"name": name, "repository_id": project}))
            .execute(pool)
            .await
            .unwrap();
    }

    impl H {
        fn clock(&self) -> i64 {
            (self.f.state.clock)().timestamp_millis()
        }
        /// The recipes aq-git lists at `commit`.
        fn tree(&self, commit: &str, recipes: &[(&str, char)]) {
            let recipes = recipes
                .iter()
                .map(|(n, c)| (n.to_string(), oid(*c)))
                .collect();
            self.ops
                .trees
                .lock()
                .unwrap()
                .insert(commit.into(), recipes);
        }
        /// A task of the project whose latest result is at `head`.
        fn task(&self, head: char) -> Uuid {
            let task = Uuid::new_v4();
            let latest = Latest {
                workspace_id: self.scope.workspace_id,
                project_id: self.project,
                attempt_id: Uuid::new_v4(),
                result_id: Uuid::new_v4(),
                outcome: "success".into(),
                changed: true,
                head: oid(head),
                base: oid('0'),
                conclusion: String::new(),
            };
            self.port.0.lock().unwrap().insert(task, latest);
            task
        }
        fn subject(task: Uuid) -> Value {
            json!({"id": task.to_string(), "revision": 1})
        }
        async fn run(&self, name: &str, input: Value) -> HostResult {
            let result = self.try_run(name, input.clone()).await.unwrap();
            let registry = CatalogProfile::Host12.registry();
            registry.commands[name].outcomes[&result.outcome]
                .validate(&result.output)
                .unwrap();
            result
        }
        async fn try_run(&self, name: &str, input: Value) -> Result<HostResult> {
            self.try_run_as(&format!("{name}-{}", Uuid::new_v4()), name, input)
                .await
        }
        async fn try_run_as(&self, id: &str, name: &str, input: Value) -> Result<HostResult> {
            let registry = CatalogProfile::Host12.registry();
            let intent = Intent {
                id: id.into(),
                run: "synthetic-run".into(),
                step: name.replace('.', "-"),
                invocation: 1,
                deadline_at_ms: self.clock() + 5000,
                command: registry.commands[name].clone(),
                input_digest: digest_of(&input),
                input,
                authority: CatalogProfile::Host12
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
            };
            self.service
                .host_dispatch(&self.runtime, self.grant, 1, &intent)
                .await
        }
        async fn accepted(&self, name: &str, input: Value) -> String {
            let result = self.run(name, input).await;
            assert_eq!(result.outcome, "accepted", "{name}: {}", result.output);
            result.output["handle"].as_str().unwrap().to_owned()
        }
        async fn pass(&self) -> usize {
            self.service
                .collect_delivery_operations(&self.runtime, 4)
                .await
                .unwrap()
        }
        /// The completion of the operation `handle`, if it completed.
        async fn completion(&self, handle: &str) -> Option<Value> {
            let page = self.service.outbox(&self.runtime, 0, 16).await.unwrap();
            let mut found = page.into_iter().filter(|e| {
                e.2.contract.name == "aq.environment.completed"
                    && e.2.correlation["operation"] == handle
            });
            let first = found.next().map(|e| e.2.payload);
            assert!(found.next().is_none(), "one completion per operation");
            first
        }
        async fn jobs(&self) -> Vec<(Uuid, Value, String, String, String)> {
            sqlx::query_as("SELECT job_id,recipes,commit,base_template,state FROM environment_jobs WHERE kind='build' ORDER BY created_at")
                .fetch_all(&self.pool)
                .await
                .unwrap()
        }
        /// The builder claims `job` and reports `result` (its job id added).
        async fn report(&self, job: Uuid, mut result: Value) {
            let node = Uuid::new_v4();
            sqlx::query("UPDATE environment_jobs SET state='claimed',claimed_by=$2,claims=1,claimed_at=now(),lease_until=now()+interval '1 hour' WHERE job_id=$1")
                .bind(job)
                .bind(node)
                .execute(&self.pool)
                .await
                .unwrap();
            result["job_id"] = json!(job);
            let body = serde_json::to_vec(&result).unwrap();
            aq_environments::worker::record(&self.pool, node, &body)
                .await
                .unwrap();
        }
        /// `environment.build` of `input`, built by the builder as `builds`:
        /// its completion.
        async fn built(&self, input: Value, builds: Value) -> Value {
            let handle = self.accepted("environment.build", input).await;
            let job = self.jobs().await.last().unwrap().0;
            let result =
                json!({"outcome": "built", "builds": builds, "failure": null, "run": null});
            self.report(job, result).await;
            assert_eq!(self.pass().await, 1);
            self.completion(&handle).await.unwrap()
        }
        /// `environment.admit` of `input`: its completion.
        async fn admit(&self, input: Value) -> Value {
            let handle = self.accepted("environment.admit", input).await;
            assert_eq!(self.pass().await, 1);
            self.completion(&handle).await.unwrap()
        }
        async fn merged(&self, task: Uuid, at_ms: i64) {
            sqlx::query("INSERT INTO delivery_merge_evidence(operation_id,project_id,task_id,pull_request,target,target_ref,expected_head_sha,merge_sha,observed_at_ms) VALUES($1,$2,$3,7,'{}','refs/heads/main',$4,$5,$6)")
                .bind(Uuid::new_v4())
                .bind(self.project)
                .bind(task)
                .bind(oid('a'))
                .bind(oid('d'))
                .bind(at_ms)
                .execute(&self.pool)
                .await
                .unwrap();
        }
        /// An admitted version of `acme.<name>`, as an earlier admit wrote it.
        async fn version(&self, name: &str, version: &str, d: char, blob: char, base: char) {
            sqlx::query("INSERT INTO environment_versions(environment_id,version,project_id,template_digest,image,provides,recipe_blob_id,base_digest,merged_at_ms,admitted_at) VALUES($1,$2,$3,$4,'registry.example/acme@'||$4,'[\"node\"]',$5,$6,1,now()-interval '1 hour'+($2||' second')::interval)")
                .bind(format!("acme.{name}"))
                .bind(version)
                .bind(self.project)
                .bind(digest(d))
                .bind(oid(blob))
                .bind(digest(base))
                .execute(&self.pool)
                .await
                .unwrap();
        }
        /// `(version, digest, recipe blob, merged at, base)` of `id`, oldest first.
        async fn versions(
            &self,
            id: &str,
        ) -> Vec<(String, String, Option<String>, Option<i64>, Option<String>)> {
            sqlx::query_as("SELECT version,template_digest,recipe_blob_id,merged_at_ms,base_digest FROM environment_versions WHERE environment_id=$1 ORDER BY admitted_at,version")
                .bind(id)
                .fetch_all(&self.pool)
                .await
                .unwrap()
        }
        async fn builds_kept(&self) -> i64 {
            sqlx::query_scalar("SELECT count(*) FROM environment_builds WHERE project_id=$1")
                .bind(self.project)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }
    }
    fn digest_of(value: &Value) -> String {
        aq_playbook_host::model::digest(value).unwrap()
    }

    #[tokio::test]
    async fn build_with_no_changed_recipe_completes_unchanged_without_a_job() {
        let h = harness(false).await;
        h.version("web", "1", 'w', 'b', '1').await;
        let task = h.task('c');
        h.tree(&oid('c'), &[("web", 'b')]);
        let handle = h
            .accepted("environment.build", json!({"subject": H::subject(task)}))
            .await;
        assert!(
            h.completion(&handle).await.is_none(),
            "nothing is done at dispatch"
        );
        assert_eq!(h.pass().await, 1);
        assert_eq!(
            h.completion(&handle).await.unwrap(),
            json!({"outcome": "unchanged", "output": {}})
        );
        assert!(h.jobs().await.is_empty(), "no builder job");
        // A repository without recipes, and no admitted one, is unchanged too.
        let other = h.task('e');
        h.tree(&oid('e'), &[]);
        sqlx::query("DELETE FROM environment_versions WHERE project_id=$1")
            .bind(h.project)
            .execute(&h.pool)
            .await
            .unwrap();
        let handle = h
            .accepted("environment.build", json!({"subject": H::subject(other)}))
            .await;
        assert_eq!(h.pass().await, 1);
        assert_eq!(h.completion(&handle).await.unwrap()["outcome"], "unchanged");
        assert!(h.jobs().await.is_empty());
        // Exactly one of subject and project; an unknown task fails at once.
        let both = json!({"subject": H::subject(task), "project": h.project.to_string()});
        assert_eq!(
            h.try_run("environment.build", both).await.unwrap_err(),
            Error::Invalid
        );
        let unknown = h
            .accepted(
                "environment.build",
                json!({"subject": H::subject(Uuid::new_v4())}),
            )
            .await;
        assert_eq!(h.pass().await, 1);
        let failed = h.completion(&unknown).await.unwrap();
        assert_eq!(
            (&failed["outcome"], &failed["output"]["step"]),
            (&json!("failed"), &json!("subject"))
        );
    }

    #[tokio::test]
    async fn build_queues_one_job_for_the_changed_recipes_and_completes_built() {
        let h = harness(false).await;
        h.version("web", "1", 'w', 'b', '1').await;
        h.version("api", "1", 'x', 'c', '1').await;
        let task = h.task('c');
        // web changed, api did not, docs is new.
        h.tree(&oid('c'), &[("web", 'e'), ("api", 'c'), ("docs", 'f')]);
        let handle = h
            .accepted("environment.build", json!({"subject": H::subject(task)}))
            .await;
        let jobs = h.jobs().await;
        assert_eq!(jobs.len(), 1);
        let (job, recipes, commit, base_template, state) = &jobs[0];
        assert_eq!(recipes, &json!(["docs", "web"]));
        assert_eq!(
            (commit.as_str(), state.as_str()),
            (oid('c').as_str(), "queued")
        );
        assert_eq!(base_template, &base('1').image);
        // Nothing completes while the job runs.
        assert_eq!(h.pass().await, 0);
        assert!(h.completion(&handle).await.is_none());
        let builds = json!([build("docs", 'd', false), build("web", 'e', true)]);
        h.report(
            *job,
            json!({"outcome": "built", "builds": builds, "failure": null, "run": null}),
        )
        .await;
        assert_eq!(h.pass().await, 1);
        let toolchains = json!([{"name": "node --version", "version": "v24.1.0"}]);
        assert_eq!(
            h.completion(&handle).await.unwrap(),
            json!({"outcome": "built", "output": {"builds": [
                {"name": "docs", "digest": digest('d'), "recipe_hash": "5".repeat(64), "toolchains": toolchains},
                {"name": "web", "digest": digest('e'), "recipe_hash": "5".repeat(64), "toolchains": toolchains},
            ]}})
        );
        // The builds wait for their admit, whatever happens to the job's row.
        assert_eq!(h.builds_kept().await, 2);
        assert_eq!(h.pass().await, 0, "a completed operation is not run again");
    }

    #[tokio::test]
    async fn a_failed_build_completes_failed_with_the_step_and_redacted_log() {
        let h = harness(false).await;
        let task = h.task('c');
        h.tree(&oid('c'), &[("web", 'e')]);
        let handle = h
            .accepted("environment.build", json!({"subject": H::subject(task)}))
            .await;
        let job = h.jobs().await[0].0;
        let failure = json!({"recipe": "web", "step": "check", "log": "$ pnpm --version\n(exit 127)\ntoken=[REDACTED]\n"});
        h.report(
            job,
            json!({"outcome": "failed", "builds": [], "failure": failure, "run": null}),
        )
        .await;
        assert_eq!(h.pass().await, 1);
        assert_eq!(
            h.completion(&handle).await.unwrap(),
            json!({"outcome": "failed", "output": failure})
        );
        assert_eq!(h.builds_kept().await, 0);
        // A Build without `default` breaks the Build contract: failed at parse.
        let again = h.task('d');
        h.tree(&oid('d'), &[("web", 'f')]);
        let handle = h
            .accepted("environment.build", json!({"subject": H::subject(again)}))
            .await;
        let job = h.jobs().await.last().unwrap().0;
        let mut old = build("web", 'f', false);
        old.as_object_mut().unwrap().remove("default");
        h.report(
            job,
            json!({"outcome": "built", "builds": [old], "failure": null, "run": null}),
        )
        .await;
        assert_eq!(h.pass().await, 1);
        let completion = h.completion(&handle).await.unwrap();
        assert_eq!(completion["outcome"], "failed");
        assert_eq!(
            (
                &completion["output"]["recipe"],
                &completion["output"]["step"]
            ),
            (&json!("web"), &json!("parse"))
        );
        assert_eq!(h.builds_kept().await, 0);
    }

    #[tokio::test]
    async fn admit_records_versions_binds_the_project_and_sets_the_default() {
        let h = harness(false).await;
        let task = h.task('c');
        h.tree(&oid('c'), &[("web", 'e'), ("api", 'f')]);
        let builds = json!([build("api", 'a', false), build("web", 'w', true)]);
        let built = h.built(json!({"subject": H::subject(task)}), builds).await;
        let summary = built["output"]["builds"].clone();
        // Not merged yet: refused.
        let unmerged = h
            .admit(json!({"subject": H::subject(task), "builds": summary}))
            .await;
        assert_eq!(
            unmerged,
            json!({"outcome": "refused", "output": {"reason": "unknown_task", "step": "merge"}})
        );
        h.merged(task, 1_000).await;
        // A digest the builder never built is refused.
        let mut forged = summary.clone();
        forged[0]["digest"] = json!(digest('9'));
        let refused = h
            .admit(json!({"subject": H::subject(task), "builds": forged}))
            .await;
        assert_eq!(
            refused["output"],
            json!({"reason": "unknown_build", "step": "api"})
        );
        assert!(h.versions("acme.api").await.is_empty());
        let admitted = h
            .admit(json!({"subject": H::subject(task), "builds": summary}))
            .await;
        assert_eq!(admitted, json!({"outcome": "admitted", "output": {}}));
        let web = h.versions("acme.web").await;
        assert_eq!(
            web,
            [(
                "1".into(),
                digest('w'),
                Some(oid('e')),
                Some(1_000),
                Some(digest('1'))
            )]
        );
        assert_eq!(h.versions("acme.api").await[0].2, Some(oid('f')));
        let text: String = sqlx::query_scalar(
            "SELECT platform_text FROM environment_versions WHERE environment_id='acme.web'",
        )
        .fetch_one(&h.pool)
        .await
        .unwrap();
        assert!(
            text.contains("node, pnpm") && text.contains("v24.1.0"),
            "{text}"
        );
        assert_eq!(
            *h.ops.binds.lock().unwrap(),
            [(
                h.project,
                vec!["acme.api".to_string(), "acme.web".to_string()],
                Some("acme.web".to_string())
            )]
        );
        // Kept (rolling) so that a pass whose completion was lost answers the same.
        assert_eq!(h.builds_kept().await, 2);
        // Nodes are told of the new versions.
        let boot = aq_environments::BootEnvironments::new([], None);
        let catalog = aq_environments::Catalog::load(&h.pool, &boot)
            .await
            .unwrap();
        assert!(
            catalog
                .admitted_for_nodes()
                .iter()
                .any(|e| e.id == "acme.web" && e.digest == digest('w'))
        );
        assert_eq!(catalog.provides("acme.web"), ["node", "pnpm"]);
        // Two default recipes are refused.
        let next = h.task('d');
        h.tree(&oid('d'), &[("web", '1'), ("api", '2')]);
        let both = json!([build("api", 'b', true), build("web", 'x', true)]);
        let built = h.built(json!({"subject": H::subject(next)}), both).await;
        h.merged(next, 2_000).await;
        let input = json!({"subject": H::subject(next), "builds": built["output"]["builds"]});
        assert_eq!(
            h.admit(input).await["output"],
            json!({"reason": "unknown_build", "step": "default"})
        );
        assert_eq!(h.versions("acme.web").await.len(), 1);
    }

    #[tokio::test]
    async fn a_late_admit_never_replaces_a_newer_version() {
        let h = harness(false).await;
        let (first, second) = (h.task('c'), h.task('d'));
        h.tree(&oid('c'), &[("web", 'e')]);
        h.tree(&oid('d'), &[("web", 'f')]);
        let one = h
            .built(
                json!({"subject": H::subject(first)}),
                json!([build("web", 'a', true)]),
            )
            .await;
        let two = h
            .built(
                json!({"subject": H::subject(second)}),
                json!([build("web", 'b', true)]),
            )
            .await;
        // The first merges first, but its admit completes last.
        h.merged(first, 1_000).await;
        h.merged(second, 2_000).await;
        let input = |task, built: &Value| json!({"subject": H::subject(task), "builds": built["output"]["builds"]});
        assert_eq!(h.admit(input(second, &two)).await["outcome"], "admitted");
        assert_eq!(h.admit(input(first, &one)).await["outcome"], "admitted");
        let web = h.versions("acme.web").await;
        assert_eq!(web.len(), 1, "the earlier merge's build is not written");
        assert_eq!(
            (web[0].1.as_str(), web[0].3),
            (digest('b').as_str(), Some(2_000))
        );
        let boot = aq_environments::BootEnvironments::new([], None);
        let catalog = aq_environments::Catalog::load(&h.pool, &boot)
            .await
            .unwrap();
        assert_eq!(
            catalog.template("acme.web").unwrap().digest.as_str(),
            hex('b')
        );
        // Only the later merge set the default.
        let binds = h.ops.binds.lock().unwrap().clone();
        let defaults: Vec<_> = binds.iter().map(|(_, _, d)| d.clone()).collect();
        assert_eq!(defaults, [Some("acme.web".to_string()), None]);
    }

    #[tokio::test]
    async fn admitting_a_base_emits_base_changed_per_project() {
        let h = harness(true).await;
        let other = Uuid::new_v4();
        bind(&h.pool, other, "beta").await;
        h.version("web", "1", 'w', 'b', '1').await;
        sqlx::query("INSERT INTO environment_versions(environment_id,version,project_id,template_digest,image,base_digest) VALUES('beta.rust','1',$1,$2,'registry.example/beta@'||$2,$3)")
            .bind(other)
            .bind(digest('r'))
            .bind(digest('1'))
            .execute(&h.pool)
            .await
            .unwrap();
        let idle = Uuid::new_v4();
        bind(&h.pool, idle, "idle").await;
        // Every project is on the newest base: nothing to say.
        assert_eq!(
            h.service.collect_base_changes(&h.runtime, 8).await.unwrap(),
            0
        );
        // The boot config names a new base: it is recorded once.
        assert!(
            environments::record_base(&h.pool, &base('2'))
                .await
                .unwrap()
        );
        assert!(
            !environments::record_base(&h.pool, &base('2'))
                .await
                .unwrap()
        );
        assert_eq!(
            h.service.collect_base_changes(&h.runtime, 8).await.unwrap(),
            2
        );
        let page = h.service.outbox(&h.runtime, 0, 16).await.unwrap();
        let mut told: Vec<Value> = (page.into_iter())
            .filter(|e| e.2.contract.name == "aq.environment.base_changed")
            .map(|e| e.2.payload)
            .collect();
        told.sort_by_key(|p| p["project"].as_str().unwrap().to_owned());
        let mut expected = vec![
            json!({"project": h.project.to_string()}),
            json!({"project": other.to_string()}),
        ];
        expected.sort_by_key(|p| p["project"].as_str().unwrap().to_owned());
        assert_eq!(told, expected, "once per project with admitted recipes");
        assert_eq!(
            h.service.collect_base_changes(&h.runtime, 8).await.unwrap(),
            0,
            "and once only"
        );
    }

    #[tokio::test]
    async fn a_project_build_rebuilds_the_admitted_recipes_on_the_current_base() {
        let h = harness(false).await;
        h.version("web", "1", 'w', 'b', '1').await;
        h.version("api", "1", 'x', 'c', '1').await;
        assert!(
            environments::record_base(&h.pool, &base('2'))
                .await
                .unwrap()
        );
        // Main's head is not known: the rebuild fails at once, with no job.
        let project = json!({"project": h.project.to_string()});
        let handle = h.accepted("environment.build", project.clone()).await;
        assert_eq!(h.pass().await, 1);
        assert_eq!(
            h.completion(&handle).await.unwrap()["output"]["step"],
            "fetch"
        );
        sqlx::query(
            "INSERT INTO delivery_main_heads(project_id,head,observed_at_ms) VALUES($1,$2,1)",
        )
        .bind(h.project)
        .bind(oid('f'))
        .execute(&h.pool)
        .await
        .unwrap();
        h.tree(&oid('f'), &[("web", 'b'), ("api", 'c')]);
        let builds = json!([build("api", 'y', false), build("web", 'z', true)]);
        let built = h.built(project.clone(), builds).await;
        let (_, recipes, commit, base_template, _) = h.jobs().await.pop().unwrap();
        assert_eq!(recipes, json!(["api", "web"]));
        assert_eq!((commit, base_template), (oid('f'), base('2').image));
        assert_eq!(built["outcome"], "built");
        let input = json!({"project": h.project.to_string(), "builds": built["output"]["builds"]});
        assert_eq!(h.admit(input).await["outcome"], "admitted");
        let web = h.versions("acme.web").await;
        assert_eq!(web.len(), 2);
        assert_eq!(
            (web[1].1.as_str(), web[1].4.as_deref()),
            (digest('z').as_str(), Some(digest('2').as_str()))
        );
        assert_eq!(
            web[1].3, web[0].3,
            "a rebuild keeps its recipe's merge order"
        );
    }

    #[tokio::test]
    async fn admit_prunes_to_keep_versions_but_keeps_one_in_use() {
        let h = harness(false).await;
        let limits = json!({"max_install_steps": 64, "max_step_args": 32, "max_arg_bytes": 4096,
            "max_prefetch": 16, "max_recipe_bytes": 1048576, "build_wall_ms": 3600000,
            "build_cpu_millis": 4000, "build_memory_bytes": 8589934592u64, "log_bytes": 65536,
            "try_concurrency": 2, "keep_versions": 2, "max_job_claims": 3});
        sqlx::query("UPDATE role_settings SET environment_limits=$1 WHERE singleton")
            .bind(limits)
            .execute(&h.pool)
            .await
            .unwrap();
        for (version, d) in [("1", 'p'), ("2", 'q'), ("3", 'r')] {
            h.version("web", version, d, 'b', '1').await;
        }
        let task = h.task('c');
        h.tree(&oid('c'), &[("web", 'e')]);
        let built = h
            .built(
                json!({"subject": H::subject(task)}),
                json!([build("web", 's', true)]),
            )
            .await;
        h.merged(task, 5_000).await;
        let input = json!({"subject": H::subject(task), "builds": built["output"]["builds"]});
        // The fixture's attempt holds its task, running version 1 of
        // `acme.web` (its sealed manifest re-pointed for this test).
        let attempt = common::profiled::insert(&h.f).await;
        let guard =
            |on: &str| format!("ALTER TABLE profiled_launches {on} TRIGGER profiled_launch_guard");
        sqlx::query(&guard("DISABLE"))
            .execute(&h.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE profiled_launches SET launch_manifest=jsonb_set(jsonb_set(launch_manifest,'{harness,environment,id}','\"acme.web\"'),'{harness,environment,version}','\"1\"') WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&h.pool)
            .await
            .unwrap();
        sqlx::query(&guard("ENABLE"))
            .execute(&h.pool)
            .await
            .unwrap();
        assert_eq!(h.admit(input).await["outcome"], "admitted");
        let kept: Vec<String> = h
            .versions("acme.web")
            .await
            .into_iter()
            .map(|v| v.0)
            .collect();
        assert_eq!(
            kept,
            ["1", "3", "4"],
            "the newest keep_versions, and the one in use"
        );
    }

    async fn keep_versions(h: &H, keep: u64) {
        let limits = json!({"max_install_steps": 64, "max_step_args": 32, "max_arg_bytes": 4096,
            "max_prefetch": 16, "max_recipe_bytes": 1048576, "build_wall_ms": 3600000,
            "build_cpu_millis": 4000, "build_memory_bytes": 8589934592u64, "log_bytes": 65536,
            "try_concurrency": 2, "keep_versions": keep, "max_job_claims": 3});
        sqlx::query("UPDATE role_settings SET environment_limits=$1 WHERE singleton")
            .bind(limits)
            .execute(&h.pool)
            .await
            .unwrap();
    }
    /// A task whose head changes `web`, built as `d` and merged at `at_ms`:
    /// the admit's input.
    async fn merged_build(h: &H, head: char, blob: char, d: char, at_ms: i64) -> Value {
        let task = h.task(head);
        h.tree(&oid(head), &[("web", blob)]);
        let built = h
            .built(
                json!({"subject": H::subject(task)}),
                json!([build("web", d, true)]),
            )
            .await;
        h.merged(task, at_ms).await;
        json!({"subject": H::subject(task), "builds": built["output"]["builds"]})
    }

    /// Fix round 1: the admit step is keyed idempotent. A pass whose
    /// completion was lost after the versions and the binding were written
    /// answers `admitted` again.
    #[tokio::test]
    async fn an_admit_whose_completion_is_lost_replays_to_admitted() {
        let h = harness(false).await;
        let input = merged_build(&h, 'c', 'e', 'a', 1_000).await;
        let task = input["subject"]["id"].as_str().unwrap();
        let request = json!({"project": h.project, "task": task, "builds": input["builds"]});
        let (operation, actor) = (Uuid::new_v4(), h.f.actor);
        for _ in 0..2 {
            let answer = environments::admit(&h.pool, h.ops.as_ref(), actor, operation, &request)
                .await
                .unwrap();
            assert_eq!(answer, Some(("admitted", json!({}))));
        }
        assert_eq!(h.versions("acme.web").await.len(), 1);
    }

    /// Fix round 1: a binding that refuses the change leaves the versions
    /// that were there (nothing pruned, nothing added); one that asks to try
    /// again completes on a later pass.
    #[tokio::test]
    async fn a_refused_admit_leaves_the_previous_versions_intact_and_a_retry_completes_later() {
        let h = harness(false).await;
        keep_versions(&h, 1).await;
        h.version("web", "1", 'p', 'b', '1').await;
        let input = merged_build(&h, 'c', 'e', 'a', 1_000).await;
        h.ops.answer.store(2, std::sync::atomic::Ordering::SeqCst);
        let refused = h.admit(input.clone()).await;
        assert_eq!(
            refused["output"],
            json!({"reason": "unknown_project", "step": "binding"})
        );
        let kept: Vec<String> = h
            .versions("acme.web")
            .await
            .into_iter()
            .map(|v| v.1)
            .collect();
        assert_eq!(kept, [digest('p')]);
        // Retry: no completion until the binding takes it.
        h.ops.answer.store(1, std::sync::atomic::Ordering::SeqCst);
        let handle = h.accepted("environment.admit", input).await;
        assert_eq!(h.pass().await, 0);
        assert!(h.completion(&handle).await.is_none());
        h.ops.answer.store(0, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(h.pass().await, 1);
        assert_eq!(h.completion(&handle).await.unwrap()["outcome"], "admitted");
        let kept: Vec<String> = h
            .versions("acme.web")
            .await
            .into_iter()
            .map(|v| v.1)
            .collect();
        assert_eq!(kept, [digest('a')], "pruned to keep_versions once bound");
    }

    #[tokio::test]
    async fn admit_takes_exactly_one_of_subject_and_project_and_only_its_own_recipes() {
        let h = harness(false).await;
        let input = merged_build(&h, 'c', 'e', 'a', 1_000).await;
        let mut both = input.clone();
        both["project"] = json!(h.project.to_string());
        assert_eq!(
            h.try_run("environment.admit", both).await.unwrap_err(),
            Error::Invalid
        );
        let neither = json!({"builds": input["builds"]});
        assert_eq!(
            h.try_run("environment.admit", neither).await.unwrap_err(),
            Error::Invalid
        );
        // A recipe name with a dot never becomes another namespace's id.
        let task = h.task('d');
        h.tree(&oid('d'), &[("beta.web", 'f')]);
        let built = h
            .built(
                json!({"subject": H::subject(task)}),
                json!([build("beta.web", 'b', false)]),
            )
            .await;
        h.merged(task, 2_000).await;
        let dotted = json!({"subject": H::subject(task), "builds": built["output"]["builds"]});
        assert_eq!(
            h.admit(dotted).await["output"],
            json!({"reason": "unknown_build", "step": "beta.web"})
        );
        // Another project's build is not this project's.
        let other = Uuid::new_v4();
        bind(&h.pool, other, "beta").await;
        sqlx::query("INSERT INTO environment_builds(project_id,digest,name,job_id,commit,base_template,build) VALUES($1,$2,'web',$3,$4,$5,$6)")
            .bind(other)
            .bind(digest('9'))
            .bind(Uuid::new_v4())
            .bind(oid('c'))
            .bind(base('1').image)
            .bind(build("web", '9', false))
            .execute(&h.pool)
            .await
            .unwrap();
        let mut theirs = input.clone();
        theirs["builds"][0]["digest"] = json!(digest('9'));
        assert_eq!(
            h.admit(theirs).await["output"],
            json!({"reason": "unknown_build", "step": "web"})
        );
        assert!(h.versions("acme.web").await.is_empty());
        assert!(h.versions("beta.web").await.is_empty());
    }

    /// Fix round 1: a rebuild on a new base skips an admitted environment
    /// whose recipe is no longer on main.
    #[tokio::test]
    async fn a_project_rebuild_skips_recipes_gone_from_main() {
        let h = harness(false).await;
        h.version("web", "1", 'w', 'b', '1').await;
        h.version("api", "1", 'x', 'c', '1').await;
        sqlx::query(
            "INSERT INTO delivery_main_heads(project_id,head,observed_at_ms) VALUES($1,$2,1)",
        )
        .bind(h.project)
        .bind(oid('f'))
        .execute(&h.pool)
        .await
        .unwrap();
        h.tree(&oid('f'), &[("web", 'b')]);
        h.accepted(
            "environment.build",
            json!({"project": h.project.to_string()}),
        )
        .await;
        let (_, recipes, ..) = h.jobs().await.pop().unwrap();
        assert_eq!(recipes, json!(["web"]));
    }

    /// Fix round 2: a retained build answers its replay from the record,
    /// without reading aq-git (here unreachable).
    #[tokio::test]
    async fn a_retained_build_replays_its_result_with_aq_git_unreachable() {
        let h = harness(false).await;
        let task = h.task('c');
        h.tree(&oid('c'), &[("web", 'e')]);
        let input = json!({"subject": H::subject(task)});
        let first = h
            .try_run_as("replayed-build", "environment.build", input.clone())
            .await
            .unwrap();
        assert_eq!(first.outcome, "accepted");
        h.ops.trees.lock().unwrap().clear();
        let again = h
            .try_run_as("replayed-build", "environment.build", input)
            .await
            .unwrap();
        assert_eq!((again.outcome, again.output), (first.outcome, first.output));
        assert_eq!(h.jobs().await.len(), 1, "no second job");
    }
}
