use super::{FakeGraph, keyring, outcome, protected_factory};
use crate::common::profiled::{Fixture, fixture, insert};
use aq_playbook_authoring::compiler::draft::{Workspace, seal};
use aq_playbook_host::plugins::Plugins;
use aq_server::playbooks::{
    domain::{GrantScope, Namespace},
    driver::Driver,
    review_domain::CatalogProfile,
    service::{PrepareRequest, Service},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use uuid::Uuid;

const ENVIRONMENT: &str = "aq-graph-deploy-canary";
const BINDING: &str = "aq.graph-deployment";

fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn command(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn docker(directory: &Path) -> Command {
    let mut command = Command::new("docker");
    command
        .arg("--config")
        .arg(directory.join("docker"))
        .args(["--host", "unix:///var/run/docker.sock"]);
    command
}

fn tick(plugin: &Path, directory: &Path) {
    command(Command::new(plugin).arg("tick").arg(directory));
}

async fn start(service: &Service, f: &Fixture) -> Driver {
    Driver::start(
        service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(f),
    )
    .await
    .unwrap()
}

struct Canary {
    f: Fixture,
    service: Service,
    driver: Driver,
    grant: Uuid,
    activation: u64,
    directory: PathBuf,
    plugin: PathBuf,
    container: String,
    bindings: PathBuf,
    #[cfg(feature = "delivery_playbook")]
    scope: GrantScope,
    #[cfg(feature = "delivery_playbook")]
    artifact: String,
}

impl Canary {
    fn opt_in(&self, extra: &str) {
        fs::write(
            &self.bindings,
            format!(
                "[projects.\"{}\"]\nenvironment = \"{ENVIRONMENT}\"\ndirectory = {:?}\n{extra}",
                self.service.namespace.project_id, self.directory
            ),
        )
        .unwrap();
        fs::set_permissions(&self.bindings, fs::Permissions::from_mode(0o600)).unwrap();
    }

    async fn admit(&self, revision: &str, expected: u64) -> String {
        let request = PrepareRequest {
            request_id: Uuid::new_v4(),
            grant_id: self.grant,
            grant_revision: 1,
            binding: BINDING.into(),
            expected_activation_revision: self.activation,
            entry: "main".into(),
            input: json!({"revision": revision,"expected_generation": expected}),
        };
        self.service
            .prepare(self.f.actor, request.clone())
            .await
            .unwrap();
        self.driver
            .admission(self.f.actor, request.request_id, false)
            .await
            .unwrap()
            .receipt
            .unwrap()
            .run_id
    }

    async fn receipt(&self, run: &str, status: &str) -> Option<Value> {
        sqlx::query_scalar(
            "SELECT receipt->'output' FROM playbook_effects
             WHERE run_id=$1 AND receipt->>'outcome'=$2 LIMIT 1",
        )
        .bind(run)
        .bind(status)
        .fetch_optional(&self.f.db.pool().await)
        .await
        .unwrap()
    }

    async fn wait_for_receipt(&self, run: &str, status: &str) -> Value {
        for _ in 0..64 {
            self.driver.quantum().await.unwrap();
            if let Some(output) = self.receipt(run, status).await {
                return output;
            }
            self.f.time.fetch_add(2_000, Ordering::SeqCst);
        }
        panic!("missing {status} receipt: {}", self.view(run).await);
    }

    async fn view(&self, run: &str) -> Value {
        self.driver.inspect_run(run.into()).await.unwrap()["items"][0].clone()
    }

    async fn finish(&self, run: &str, expected: &str) {
        let mut next_tick = Instant::now();
        for _ in 0..64 {
            if expected == "healthy" && Instant::now() >= next_tick {
                tick(&self.plugin, &self.directory);
                next_tick = Instant::now() + Duration::from_secs(3);
            }
            self.driver.quantum().await.unwrap();
            let view = self.view(run).await;
            if matches!(
                view["status"].as_str(),
                Some("completed" | "failed" | "blocked")
            ) {
                assert_eq!(outcome(&view), ("completed", expected), "{view}");
                return;
            }
            self.f.time.fetch_add(2_000, Ordering::SeqCst);
        }
        panic!("run did not finish: {}", self.view(run).await);
    }

    fn provider(&self) -> String {
        command(docker(&self.directory).args(["inspect", "--format", "{{.Id}}", &self.container]))
    }

    async fn ready(&self) {
        for _ in 0..45 {
            tick(&self.plugin, &self.directory);
            let health = command(docker(&self.directory).args([
                "inspect",
                "--format",
                "{{.State.Health.Status}}",
                &self.container,
            ]));
            if health == "healthy" {
                self.assert_ready_endpoint().await;
                // A fresh tick records the successful provider observation.
                tick(&self.plugin, &self.directory);
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        panic!("disposable Graph failed to reach readiness");
    }

    async fn assert_ready_endpoint(&self) {
        let address = command(docker(&self.directory).args(["port", &self.container, "8741/tcp"]));
        assert!(address.starts_with("127.0.0.1:"));
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}/health/ready"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    async fn healthy(&self, run: &str, revision: &str, generation: u64, provider: &str) -> Value {
        self.finish(run, "healthy").await;
        let observed = self.receipt(run, "healthy").await.unwrap();
        let evidence = read(&self.directory.join(format!("provenance/{revision}.json")));
        assert_eq!(observed["generation"], generation);
        assert_eq!(observed["revision"], revision);
        assert_eq!(observed["provider_id"], provider);
        for (observed_field, evidence_field) in [
            ("image_digest", "image_digest"),
            ("image_id", "image_id"),
            ("observed_digest", "image_digest"),
            ("source_archive_digest", "source_archive_digest"),
            ("image_archive_digest", "image_archive_digest"),
        ] {
            assert_eq!(
                observed[observed_field], evidence[evidence_field],
                "{observed_field}"
            );
        }
        assert_eq!(
            command(docker(&self.directory).args([
                "inspect",
                "--format",
                "{{.Image}}",
                &self.container
            ])),
            evidence["image_id"].as_str().unwrap()
        );
        assert!(
            observed["observed_at_ms"].as_i64().unwrap()
                >= observed["requested_at_ms"].as_i64().unwrap()
        );
        println!("Observed {observed}");
        observed
    }
}

fn proof_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/graph-deployment")
}

fn deployment_artifact(root: &Path, service: &Service) -> aq_playbook_host::model::Artifact {
    let playbook = root.join("playbook");
    seal(&Workspace {
        markdown: fs::read_to_string(playbook.join("source.md")).unwrap(),
        record: serde_json::from_value(read(&playbook.join("record.json"))).unwrap(),
        catalog: service.registry().clone(),
        facts: serde_json::from_value(read(&playbook.join("facts.json"))).unwrap(),
        draft: read(&playbook.join("draft.json")),
    })
    .unwrap_or_else(|faults| panic!("{faults:?}"))
}

async fn prepare() -> Canary {
    let directory = PathBuf::from(std::env::var_os("AQ_GRAPH_DEPLOY_CANARY").unwrap());
    let root = proof_root();
    let plugin = root.join("plugins/graph_deploy/bin/graph-deploy");
    let f = fixture().await;
    insert(&f).await;
    let lineage = &f.selection.lineage;
    let settings_path = directory.join("environment.json");
    let mut settings = read(&settings_path);
    settings["project_id"] = json!(lineage.project_id);
    fs::write(settings_path, settings.to_string()).unwrap();
    let container = format!(
        "{ENVIRONMENT}-{}-graph",
        settings["instance"].as_str().unwrap()
    );

    let vault = directory.join("vault");
    let bindings = vault.join("plugins/graph_deploy/bindings.toml");
    fs::create_dir_all(bindings.parent().unwrap()).unwrap();

    let service = Service::new_host2(
        &f.state,
        Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: ENVIRONMENT.into(),
            workspace_id: lineage.workspace_id,
            project_id: lineage.project_id,
            owner_user_id: f.actor,
        },
        keyring(),
        CatalogProfile::Host11,
        Arc::new(FakeGraph::default()), // This playbook never calls a base Graph command.
        None,
        None,
        None,
    )
    .await
    .unwrap()
    .with_plugins(Plugins::discover(
        &root.join("plugins"),
        &directory.join("state"),
        &vault,
    ));
    service.initialize(f.actor).await.unwrap();
    let grant = Uuid::new_v4();
    let scope = GrantScope {
        workspace_id: lineage.workspace_id,
        project_id: lineage.project_id,
        recipients: vec![],
        capabilities: BTreeSet::from(["ext.graph_deploy".into()]),
    };
    service
        .grant(
            f.actor,
            grant,
            scope.clone(),
            (f.state.clock)().timestamp_millis() + 600_000,
        )
        .await
        .unwrap();
    let artifact = deployment_artifact(&root, &service);
    let driver = start(&service, &f).await;
    driver.install(artifact.clone()).await.unwrap();
    let activation = driver
        .activate(BINDING.into(), artifact.digest.clone(), grant, 1)
        .await
        .unwrap()["revision"]
        .as_u64()
        .unwrap();
    Canary {
        f,
        service,
        driver,
        grant,
        activation,
        directory,
        plugin,
        container,
        bindings,
        #[cfg(feature = "delivery_playbook")]
        scope,
        #[cfg(feature = "delivery_playbook")]
        artifact: artifact.digest,
    }
}

#[tokio::test]
#[ignore = "requires the isolated Docker fixture from scripts/test-graph-deployment.py"]
async fn merged_graph_rolls_out_by_verified_digest() {
    let revisions = std::env::var("AQ_GRAPH_DEPLOY_REVISIONS").unwrap();
    let (older, newer) = revisions.split_once(',').unwrap();
    let mut canary = prepare().await;
    let disabled = canary.admit(older, 0).await;
    canary.finish(&disabled, "refused").await;
    assert_eq!(
        canary.receipt(&disabled, "refused").await.unwrap()["reason"],
        "deployment_disabled"
    );
    canary.opt_in("");

    // Unknown source evidence is refused before any generation is registered.
    let unknown = canary.admit(&"0".repeat(40), 0).await;
    canary.finish(&unknown, "refused").await;
    assert_eq!(
        canary.receipt(&unknown, "refused").await.unwrap()["reason"],
        "build_unavailable"
    );

    let first = canary.admit(older, 0).await;
    let queued = canary.wait_for_receipt(&first, "queued").await;
    assert_eq!(queued["generation"], 1);
    assert_eq!(queued["observed_digest"], "");
    assert_eq!(queued["provider_id"], "");

    // Dispatch, then restart the AQ owner before it sees the running provider.
    tick(&canary.plugin, &canary.directory);
    let first_provider = canary.provider();
    canary.driver.stop().await.unwrap();
    canary.driver = start(&canary.service, &canary.f).await;
    canary.driver.recover().await.unwrap();
    let running = canary.wait_for_receipt(&first, "running").await;
    assert_eq!(running["observed_digest"], "");
    assert_eq!(running["operation_id"], queued["operation_id"]);
    canary.ready().await;
    canary.healthy(&first, older, 1, &first_provider).await;

    let stale = canary.admit(newer, 0).await;
    canary.finish(&stale, "refused").await;
    assert_eq!(
        canary.receipt(&stale, "refused").await.unwrap()["reason"],
        "generation_changed"
    );

    // An archive changed after its build receipt was recorded must leave the
    // previously running container alone. Retry recovers the same frozen build.
    let archive = canary.directory.join(format!("provenance/{newer}.tar"));
    let saved = archive.with_extension("saved");
    fs::rename(&archive, &saved).unwrap();
    fs::write(&archive, b"changed after build").unwrap();
    let second = canary.admit(newer, 1).await;
    let queued = canary.wait_for_receipt(&second, "queued").await;
    tick(&canary.plugin, &canary.directory);
    canary.finish(&second, "failed").await;
    let failed = canary.receipt(&second, "failed").await.unwrap();
    assert_eq!(failed["diagnostic"], "image_archive_mismatch");
    assert_eq!(failed["observed_digest"], "");
    assert_eq!(canary.provider(), first_provider);
    fs::rename(&saved, &archive).unwrap();

    let retry = canary.admit(newer, 2).await;
    canary.wait_for_receipt(&retry, "queued").await;
    tick(&canary.plugin, &canary.directory);
    let second_provider = canary.provider();
    assert_ne!(first_provider, second_provider);
    canary.ready().await;
    let healthy = canary.healthy(&retry, newer, 2, &second_provider).await;
    assert_eq!(healthy["operation_id"], queued["operation_id"]);
    assert_eq!(healthy["operation_id"], failed["operation_id"]);

    let delayed = canary.admit(older, 0).await;
    canary.finish(&delayed, "superseded").await;
    tick(&canary.plugin, &canary.directory);
    let replay = canary.admit(newer, 1).await;
    let observed = canary.healthy(&replay, newer, 2, &second_provider).await;
    assert_eq!(observed["operation_id"], healthy["operation_id"]);
    assert_eq!(canary.provider(), second_provider);
    canary.driver.stop().await.unwrap();
}

#[cfg(feature = "delivery_playbook")]
#[path = "graph_deployment_automatic.rs"]
mod automatic;
