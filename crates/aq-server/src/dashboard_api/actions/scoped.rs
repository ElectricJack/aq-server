//! Bind action admissions and replays to the current user and installed registry.
use super::*;
pub(crate) struct ScopedActions<'a> {
    state: &'a AppState,
    user: &'a User,
    registry: &'a Registry,
}
impl Registry {
    pub(crate) fn for_user<'a>(&'a self, state: &'a AppState, user: &'a User) -> ScopedActions<'a> {
        ScopedActions {
            state,
            user,
            registry: self,
        }
    }
}
impl ScopedActions<'_> {
    /// Creation-only machine registries pass an authenticated opaque session identity.
    pub(crate) async fn submit_scoped(
        &self,
        request: ActionRequest,
        admission: Option<AgentAdmission<'_>>,
    ) -> Result<Operation, ApiError> {
        let origin = admission.map_or(RequestOrigin::Union, RequestOrigin::Agent);
        self.submit_observed(request, None, origin).await
    }
    pub(crate) async fn lookup_scoped(&self, id: Uuid) -> Result<Operation, ApiError> {
        let (state, user, registry) = (self.state, self.user, self.registry);
        let lookup = journal::lookup_for_authorization(&state.store.pool, user.id, id)
            .await?
            .ok_or_else(|| ApiError::for_status(StatusCode::NOT_FOUND))?;
        let adapter = registry
            .get(&lookup.kind)
            .map_err(|_| ApiError::for_status(StatusCode::NOT_FOUND))?;
        adapter
            .authorize_lookup(state, user, lookup.target())
            .await
            .map_err(neutral_lookup)?;
        let op = lookup.operation.ok_or_else(|| journal::retired(id))?;
        reconcile(state, user, adapter.as_ref(), op)
            .await
            .map_err(neutral_lookup)
    }
    /// Resolve an exact replay before checking permissions against changed Graph facts.
    pub(crate) async fn replay_scoped(
        &self,
        request: &ActionRequest,
        scope: &Scope,
    ) -> Result<Option<Operation>, ApiError> {
        let (state, user) = (self.state, self.user);
        request.validate()?;
        let Some(existing) =
            journal::lookup_for_authorization(&state.store.pool, user.id, request.operation_id)
                .await?
        else {
            return Ok(None);
        };
        if existing.kind != request.kind || existing.scope != *scope {
            return Err(ApiError::for_status(StatusCode::CONFLICT));
        }
        if let Some(op) = existing.operation
            && canonical_digest(request, scope)? != op.request_digest
        {
            return Err(ApiError::for_status(StatusCode::CONFLICT));
        }
        self.lookup_scoped(request.operation_id).await.map(Some)
    }
    pub(super) async fn submit_observed(
        &self,
        request: ActionRequest,
        receipt: Option<&AdmissionReceipt>,
        origin: RequestOrigin<'_>,
    ) -> Result<Operation, ApiError> {
        let (state, user, registry) = (self.state, self.user, self.registry);
        let legacy = matches!(origin, RequestOrigin::Legacy);
        if legacy {
            // Native supervisor bodies allow JSON escaping within the shipped 128 KiB envelope.
            request.validate_with_limit(131072)?;
        } else {
            request.validate()?;
        }
        let adapter = registry.get(&request.kind)?;
        let scope = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            adapter.authorize(state, user, &request),
        )
        .await
        .map_err(|_| ApiError::unavailable())??;
        let digest = canonical_digest(&request, &scope)?;
        let (original, dispatch) =
            journal::NewEntry::pending(&request, scope, digest, (state.clock)())
                .admit(
                    state,
                    user.id,
                    match origin {
                        RequestOrigin::Agent(admission) => Some(admission),
                        _ => None,
                    },
                )
                .await?;
        if let Some(receipt) = receipt {
            receipt.pin(state, user, adapter.clone());
            receipt.record(response(state, original.clone()));
        }
        if !dispatch {
            return reconcile(state, user, adapter.as_ref(), original).await;
        }
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            adapter.execute(state, user, &request),
        )
        .await
        .unwrap_or_else(|_| Outcome::unknown());
        let completed = finish(state, user, original, outcome).await?;
        if !legacy {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                adapter.authorize_lookup(
                    state,
                    user,
                    registry::LookupTarget::live(&completed.scope, &completed),
                ),
            )
            .await
            .map_err(|_| ApiError::unavailable().operation(completed.operation_id))??;
        }
        Ok(completed)
    }
}
