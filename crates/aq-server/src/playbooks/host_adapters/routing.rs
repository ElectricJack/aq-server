//! Host9 (catalog 2.2.0) task facts and routes: `work.read`, `work.route`
//! and `work.unroutable`. One final effect row per intent; the pure parts
//! are `aq_playbook_domain::host9`.
use super::*;
use aq_playbook_domain::host9::{self, Provenance, WorkFacts};
#[cfg(feature = "project_environments")]
use aq_playbook_domain::host10;
use aq_playbook_domain::route_effect::{RouteTarget, RoutingFailure};
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
    /// The project's environments (spec 0017 §5.4).
    #[cfg(feature = "project_environments")]
    async fn environments(&self, project: Uuid) -> Result<host10::Environments>;
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
        // Every bound project's tasks are routed (spec 0014 §5.2).
        match crate::project_bindings::current(&self.0, project).await {
            Ok(_) => Ok(true),
            Err(crate::error::AppError::NotFound(_)) => Ok(false),
            Err(_) => Err(Error::Unknown),
        }
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
        let limits = replacement::limits(&mut conn, &priors).await;
        let limits = limits.map_err(|_| Error::Unknown)?;
        let charged = aq_profiled_state::replacement::charged(&priors, &limits).0;
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
    #[cfg(feature = "project_environments")]
    async fn environments(&self, project: Uuid) -> Result<host10::Environments> {
        let (state, unknown) = (&self.0, |_| Error::Unknown);
        let binding = (crate::project_bindings::current(state, project).await).map_err(unknown)?;
        let catalog = crate::profiled_execution::definitions::catalog(state, &state.store.pool);
        let catalog = catalog.await.map_err(unknown)?;
        let allowed = binding.binding.allowed_environments.iter();
        Ok(host10::Environments {
            default: binding.binding.default_environment.unwrap_or_default(),
            allowed: allowed.map(|e| (e.clone(), catalog.provides(e))).collect(),
        })
    }
}

impl Service {
    pub(super) async fn work_routing(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Unknown),
            Stage::Fresh => {}
        }
        let result = self.route_command(&mut tx, intent).await?;
        self.settle_fresh(tx, command.retained(&identity), result)
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
        let outside = project != own.1 && !self.installation_routing;
        if outside || !ports.routing.on_router(project).await? {
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
        if name == "work.read" && self.profile.is_delivery() && intent.command.version != "1.0.0" {
            return self.read_delivery(tx, input, &intent.command.version).await;
        }
        let id = text(&input["subject"], "id")?;
        let asked = input["subject"]["revision"]
            .as_i64()
            .ok_or(Error::Invalid)?;
        let (task, (workspace, project), node) = match self.routed_task(&input["subject"]).await? {
            Ok(found) => found,
            Err(reason) => return Ok(host_result("refused", host9::refusal(id, asked, reason))),
        };
        let routing = &self.ports()?.routing;
        let version = node_version(&node)?;
        if name == "work.read" {
            let (charged, ended) = routing.attempts(task).await?;
            let ours = self.namespace_work(tx, task).await?;
            let facts = WorkFacts::of(&node, ours, charged, ended);
            return Ok(host_result(
                "observed",
                host9::read_output(task, version, &facts),
            ));
        }
        let stored = host9::route_source(&node["metadata"]);
        let target = RouteTarget {
            id,
            task,
            workspace,
            project,
            version,
            input,
        };
        if let Some(refusal) = target.refusal(name, asked, stored) {
            return Ok(refusal);
        }
        if name == "work.unroutable" {
            return self.clear_route(tx, target, stored).await;
        }
        self.write_route(tx, target, &node["metadata"]).await
    }

    /// Clears the router-owned route and records the graph version it left.
    async fn clear_route(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        target: RouteTarget<'_>,
        stored: Option<Source>,
    ) -> Result<HostResult> {
        let mut version = target.version;
        if stored == Some(Source::Router) {
            let ports = self.ports()?;
            if ports.routing.write(target.location(), None).await? == Written::Stale {
                return Ok(target.stale());
            }
            let node = ports.graph.node(target.workspace, target.task).await?;
            version = node_version(&node.ok_or(Error::Unknown)?)?;
        }
        self.route_settled(tx, target.failure("unroutable", version), None)
            .await?;
        Ok(host_result(
            "recorded",
            host9::task_output(target.id, version),
        ))
    }

    #[cfg_attr(not(feature = "project_environments"), allow(unused_variables))]
    async fn write_route(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        target: RouteTarget<'_>,
        metadata: &Value,
    ) -> Result<HostResult> {
        let routing = &self.ports()?.routing;
        let options = routing.options().await?;
        let (artifact_digest, source_revision) = self.deciding_artifact(tx, target.task).await?;
        let from = Provenance {
            artifact_digest: artifact_digest.as_deref(),
            source_revision,
            ended_attempts: routing.attempts(target.task).await?.1,
        };
        let Some(route) = host9::route_from_input(target.input, &options, from) else {
            let offers = self.offers_digest(&options, target.project).await?;
            self.route_settled(
                tx,
                target.failure("not_offered", target.version),
                Some(&offers),
            )
            .await?;
            return Ok(host_result(
                "refused",
                host9::refusal(target.id, target.version, "not_offered"),
            ));
        };
        // Spec 0017 §5.4: a routed environment is one the project allows
        // that provides every need; a refusal says which and why.
        #[cfg(feature = "project_environments")]
        if let Some(environment) = route.environment.as_deref() {
            let environments = routing.environments(target.project).await?;
            let needs = host10::needs(metadata);
            if let Some(refusal) = environments.refusal(environment, &needs) {
                let mut input = target.input.clone();
                input["environment_refusal"] = refusal;
                let offers = environments.offers_digest(&options);
                let failure = RoutingFailure {
                    input: &input,
                    ..target.failure("not_offered", target.version)
                };
                self.route_settled(tx, failure, Some(&offers)).await?;
                return Ok(host_result(
                    "refused",
                    host9::refusal(target.id, target.version, "not_offered"),
                ));
            }
        }
        if routing.write(target.location(), Some(&route)).await? == Written::Stale {
            return Ok(target.stale());
        }
        // The task has its route: it no longer waits, and nothing more is asked.
        sqlx::query("UPDATE route_needed_emissions SET wanted_at_ms=NULL,outcome=NULL,reasons=NULL WHERE task_id=$1")
            .bind(target.task).execute(&mut **tx).await?;
        Ok(host_result(
            "routed",
            host9::routed_output(target.id, target.version, &route),
        ))
    }

    /// Catalog 2.3.0 `work.read` (spec 0016 section 6.2): the task's facts and
    /// those of its latest result, for a task of any bound project. The 2.6.0
    /// and 2.7.0 ones add the task's acceptance criteria and the result's
    /// conclusion (2.7.0's up to 4096 bytes), 2.8.0's adds the head the pull
    /// request's checks were evaluated on, and 2.9.0's the head whose checks
    /// failed as a plain fact (`failed_checks_head`).
    async fn read_delivery(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        input: &Value,
        version: &str,
    ) -> Result<HostResult> {
        use aq_playbook_domain::{delivery_store as store, host10};
        let (ports, id) = (self.ports()?, text(&input["subject"], "id")?);
        let asked = input["subject"]["revision"]
            .as_i64()
            .ok_or(Error::Invalid)?;
        let refused = |reason| Ok(host_result("refused", host9::refusal(id, asked, reason)));
        // Catalog 2.5.0 is routing's: only a task on the automatic router.
        #[cfg(feature = "project_environments")]
        if version == "2.5.0"
            && let Err(reason) = self.routed_task(&input["subject"]).await?
        {
            return refused(reason);
        }
        let Ok(task) = Uuid::parse_str(id) else {
            return refused("unknown_task");
        };
        let own = (self.namespace.workspace_id, self.namespace.project_id);
        let fallback = ports.routing.located(task).await?.unwrap_or(own);
        let (latest, at, bound) = store::locate(&*self.delivery, task, fallback).await?;
        let Some(node) = ports.graph.node(at.0, task).await?.filter(|_| bound) else {
            return refused(if bound {
                "unknown_task"
            } else {
                "not_on_router"
            });
        };
        let (charged, ended) = ports.routing.attempts(task).await?;
        let facts = (self.namespace_work(tx, task).await?, charged, ended);
        let output = store::observed(tx, task, at, latest.as_ref(), &node, facts).await?;
        let output = host10::for_catalog(output, version);
        let output = match version {
            "2.6.0" | "2.7.0" | "2.8.0" | "2.9.0" => {
                let conclusion = match &latest {
                    Some(l) => self.final_message(task, l).await?,
                    None => None,
                };
                let conclusion = conclusion
                    .as_deref()
                    .or(latest.as_ref().map(|l| l.conclusion.as_str()))
                    .unwrap_or("");
                host10::with_judgment_facts(output, &node, conclusion, version)
            }
            #[cfg(feature = "project_environments")]
            "2.5.0" => {
                (ports.routing.environments(at.1).await?).read_output(output, &node["metadata"])
            }
            _ => output,
        };
        Ok(host_result("observed", output))
    }

    /// The whole final message the worker posted as a task comment for the
    /// attempt of `latest` (the report's conclusion is bounded to 512 bytes).
    async fn final_message(
        &self,
        task: Uuid,
        latest: &aq_playbook_domain::delivery_store::Latest,
    ) -> Result<Option<String>> {
        use aq_task_comments::{Author, MAX_LIMIT, Thread};
        let thread = Thread {
            project_id: latest.project_id,
            task_id: task,
        };
        // shortcut: only the newest page of comments is searched; the worker posts the final message right after the result, so it is there when delivery reads it. Page with `before` if tasks gather 100+ comments after a result.
        let (comments, _) = aq_task_comments::list(&self.pool, thread, None, MAX_LIMIT)
            .await
            .map_err(|_| Error::Unknown)?;
        // Only the attempt itself can author a comment as it.
        Ok(comments.iter().find_map(|c| match c.author {
            Author::Agent { attempt_id, .. } if attempt_id == latest.attempt_id => {
                aq_playbook_domain::host10::final_message(c.body.as_deref()?, attempt_id)
                    .map(str::to_owned)
            }
            _ => None,
        }))
    }

    /// The artifact the engine has active on the routing binding (what the
    /// playbook owner last activated there, by any path) and the source
    /// revision it was authored from, when it was authored.
    pub(crate) async fn routing_artifact(
        &self,
        db: &mut PgConnection,
    ) -> Result<(Option<String>, Option<u64>)> {
        let active: Option<String> = sqlx::query_scalar("SELECT artifact FROM playbook_engine_bindings WHERE namespace_id=$1 AND generation=$2 AND binding=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(host9::ROUTING_BINDING)
            .fetch_optional(&mut *db)
            .await?;
        self.authored(db, active).await
    }

    /// `artifact` with the source revision of the authoring run that saved it.
    async fn authored(
        &self,
        db: &mut PgConnection,
        artifact: Option<String>,
    ) -> Result<(Option<String>, Option<u64>)> {
        let revision: Option<i64> = sqlx::query_scalar("SELECT source_revision FROM playbook_authoring_runs WHERE artifact_digest=$1 AND status='saved' ORDER BY id DESC LIMIT 1")
            .bind(&artifact)
            .fetch_optional(&mut *db)
            .await?;
        Ok((artifact, revision.and_then(|r| u64::try_from(r).ok())))
    }

    /// The artifact that decides about `task`: the one that was active when
    /// the server asked for its route (the run that answers started under
    /// it), else the active one. A run that began under an earlier artifact
    /// never speaks for a later one.
    async fn deciding_artifact(
        &self,
        db: &mut PgConnection,
        task: Uuid,
    ) -> Result<(Option<String>, Option<u64>)> {
        let asked: Option<Option<String>> = sqlx::query_scalar("SELECT artifact_digest FROM route_needed_emissions WHERE task_id=$1 AND emitted_at_ms>0 AND outcome IS NULL")
            .bind(task)
            .fetch_optional(&mut *db)
            .await?;
        match asked.flatten() {
            Some(asked) => self.authored(db, Some(asked)).await,
            None => self.routing_artifact(db).await,
        }
    }

    /// Records what the playbook owner activated in the engine for a binding.
    pub(crate) async fn binding_activated(&self, binding: &str, artifact: &str) -> Result<()> {
        sqlx::query("INSERT INTO playbook_engine_bindings(namespace_id,generation,binding,artifact,at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(namespace_id,generation,binding) DO UPDATE SET artifact=$4,at_ms=$5 WHERE playbook_engine_bindings.artifact<>$4")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(binding)
            .bind(artifact)
            .bind(self.now()?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn binding_deactivated(&self, binding: &str) -> Result<()> {
        sqlx::query("DELETE FROM playbook_engine_bindings WHERE namespace_id=$1 AND generation=$2 AND binding=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(binding)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// What a `not_offered` task is asked about again on: the Settings
    /// offers and, on catalog 2.5.0, the project's environments.
    pub(crate) async fn offers_digest(&self, options: &Options, project: Uuid) -> Result<String> {
        #[cfg(feature = "project_environments")]
        if self.profile.has_environments() {
            let environments = self.ports()?.routing.environments(project).await?;
            return Ok(environments.offers_digest(options));
        }
        let _ = project;
        Ok(host9::options_digest(options))
    }

    /// Records why the task is not asked about again (`not_offered`,
    /// `unroutable`) for this task version, routing artifact and offers, and
    /// raises it (W6), once for this outcome at this task version.
    async fn route_settled(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        failure: RoutingFailure<'_>,
        offers: Option<&str>,
    ) -> Result<()> {
        let RoutingFailure {
            at: (task, _),
            version,
            outcome,
            input,
        } = failure;
        let reasons = &input["reasons"];
        self.routing_failure(failure).await?;
        // Settled under the artifact that was asked (the row's), not one
        // activated since: a new artifact is a new cause and asks again.
        let (artifact, _) = self.deciding_artifact(tx, task).await?;
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
    pub(crate) async fn routing_failure(&self, failure: RoutingFailure<'_>) -> Result<bool> {
        let RoutingFailure {
            at: (task, project),
            outcome,
            version,
            input,
        } = failure;
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
        let raised = store::Queue::at(&self.pool, self.now()?).record_failure(
            store::FailureCause {
                purpose,
                affected: &affected,
                cause: &cause,
            },
            (reason, &words),
        );
        Ok(raised.await.map_err(|_| Error::Unknown)?.is_some())
    }
}
