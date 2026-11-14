//! Host9 (catalog 2.2.0) task facts and routes: `work.read`, `work.route`
//! and `work.unroutable`. One final effect row per intent; the pure parts
//! are `aq_playbook_domain::host9`.
use super::*;
use aq_playbook_domain::host9::{self, Provenance, WorkFacts};
use aq_routing::{Route, Source, Written, classify::Options};

/// What the routing commands need of the server besides the graph.
#[async_trait::async_trait]
pub trait RoutingPort: Send + Sync {
    /// The workspace and project of a task the server asked a route for.
    async fn located(&self, task: Uuid) -> Result<Option<(Uuid, Uuid)>>;
    /// Whether the project's binding runs its agent on the automatic router.
    async fn on_router(&self, project: Uuid) -> Result<bool>;
    /// The task's (charged failures, ended attempts).
    async fn attempts(&self, task: Uuid) -> Result<(u32, u32)>;
    async fn options(&self) -> Result<Options>;
    /// Writes the router's route for the task at `version`, or clears it.
    async fn write(&self, at: (Uuid, Uuid, u64), route: Option<&Route>) -> Result<Written>;
}

/// Production port: the server's routing, bindings and attempt history.
pub struct ServerRouting(pub crate::AppState);
#[async_trait::async_trait]
impl RoutingPort for ServerRouting {
    async fn located(&self, task: Uuid) -> Result<Option<(Uuid, Uuid)>> {
        let row: Option<(Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
            "SELECT workspace_id,project_id FROM route_needed_emissions WHERE task_id=$1",
        )
        .bind(task)
        .fetch_optional(&self.0.store.pool)
        .await?;
        Ok(row.and_then(|(workspace, project)| workspace.zip(project)))
    }
    async fn on_router(&self, project: Uuid) -> Result<bool> {
        let record = match crate::project_bindings::current(&self.0, project).await {
            Ok(record) => record,
            Err(crate::error::AppError::NotFound(_)) => return Ok(false),
            Err(_) => return Err(Error::Unknown),
        };
        let on = crate::routing::on_router(&self.0, &record).await;
        on.map_err(|_| Error::Unknown)
    }
    async fn attempts(&self, task: Uuid) -> Result<(u32, u32)> {
        use crate::profiled_execution::{no_grant, replacement};
        let mut conn = self.0.store.pool.acquire().await?;
        let ended: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM execution_attempts WHERE task_id=$1 AND NOT holds_task",
        )
        .bind(task)
        .fetch_one(&mut *conn)
        .await?;
        let counts = no_grant::attempt_counts_locked(&mut conn, task).await;
        let started = counts.map_err(|_| Error::Unknown)?.started;
        let priors = replacement::load(&mut conn, task, started).await;
        let mut priors = priors.map_err(|_| Error::Unknown)?.unwrap_or_default();
        priors.retain(|p| !p.holds_task);
        let charged = aq_profiled_state::replacement::charged(&priors).0;
        Ok((charged, u32::try_from(ended).unwrap_or(u32::MAX)))
    }
    async fn options(&self) -> Result<Options> {
        let options = crate::routing::options(&self.0).await;
        options.map_err(|_| Error::Unknown)
    }
    async fn write(&self, at: (Uuid, Uuid, u64), route: Option<&Route>) -> Result<Written> {
        let (workspace, task, version) = at;
        let written = match route {
            Some(route) => crate::routing::write(&self.0, workspace, task, version, route).await,
            None => crate::routing::clear(&self.0, workspace, task, version, Source::Router).await,
        };
        written.map_err(|_| Error::Unknown)
    }
}

fn result(outcome: &str, output: Value) -> HostResult {
    HostResult {
        outcome: outcome.into(),
        output,
    }
}

impl Service {
    pub(super) async fn work_routing(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Unknown),
            Stage::Fresh => {}
        }
        let result = self.route_command(&mut tx, intent).await?;
        self.settle_fresh(tx, id, revision, intent, &identity, result)
            .await
    }

    /// The task a routing command names: its id, workspace and graph node,
    /// or the catalog's refusal reason. R1: with `installation_routing` a
    /// task of any project on the automatic router; otherwise only one of
    /// the namespace's project.
    pub(super) async fn routed_task(
        &self,
        subject: &Value,
    ) -> Result<std::result::Result<(Uuid, (Uuid, Uuid), Value), &'static str>> {
        let ports = self.ports()?;
        let Ok(task) = Uuid::parse_str(text(subject, "id")?) else {
            return Ok(Err("unknown_task"));
        };
        let own = (self.namespace.workspace_id, self.namespace.project_id);
        let (workspace, project) = ports.routing.located(task).await?.unwrap_or(own);
        if (project != own.1 && !self.installation_routing)
            || !ports.routing.on_router(project).await?
        {
            return Ok(Err("not_on_router"));
        }
        Ok(match ports.graph.node(workspace, task).await? {
            Some(node) => Ok((task, (workspace, project), node)),
            None => Err("unknown_task"),
        })
    }

    async fn route_command(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
    ) -> Result<HostResult> {
        let (input, name) = (&intent.input, intent.command.name.as_str());
        let id = text(&input["subject"], "id")?;
        let asked = input["subject"]["revision"]
            .as_i64()
            .ok_or(Error::Invalid)?;
        let (task, (workspace, project), node) = match self.routed_task(&input["subject"]).await? {
            Ok(found) => found,
            Err(reason) => return Ok(result("refused", host9::refusal(id, asked, reason))),
        };
        let routing = &self.ports()?.routing;
        let version = node_version(&node)?;
        if name == "work.read" {
            let (charged, ended) = routing.attempts(task).await?;
            let ours = self.namespace_work(tx, task).await?;
            let facts = WorkFacts::of(&node, ours, charged, ended);
            return Ok(result(
                "observed",
                host9::read_output(task, version, &facts),
            ));
        }
        let reasons = &input["reasons"];
        let stored = host9::route_source(&node["metadata"]);
        let refused = |reason| Ok(result("refused", host9::refusal(id, version, reason)));
        if name == "work.unroutable" && reasons.as_array().is_none_or(Vec::is_empty) {
            return refused("no_reasons");
        }
        if stored == Some(Source::Owner) {
            return refused("owner_route");
        }
        let stale = || Ok(result("stale", host9::task_output(id, version)));
        if asked != version {
            return stale();
        }
        let at = (workspace, task, version as u64);
        if name == "work.unroutable" {
            // The task is left unrouted: a router route it still has goes.
            let mut left = version;
            if stored == Some(Source::Router) {
                if routing.write(at, None).await? == Written::Stale {
                    return stale();
                }
                let node = self.ports()?.graph.node(workspace, task).await?;
                left = node_version(&node.ok_or(Error::Unknown)?)?;
            }
            self.route_settled(tx, (task, project), left, "unroutable", input, None)
                .await?;
            return Ok(result("recorded", host9::task_output(id, left)));
        }
        let options = routing.options().await?;
        let (artifact_digest, source_revision) = self.routing_artifact(tx).await?;
        let from = Provenance {
            artifact_digest: artifact_digest.as_deref(),
            source_revision,
            ended_attempts: routing.attempts(task).await?.1,
        };
        let Some(route) = host9::route_from_input(input, &options, from) else {
            let offers = host9::options_digest(&options);
            self.route_settled(
                tx,
                (task, project),
                version,
                "not_offered",
                input,
                Some(&offers),
            )
            .await?;
            return refused("not_offered");
        };
        Ok(match routing.write(at, Some(&route)).await? {
            Written::Routed { .. } => result("routed", host9::routed_output(id, version, &route)),
            Written::Stale => return stale(),
        })
    }

    /// The active artifact of the routing binding and the source revision it
    /// was authored from: the active authoring run, else the newest
    /// activation in this namespace's ledger.
    pub(crate) async fn routing_artifact(
        &self,
        db: &mut PgConnection,
    ) -> Result<(Option<String>, Option<u64>)> {
        let authored: Option<(String, i64)> = sqlx::query_as("SELECT r.artifact_digest,r.source_revision FROM playbook_authoring_runs r JOIN playbook_sources s ON s.id=r.source_id WHERE s.binding=$1 AND r.status='saved' AND r.active")
            .bind(host9::ROUTING_BINDING)
            .fetch_optional(&mut *db)
            .await?;
        if let Some((digest, revision)) = authored {
            return Ok((Some(digest), u64::try_from(revision).ok()));
        }
        let activated = sqlx::query_scalar("SELECT artifact FROM playbook_activations WHERE namespace_id=$1 AND binding=$2 AND kind<>'install' ORDER BY seq DESC LIMIT 1")
            .bind(self.namespace.namespace_id)
            .bind(host9::ROUTING_BINDING)
            .fetch_optional(&mut *db)
            .await?;
        Ok((activated, None))
    }

    /// Records why the task is not asked about again (`not_offered`,
    /// `unroutable`) for this task version, routing artifact and offers, and
    /// raises it (W6), once for this outcome at this task version.
    async fn route_settled(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        (task, project): (Uuid, Uuid),
        version: i64,
        outcome: &str,
        input: &Value,
        offers: Option<&str>,
    ) -> Result<()> {
        let reasons = &input["reasons"];
        self.routing_failure((task, project), outcome, version, input)
            .await?;
        let (artifact, _) = self.routing_artifact(tx).await?;
        sqlx::query("INSERT INTO route_needed_emissions(task_id,task_version,emitted_at_ms,outcome,reasons,artifact_digest,options_digest) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(task_id) DO UPDATE SET task_version=$2,outcome=$4,reasons=$5,artifact_digest=$6,options_digest=$7")
            .bind(task)
            .bind(version)
            .bind(self.now()?)
            .bind(outcome)
            .bind(reasons)
            .bind(artifact)
            .bind(offers)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// W6: a task routing could not route follows the internal work failure
    /// path (a supervisor first, then the owner's attention), once per cause.
    pub(crate) async fn routing_failure(
        &self,
        (task, project): (Uuid, Uuid),
        outcome: &str,
        version: i64,
        input: &Value,
    ) -> Result<bool> {
        use aq_internal_work::{Affected, Purpose, Reason, store};
        let (cause, words) = host9::routing_failure(outcome, version, input);
        let affected = Affected {
            kind: "task".into(),
            id: task.to_string(),
            project_id: Some(project),
        };
        let reason = match outcome {
            "unrouted" => Reason::DeadlineExceeded,
            _ => Reason::NoCandidate,
        };
        let purpose = Purpose::RoutingClassification;
        let raised = store::record_failure(
            &self.pool,
            purpose,
            &affected,
            &cause,
            (reason, &words),
            self.now()?,
        );
        Ok(raised.await.map_err(|_| Error::Unknown)?.is_some())
    }
}
