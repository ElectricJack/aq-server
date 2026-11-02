//! Actual-owner setup before first project observation. This is test data wiring,
//! not qualification: caller retains real Graph/Git child pins and runtime proof.
use super::*;
use anyhow::{Context, Result, ensure};
use aq_admission::TaskRevision;
use aq_server::{
    agent_access::scheduling::{ReadyPage, TaskContext},
    project_bindings::domain::{
        EligibilityRequest, ExternalStatus, FrozenProjectBinding, ProjectBinding, ProjectRecord,
    },
    source_retention::SourceRetentionConfig,
};

/// No example catalog/model or source is implicitly selected. Every field comes
/// from the outer actual recipe BEFORE project creation or eligibility checks.
#[derive(Clone)]
pub struct ActualSetup {
    /// Declared enrolled worker version from the caller's pinned recipe, not a
    /// claim that a process has started. The real heartbeat replaces this metadata.
    pub enrollment_worker_version: String,
    pub project: ProjectBinding,
    pub catalog_artifact_json: String,
    pub graph: AgentAccessConfig,
    pub task: TaskRevision,
    pub source: SourceRetentionConfig,
}
#[derive(Clone)]
pub struct ActualProfileChoice {
    pub environment: String,
    pub agent_profile: String,
    pub intelligence_profile: String,
    pub execution_profile: String,
}
pub struct ActualSelectionFixture {
    pub env: Env,
    pub project: ProjectRecord,
    pub catalog_activation_revision: u64,
    pub lineage: aq_selection::TaskLineage,
    /// Immutable caller starting pin, checked against real owner reads. It is
    /// never replaced by a newer task revision to make admission pass.
    pub task: TaskRevision,
    pub actor: Uuid,
    pub ready: ReadyPage,
    pub context: TaskContext,
}
impl ActualSetup {
    /// Pure/local validation, before database creation or network observation.
    pub fn validate(&self) -> Result<aq_profiles::ValidatedCatalog> {
        ensure!(
            semver::Version::parse(&self.enrollment_worker_version).is_ok(),
            "actual enrollment worker version must come from a pinned recipe"
        );
        self.task
            .validate()
            .map_err(|_| anyhow::anyhow!("invalid actual task pin"))?;
        ensure!(
            self.project.project_id.get_version_num() == 4
                && self.project.repository_id.get_version_num() == 4
                && self.task.task_id.get_version_num() == 4
                && self.task.workspace_id.get_version_num() == 4,
            "actual recipe requires fresh project/repository/task/workspace UUIDs"
        );
        ensure!(
            self.project.graph_workspace_id == self.task.workspace_id,
            "actual project/workspace mismatch"
        );
        ensure!(
            self.project.enabled
                && self.project.allowed_node_ids.is_empty()
                && !self.project.allowed_worker_groups.is_empty(),
            "actual initial project must use explicit fresh enrolled-node groups"
        );
        ensure!(
            self.project.repository_adapter_reference == "aq.git.configured"
                && self.project.graph_adapter_reference == "aq.graph.configured",
            "fixed actual owner adapters required"
        );
        let origin = reqwest::Url::parse(&self.graph.graph_url)?;
        ensure!(
            origin.scheme() == "http"
                && origin.host_str() == Some("127.0.0.1")
                && origin.port().is_some_and(|p| p > 0)
                && origin.path() == "/"
                && origin.query().is_none()
                && origin.fragment().is_none()
                && origin.username().is_empty()
                && origin.password().is_none(),
            "actual fixture requires exact numeric-loopback Graph origin"
        );
        ensure!(
            self.graph
                .mutation_actor_id
                .as_ref()
                .is_some_and(|s| !s.is_empty()),
            "actual Graph writer actor required"
        );
        let catalog =
            aq_profiles::ValidatedCatalog::from_artifact(self.catalog_artifact_json.as_bytes())
                .context("actual canonical catalog refused")?;
        ensure!(
            catalog.canonical_bytes()? == self.catalog_artifact_json.as_bytes(),
            "actual catalog must be canonical original bytes"
        );
        ensure!(
            catalog.digest() == self.project.catalog_digest,
            "actual project/catalog digest mismatch"
        );
        Ok(catalog)
    }
}
impl ActualSelectionFixture {
    /// Optional diagnostic after a real worker heartbeat. Ordinary run_once's
    /// Poll performs its own production eligibility/selection; this never drives
    /// a proposal, records liveness or retimes an original snapshot.
    pub async fn freeze_after_heartbeat(
        &self,
        profiles: &ActualProfileChoice,
    ) -> Result<FrozenProjectBinding> {
        let request = EligibilityRequest {
            expected_project_revision: self.project.revision,
            expected_catalog_revision: self.catalog_activation_revision,
            node_id: self.env.node,
            environment: profiles.environment.clone(),
            agent_profile: profiles.agent_profile.clone(),
            intelligence_profile: profiles.intelligence_profile.clone(),
            execution_profile: profiles.execution_profile.clone(),
        };
        let current = aq_server::project_bindings::check_eligibility(
            &self.env.state,
            self.lineage.project_id,
            &request,
        )
        .await
        .map_err(|_| anyhow::anyhow!("actual current project eligibility refused"))?;
        ensure!(
            current.eligible,
            "actual worker/project not eligible: {:?}",
            current.reasons
        );
        current
            .frozen_binding
            .context("actual frozen project absent")
    }
}
pub async fn setup_actual(input: &ActualSetup) -> Result<ActualSelectionFixture> {
    let catalog = input.validate()?;
    let gate = fixture_gate().await;
    // These configured adapters exist before even the first project PUT. No
    // synthetic peer can be verified and then relabelled as a real owner later.
    let graph = GraphRuntime::new(&input.graph)
        .await
        .context("actual Graph runtime refused")?;
    let source = GitRuntime::new(input.source.clone()).context("actual source runtime refused")?;
    let db = Arc::new(TestDb::create().await);
    let store = db.store().await;
    let pool = db.pool().await;
    let dir = tempfile::tempdir()?;
    let now = Utc::now();
    let owner = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await?
        .context("actual fixture owner bootstrap refused")?;
    store
        .create_enrollment(
            "actual-project-enroll",
            owner.id,
            now,
            now + Duration::minutes(5),
        )
        .await?;
    let credential = tokens::new_node_credential();
    let node = store
        .redeem_enrollment(
            "actual-project-enroll",
            NewNode {
                name: "actual-project-worker".into(),
                credential_hash: tokens::verifier(&credential),
                worker_version: input.enrollment_worker_version.clone(),
                protocol_version: 1,
            },
            now,
        )
        .await?
        .context("actual node enrollment refused")?;
    // Enrollment is an actual owner operation, NOT a physical worker observation.
    // In particular no last_seen_at, fake protocol2 registration or inventory is set.
    let mut config: Config = toml::from_str(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#,
    )?;
    for group in &input.project.allowed_worker_groups {
        config
            .projects
            .worker_groups
            .insert(group.clone(), vec![node.id]);
    }
    input
        .project
        .validate(&config.projects)
        .map_err(anyhow::Error::msg)?;
    let state = AppState {
        config: Arc::new(config),
        store,
        identity: Arc::new(aq_server::identity::DevIdentity::new(
            "owner@localhost".into(),
        )),
        clock: Arc::new(Utc::now),
        agent_access: Some(Arc::new(graph)),
        source_retention: Some(Arc::new(source)),
    };
    ensure!(
        state.config.intelligence.is_none() && state.config.profiled_execution.is_none(),
        "actual base must not seed synthetic model/runtime configuration"
    );
    let app = aq_server::build_admin_router(state.clone());
    let (status,activation)=call(&app,"/api/profiles/catalogs/activate",Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"artifact_json":input.catalog_artifact_json})),"owner@localhost").await;
    ensure!(
        status == StatusCode::OK,
        "actual catalog activation refused: {activation}"
    );
    let (catalog_revision, active): (i64, Option<String>) = sqlx::query_as(
        "SELECT revision,active_digest FROM profile_catalog_registry WHERE singleton",
    )
    .fetch_one(&pool)
    .await?;
    ensure!(
        catalog_revision == 1 && active.as_deref() == Some(catalog.digest()),
        "actual catalog activation identity mismatch"
    );
    let body = serde_json::to_value(&input.project)?;
    let path = format!("/api/projects/{}", input.project.project_id);
    let (status, put) = call(
        &app,
        &path,
        Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"binding":body})),
        "owner@localhost",
    )
    .await;
    ensure!(
        status == StatusCode::OK,
        "actual source/Graph project validation refused: {put}"
    );
    let (status, record) = call(&app, &path, None, "owner@localhost").await;
    ensure!(status == StatusCode::OK, "actual project read refused");
    let project: ProjectRecord = serde_json::from_value(record)?;
    ensure!(
        project.revision == 1
            && project.validation.repository == ExternalStatus::Verified
            && project.validation.graph == ExternalStatus::Verified
            && project.binding.repository_id == input.project.repository_id
            && project.binding.graph_workspace_id == input.task.workspace_id
            && project.validation.repository_generation
                == Some(input.project.repository_binding_generation),
        "actual project owner observations mismatch"
    );
    let graph = state.agent_access.as_ref().unwrap();
    let ready = graph
        .scheduling_ready(input.task.workspace_id, 8, None)
        .await?;
    ensure!(
        ready.items.len() == 1
            && ready.next_cursor.is_none()
            && ready.items[0].revision == input.task,
        "actual readiness must contain exactly the original task pin"
    );
    let context = graph.scheduling_context(&input.task, 1, 64, 65_536).await?;
    ensure!(
        !context.truncated && context.context["node"] == ready.items[0].node,
        "actual context/root mismatch"
    );
    for key in ["nodes", "edges", "pinned_revisions"] {
        ensure!(
            context.context[key]
                .as_array()
                .is_some_and(|a| a.is_empty()),
            "actual pilot refuses required neighbor context, never trims it"
        );
    }
    let last_seen: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT last_seen_at FROM nodes WHERE id=$1")
            .bind(node.id)
            .fetch_one(&pool)
            .await?;
    ensure!(
        last_seen.is_none(),
        "actual constructor must not stamp worker liveness"
    );
    let empty:(i64,i64,i64,i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM intelligence_node_snapshots),(SELECT count(*) FROM intelligence_observations),(SELECT count(*) FROM execution_nodes),(SELECT count(*) FROM profiled_launch_registrations),(SELECT count(*) FROM intelligence_credential_definitions),(SELECT count(*) FROM intelligence_decisions),(SELECT count(*) FROM profiled_launches)").fetch_one(&pool).await?;
    ensure!(
        empty == (0, 0, 0, 0, 0, 0, 0),
        "actual constructor cannot install synthetic selection/registration facts"
    );
    let env = Env {
        app,
        state,
        pool,
        node: node.id,
        body,
        generation: Arc::new(AtomicU64::new(input.project.repository_binding_generation)),
        git_status: Arc::new(AtomicU8::new(0)),
        graph_status: Arc::new(AtomicU8::new(0)),
        time: Arc::new(AtomicI64::new(now.timestamp_millis())),
        tasks: vec![],
        _dir: dir,
        _db: db,
        credential,
        _gate: gate,
    };
    Ok(ActualSelectionFixture {
        env,
        project,
        catalog_activation_revision: catalog_revision as u64,
        lineage: aq_selection::TaskLineage {
            project_id: input.project.project_id,
            workspace_id: input.task.workspace_id,
            task_id: input.task.task_id,
        },
        task: input.task.clone(),
        actor: owner.id,
        ready,
        context,
    })
}
