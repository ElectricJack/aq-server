//! Execute a validated Graph action against the observed subject and retain its native intent.
use super::*;
use crate::agent_access::{settlement_post, settlement_service_identity};
pub(super) struct Dispatch<'a, P> {
    pub action: &'a GraphAction<P>,
    pub state: &'a AppState,
    pub user: &'a User,
    pub request: &'a ActionRequest,
    pub payload: &'a P,
}
struct Subject {
    id: Uuid,
    workspace: Uuid,
    version: u64,
    node: Value,
}
impl Subject {
    /// Sets or clears the task's route; the stale check is the same as a patch's.
    async fn route(&self, state: &AppState, change: RouteChange) -> Result<Outcome> {
        let written = match change {
            RouteChange::Set(route) => {
                crate::routing::write(state, self.workspace, self.id, self.version, &route).await?
            }
            RouteChange::Clear => {
                crate::routing::clear(
                    state,
                    self.workspace,
                    self.id,
                    self.version,
                    aq_routing::Source::Owner,
                )
                .await?
            }
        };
        let rt = runtime(state)?;
        Ok(match written {
            crate::routing::Written::Routed { sequence } => Outcome::confirmed(
                vec![existing::entity(
                    "task_route",
                    self.id,
                    Some(sequence.to_string()),
                )],
                vec![existing::entity(
                    "task",
                    self.id,
                    Some((self.version + 1).to_string()),
                )],
            ),
            crate::routing::Written::Stale => {
                match current(rt, self.workspace, self.id, "task").await {
                    Ok(n) => n["version"]
                        .as_u64()
                        .map_or_else(|| Outcome::conflict(BTreeMap::new()), |v| stale("task", v)),
                    Err(_) => Outcome::conflict(BTreeMap::new()),
                }
            }
        })
    }
    async fn conflict<P: GraphPayload>(&self, graph: &Runtime) -> Outcome {
        if P::ACTIVATE_SUBTREE {
            return Outcome::conflict(BTreeMap::new());
        }
        if P::ENTITY == "project" {
            return existing::disposition(Disposition::Conflict);
        }
        // A matching version means Graph rejected the change itself, e.g. an open child.
        match current(graph, self.workspace, self.id, P::ENTITY).await {
            Ok(node) if node["version"].as_u64() == Some(self.version) => {
                existing::disposition(Disposition::Rejected)
            }
            Ok(node) => node["version"].as_u64().map_or_else(
                || Outcome::conflict(BTreeMap::new()),
                |v| stale(P::ENTITY, v),
            ),
            Err(_) => Outcome::conflict(BTreeMap::new()),
        }
    }
    async fn outcome<P: GraphPayload>(
        &self,
        graph: &Runtime,
        posted: (StatusCode, Value),
        operations: &[Value],
    ) -> Outcome {
        let (status, raw) = posted;
        match status.as_u16() {
            200..=299 if P::ACTIVATE_SUBTREE => subtree::confirmed(&raw, operations),
            200..=299 => confirmed(
                &raw,
                operations.len(),
                aq_human_work::task_graph::leading_guards(operations),
            ),
            409 if code(&raw) == Some("conflict") => self.conflict::<P>(graph).await,
            400 | 401 | 403 | 404 | 422 if code(&raw).is_some() => {
                existing::disposition(Disposition::Rejected)
            }
            _ => Outcome::unknown(),
        }
    }
}
impl<P: GraphPayload> Dispatch<'_, P> {
    pub(super) async fn run(&self) -> Result<Outcome> {
        let subject: Uuid = self.request.subject.id.parse().map_err(|_| invalid())?;
        let expected = version(self.payload.expected()).ok_or_else(invalid)?;
        let record = self
            .action
            .project(self.state, self.user, self.payload.project_id())
            .await?;
        let graph = runtime(self.state)?;
        let subject = self.subject(graph, &record, subject).await?;
        if subject.version != expected {
            return Ok(stale(P::ENTITY, subject.version));
        }
        if let Some(change) = self.payload.route_change() {
            return subject.route(self.state, change).await;
        }
        let mutations = match self.mutations(graph, &subject).await? {
            Ok(mutations) => mutations,
            Err(outcome) => return Ok(outcome),
        };
        if self.blocked(subject.id).await? {
            return Ok(existing::disposition(Disposition::Rejected));
        }
        let edge = async {
            let operations:Vec<_>=self.action.guards.iter().map(|(id,version)|json!({"op":"update_node","id":id,"expected_version":version,"patch":{}})).chain(mutations).collect();
            let id = self.payload.operation_id();
            let bytes = self.body(id, &operations)?;
            let service = settlement_service_identity(graph, subject.workspace)?;
            if !(journal::Admission {
                id,
                record: &record,
                service: &service,
                body: &bytes,
                #[cfg(feature = "project_environments")]
                needs_task: (P::KIND == needs::ACTION).then_some((
                    subject.id,
                    self.action.supervisor.is_none() && self.action.agent.is_none(),
                )),
            })
            .retain(self.state, self.user)
            .await?
            {
                return Ok(existing::disposition(Disposition::Rejected));
            }
            // After retaining the intent, a transport failure leaves the effect unknown.
            let posted = tokio::time::timeout(
                Duration::from_secs(3),
                settlement_post(graph, subject.workspace, bytes),
            )
            .await;
            match posted {
                Ok(Ok(response)) => Ok(subject.outcome::<P>(graph, response, &operations).await),
                _ => Ok(Outcome::unknown()),
            }
        };
        let Some(blocker) = self.payload.resume_held() else {
            return edge.await;
        };
        let at = (&record, subject.workspace, subject.id, blocker);
        let caller = (self.state, self.user, self.request);
        super::super::task_execution::resume_held(caller, at, edge).await
    }
    async fn subject(&self, rt: &Runtime, record: &ProjectRecord, id: Uuid) -> Result<Subject> {
        let workspace = record.binding.graph_workspace_id;
        let (node, live) = if P::ENTITY == "project" {
            (Value::Null, record.revision)
        } else {
            let raw = current(rt, workspace, id, P::ENTITY).await?;
            let v = raw["version"].as_u64().ok_or_else(unavailable)?;
            (raw, v)
        };
        Ok(Subject {
            id,
            workspace,
            version: live,
            node,
        })
    }
    async fn mutations(
        &self,
        rt: &Runtime,
        subject: &Subject,
    ) -> Result<std::result::Result<Vec<Value>, Outcome>> {
        let payload = self.payload;
        if let Some(action) = payload.retirement() {
            let reads = retirement::GraphReads {
                graph: rt,
                pool: &self.state.store.pool,
            };
            return retirement::plan(
                reads,
                (subject.workspace, self.user.id, payload.project_id()),
                &subject.node,
                action,
            )
            .await
            .map(Ok);
        }
        if P::ACTIVATE_SUBTREE {
            return subtree::plan(rt, subject.workspace, subject.id, subject.version).await;
        }
        let current = Current {
            subject: subject.id,
            actor: self.user.id,
            supervisor_actor: self.action.supervisor.as_ref().map(|p| p.actor_id),
            agent: self.action.agent.map(|ids| (ids.attempt_id, ids.task_id)),
            project: payload.project_id(),
            version: subject.version,
            node: &subject.node,
        };
        payload.operations(&current).map(Ok).map_err(|_| invalid())
    }
    async fn blocked(&self, subject: Uuid) -> Result<bool> {
        if P::never_attempted() || P::not_held() {
            let attempted: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE task_id=$1 AND ($2 OR holds_task))",
            )
            .bind(subject)
            .bind(P::never_attempted())
            .fetch_one(&self.state.store.pool)
            .await
            .map_err(|_| AppError::Internal)?;
            if attempted {
                return Ok(true);
            }
        }
        if P::never_granted() {
            return crate::role_settings::granted(&self.state.store.pool, subject).await;
        }
        Ok(false)
    }
    fn body(&self, id: Uuid, operations: &[Value]) -> Result<Vec<u8>> {
        let reason = self
            .request
            .reason
            .clone()
            .unwrap_or_else(|| format!("AQ dashboard action {} {id}", P::KIND));
        let body = json!({"idempotency_key":format!("aq-dashboard-action:{id}"),"reason":reason,"operations":operations});
        if !contract::upstream_valid("MutationRequest", &body) {
            return Err(invalid());
        }
        // This is native Graph JSON, which can include fractional metadata.
        // Integer-only canonical hashing belongs to the action request boundary.
        serde_json::to_vec(&body).map_err(|_| invalid())
    }
}
