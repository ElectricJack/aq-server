use super::*;
impl<P: GraphPayload> GraphAction<P> {
    fn verify_machine_subject(request: &ActionRequest, payload: &P) -> ApiResult<()> {
        if request.operation_id != payload.operation_id() || version(payload.expected()).is_none() {
            return Err(ApiError::invalid());
        }
        let subject = existing::subject(request, P::ENTITY, P::ENTITY, payload.expected());
        if P::ENTITY == "project" {
            if subject? != payload.project_id() {
                return Err(ApiError::invalid());
            }
        } else if subject.is_err() {
            return Err(ApiError::invalid());
        }
        Ok(())
    }
    async fn verify_task_scope(&self, state: &AppState, user: &User, payload: &P) -> ApiResult<()> {
        let needs_fences = self.machine() || P::KIND == "task.graph.create";
        if !needs_fences || !self.guards.is_empty() {
            return Ok(());
        }
        let record = self
            .project(state, user, payload.project_id())
            .await
            .map_err(ApiError::from)?;
        let rt = runtime(state).map_err(ApiError::from)?;
        for id in payload.tasks() {
            let node = current(rt, record.binding.graph_workspace_id, id, "task")
                .await
                .map_err(ApiError::from)?;
            if node["metadata"]["aq:project_id"] != json!(payload.project_id()) {
                return Err(ApiError::for_status(StatusCode::FORBIDDEN));
            }
        }
        Ok(())
    }
}
#[async_trait]
impl<P: GraphPayload> ActionKind for GraphAction<P> {
    type Payload = P;
    const KIND: &'static str = P::KIND;
    const GRANT: &'static str = P::GRANT;
    const TARGET: aq_command_registry::TargetRule = P::TARGET;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: &P,
    ) -> ApiResult<Scope> {
        let has_reason = request
            .reason
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty());
        let reason_ok = !P::reason_required() || has_reason;
        if !payload.valid() || !reason_ok {
            return Err(ApiError::invalid());
        }
        if self.machine() {
            Self::verify_machine_subject(request, payload)?;
        } else {
            OwnerRead {
                state,
                runtime: &self.runtime,
                user,
            }
            .admit(
                request,
                Claim {
                    operation: payload.operation_id(),
                    project: payload.project_id(),
                    entity: P::ENTITY,
                    expected: payload.expected(),
                    tasks: &payload.tasks(),
                },
            )
            .await?;
        }
        crate::role_settings::named_api(state, &payload.roles()).await?;
        if let Some(RouteChange::Set(route)) = payload.route_change()
            && !crate::routing::class_assigned(state, route.class)
                .await
                .map_err(ApiError::from)?
        {
            return Err(ApiError::invalid());
        }
        self.verify_task_scope(state, user, payload).await?;
        Ok(Scope::Project {
            project_id: payload.project_id(),
        })
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> ApiResult<()> {
        if !self.machine() {
            return OwnerRead {
                state,
                runtime: &self.runtime,
                user,
            }
            .lookup(scope, operation, P::ENTITY)
            .await;
        }
        let Scope::Project { project_id } = scope else {
            return Err(ApiError::invalid());
        };
        self.project(state, user, *project_id)
            .await
            .map_err(ApiError::from)?;
        admission::verify_scope(operation, P::ENTITY, *project_id)
    }
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        operation: &actions::Operation,
    ) -> ApiResult<Option<Outcome>> {
        let Some(intent) = journal::load(state, user, operation.operation_id, P::KIND)
            .await
            .map_err(ApiError::from)?
        else {
            // Pre-journal operations have no exact body to replay safely.
            return Ok(None);
        };
        let record = self
            .project(state, user, intent.project)
            .await
            .map_err(ApiError::from)?;
        let rt = runtime(state).map_err(ApiError::from)?;
        if !intent.matches(&operation.scope, &record, rt) {
            return Ok(Some(Outcome::unknown()));
        }
        // Graph scopes receipts by actor/workspace/operation/key and checks the
        // payload hash before executing. Replay the retained bytes, including
        // the original reason and metadata; never rebuild from current state.
        let posted = tokio::time::timeout(
            Duration::from_millis(1500),
            crate::agent_access::settlement_post(rt, intent.workspace, intent.body),
        )
        .await;
        Ok(Some(match posted {
            Ok(Ok((status, raw))) if status.is_success() => {
                if P::ACTIVATE_SUBTREE {
                    subtree::confirmed(&raw, &intent.operations)
                } else {
                    confirmed(&raw, intent.operations.len(), intent.guards)
                }
            }
            // A new authorization/conflict/transport failure cannot disprove
            // an earlier commit. Only its original receipt settles the effect.
            _ => Outcome::unknown(),
        }))
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: P,
    ) -> Outcome {
        let dispatch = dispatch::Dispatch {
            action: self,
            state,
            user,
            request,
            payload: &payload,
        };
        match dispatch.run().await {
            Ok(outcome) => outcome,
            Err(error) => existing::failure(error),
        }
    }
}
