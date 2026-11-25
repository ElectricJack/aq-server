//! Registration is trusted Rust composition, never client-driven dispatch.
use super::*;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeMap;

#[async_trait]
pub trait ActionKind: Send + Sync + 'static {
    type Payload: DeserializeOwned + JsonSchema + Send + Sync;
    const KIND: &'static str;
    const VERSION: u32 = 1;
    /// The operation a caller must hold (spec 0006 §4.2). An operation
    /// outside the agent grant vocabulary makes the kind owner-only.
    const GRANT: &'static str;
    /// How the command's scope target is derived from the request.
    const TARGET: aq_command_registry::TargetRule;
    /// Recheck current subject, scope and role on every request, including retry.
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: &Self::Payload,
    ) -> Result<Scope, ApiError>;
    /// Recheck current scope and every returned reference. None is a retired
    /// tombstone: authorize scope-only disclosure or refuse it. Keep this
    /// policy installed for retained kinds even when new admission is disabled.
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&Operation>,
    ) -> Result<(), ApiError>;
    /// Recover evidence for the original operation. An adapter may replay a
    /// durably retained body/key only when its native owner guarantees atomic
    /// idempotent receipt replay; it must never construct a new operation.
    async fn reconcile(
        &self,
        _state: &AppState,
        _user: &User,
        _operation: &Operation,
    ) -> Result<Option<Outcome>, ApiError> {
        Ok(None)
    }
    /// Called only by the successful admitting request. Domain reconciliation
    /// must use the original operation ID; it must never infer failure from IO.
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: Self::Payload,
    ) -> Outcome;
}
/// A retired operation still has a scope to authorize even when its response has expired.
pub(super) struct LookupTarget<'a> {
    pub scope: &'a Scope,
    pub operation: Option<&'a Operation>,
}
impl<'a> LookupTarget<'a> {
    pub fn live(scope: &'a Scope, operation: &'a Operation) -> Self {
        Self {
            scope,
            operation: Some(operation),
        }
    }
}
impl journal::Lookup {
    pub(super) fn target(&self) -> LookupTarget<'_> {
        LookupTarget {
            scope: &self.scope,
            operation: self.operation.as_ref(),
        }
    }
}
#[async_trait]
pub(super) trait Adapter: Send + Sync {
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
    ) -> Result<Scope, ApiError>;
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        target: LookupTarget<'_>,
    ) -> Result<(), ApiError>;
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        operation: &Operation,
    ) -> Result<Option<Outcome>, ApiError>;
    async fn execute(&self, state: &AppState, user: &User, request: &ActionRequest) -> Outcome;
}
struct Typed<T>(T);
#[async_trait]
impl<T: ActionKind> Adapter for Typed<T> {
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
    ) -> Result<Scope, ApiError> {
        let payload = serde_json::from_value::<T::Payload>(request.payload.clone())
            .map_err(|_| ApiError::invalid())?;
        self.0.authorize(state, user, request, &payload).await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        target: LookupTarget<'_>,
    ) -> Result<(), ApiError> {
        self.0
            .authorize_lookup(state, user, target.scope, target.operation)
            .await
    }
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        operation: &Operation,
    ) -> Result<Option<Outcome>, ApiError> {
        self.0.reconcile(state, user, operation).await
    }
    async fn execute(&self, state: &AppState, user: &User, request: &ActionRequest) -> Outcome {
        match serde_json::from_value::<T::Payload>(request.payload.clone()) {
            Ok(payload) => self.0.execute(state, user, request, payload).await,
            Err(_) => Outcome::unknown(),
        }
    }
}
#[derive(Default)]
pub struct Registry {
    adapters: BTreeMap<String, Arc<dyn Adapter>>,
    schemas: BTreeMap<String, Value>,
    specs: Vec<aq_command_registry::ActionSpec>,
}
impl Registry {
    /// Payload must be a closed object schema and deny unknown fields in Serde.
    pub fn register<T: ActionKind>(&mut self, kind: T) -> Result<(), ApiError> {
        let supported = valid_kind(T::KIND) && T::VERSION == 1;
        if !supported || self.adapters.contains_key(T::KIND) {
            return Err(ApiError::invalid());
        }
        self.register_schema::<T>()?;
        self.adapters.insert(T::KIND.into(), Arc::new(Typed(kind)));
        Ok(())
    }
    pub(crate) fn register_schema<T: ActionKind>(&mut self) -> Result<(), ApiError> {
        let schema = serde_json::to_value(schemars::schema_for!(T::Payload))
            .map_err(|_| ApiError::invalid())?;
        if schema["type"] != "object" || schema["additionalProperties"] != false {
            return Err(ApiError::invalid());
        }
        self.schemas.insert(T::KIND.into(), schema);
        self.specs.retain(|s| s.kind != T::KIND);
        let (kind, grant, target) = (T::KIND, T::GRANT, T::TARGET);
        self.specs.push(aq_command_registry::ActionSpec {
            kind,
            grant,
            target,
        });
        Ok(())
    }
    pub fn schemas(&self) -> &BTreeMap<String, Value> {
        &self.schemas
    }
    pub fn specs(&self) -> &[aq_command_registry::ActionSpec] {
        &self.specs
    }
    pub(super) fn get(&self, kind: &str) -> Result<&Arc<dyn Adapter>, ApiError> {
        self.adapters.get(kind).ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_action_kind",
                "This action kind is unsupported",
            )
        })
    }
}
pub(super) use super::contract::valid_kind;
