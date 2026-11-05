//! Dashboard playbook actions over the mounted D9 owner driver: run start and
//! cancel, binding activate and rollback. Only the mounted runtime namespace
//! owner may use them. A binding's expected version is its current engine
//! activation revision. Activation authority is the grant named by that
//! activation, which must still be live.
use super::{
    artifacts::{self, DriverCatalog, Ledger, PgLedger},
    domain::{Error, grant_actor},
    driver::Driver,
    service::PrepareRequest,
};
use crate::{
    AppState,
    dashboard_api::{
        actions::{self, ActionKind, ActionRequest, Disposition, Outcome, Registry, existing},
        contract::Scope,
        errors::ApiError,
    },
    store::User,
};
use async_trait::async_trait;
use axum::http::StatusCode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;

const RUN: &str = "playbook_run";
const BINDING: &str = "playbook_binding";

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunCancel {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(length(min = 1, max = 128))]
    pub run_id: String,
}

/// One scalar run input.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum Input {
    Bool(bool),
    Number(serde_json::Number),
    Text(#[schemars(length(max = 4096))] String),
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunStart {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    /// The binding's activation revision, as shown in the catalog.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,19}$"))]
    pub expected_version: String,
    #[schemars(length(min = 1, max = 128))]
    pub binding: String,
    /// The binding's currently active artifact digest.
    #[schemars(length(min = 1, max = 128))]
    pub artifact: String,
    #[schemars(length(min = 1, max = 128))]
    pub entry: String,
    #[serde(default)]
    #[schemars(extend("maxProperties" = 32))]
    pub inputs: BTreeMap<String, Input>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingChange {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    /// The binding's activation revision, as shown in the catalog.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,19}$"))]
    pub expected_version: String,
    #[schemars(length(min = 1, max = 128))]
    pub binding: String,
    /// Activate: the artifact to make active. Rollback: the active artifact.
    #[schemars(length(min = 1, max = 128))]
    pub artifact: String,
}

pub(crate) struct StartRun(Arc<Driver>);
pub(crate) struct CancelRun(Arc<Driver>);
pub(crate) struct Activate(Arc<Driver>);
pub(crate) struct Rollback(Arc<Driver>);

pub(crate) fn register(driver: Arc<Driver>, r: &mut Registry) {
    r.register(StartRun(driver.clone()))
        .expect("playbook start adapter");
    r.register(CancelRun(driver.clone()))
        .expect("playbook cancel adapter");
    r.register(Activate(driver.clone()))
        .expect("playbook activate adapter");
    r.register(Rollback(driver))
        .expect("playbook rollback adapter");
}
pub(crate) fn schemas(r: &mut Registry) {
    r.register_schema::<StartRun>()
        .expect("playbook start schema");
    r.register_schema::<CancelRun>()
        .expect("playbook cancel schema");
    r.register_schema::<Activate>()
        .expect("playbook activate schema");
    r.register_schema::<Rollback>()
        .expect("playbook rollback schema");
}

fn id_ok(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}
fn version(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0 && v.to_string() == value)
}
/// Owner of the mounted namespace, in the namespace's project.
async fn owner(
    state: &AppState,
    driver: &Driver,
    user: &User,
    request: &ActionRequest,
    operation_id: Uuid,
    project_id: Uuid,
) -> std::result::Result<Scope, ApiError> {
    if request.operation_id != operation_id {
        return Err(ApiError::invalid());
    }
    existing::owner(state, user).await?;
    let ns = &driver.service.namespace;
    if user.id != ns.owner_user_id {
        return Err(ApiError::for_status(StatusCode::FORBIDDEN));
    }
    if project_id != ns.project_id {
        return Err(ApiError::for_status(StatusCode::NOT_FOUND));
    }
    Ok(Scope::Project { project_id })
}
async fn lookup(
    state: &AppState,
    driver: &Driver,
    user: &User,
    scope: &Scope,
    operation: Option<&actions::Operation>,
    kind: &str,
) -> std::result::Result<(), ApiError> {
    existing::owner(state, user).await?;
    let ns = &driver.service.namespace;
    if user.id != ns.owner_user_id {
        return Err(ApiError::for_status(StatusCode::FORBIDDEN));
    }
    if !matches!(scope, Scope::Project { project_id } if *project_id == ns.project_id) {
        return Err(ApiError::invalid());
    }
    if let Some(op) = operation
        && (op.subject_refs.len() != 1 || op.subject_refs[0].entity_type != kind)
    {
        return Err(ApiError::invalid());
    }
    Ok(())
}
/// Versioned binding subject: `playbook_binding@expected_version`.
fn binding_subject(
    request: &ActionRequest,
    binding: &str,
    expected: &str,
) -> std::result::Result<u64, ApiError> {
    let v = version(expected).ok_or_else(ApiError::invalid)?;
    if !id_ok(binding)
        || request.subject.entity_type != BINDING
        || request.subject.id != binding
        || request.subject.revision.as_deref() != Some(expected)
        || request.expected_versions != BTreeMap::from([(BINDING.into(), expected.into())])
    {
        return Err(ApiError::invalid());
    }
    Ok(v)
}
fn outcome(error: Error) -> Outcome {
    existing::disposition(match error {
        Error::Invalid | Error::Forbidden => Disposition::Rejected,
        Error::Conflict | Error::Fenced => Disposition::Conflict,
        Error::Overloaded | Error::Unknown => Disposition::Unknown,
    })
}
fn rejected() -> Outcome {
    existing::disposition(Disposition::Rejected)
}
/// The live activation when it is at `expected` and running `artifact`
/// (when given); otherwise the conflict outcome.
async fn current(
    driver: &Driver,
    binding: &str,
    expected: u64,
    artifact: Option<&str>,
) -> std::result::Result<aq_playbook_host::runner::Activation, Outcome> {
    let Some(live) = driver
        .binding_activation(binding.into())
        .await
        .map_err(outcome)?
    else {
        return Err(rejected());
    };
    if live.revision != expected || artifact.is_some_and(|a| a != live.artifact) {
        return Err(Outcome::conflict(
            [(BINDING.to_string(), live.revision.to_string())].into(),
        ));
    }
    Ok(live)
}
fn ledger(driver: &Driver) -> PgLedger {
    PgLedger {
        pool: driver.service.pool.clone(),
        namespace_id: driver.service.namespace.namespace_id,
    }
}
async fn changed(driver: &Driver, binding: &str, op: &str) -> Outcome {
    let revision = driver
        .binding_activation(binding.into())
        .await
        .ok()
        .flatten()
        .map(|a| a.revision.to_string());
    Outcome::confirmed(
        vec![existing::entity("playbook_activation_operation", op, None)],
        vec![existing::entity(BINDING, binding, revision)],
    )
}
async fn recorded(
    driver: &Driver,
    operation: &actions::Operation,
) -> std::result::Result<Option<Outcome>, ApiError> {
    let op = operation.operation_id.to_string();
    let record = ledger(driver)
        .get(&op)
        .await
        .map_err(|_| ApiError::unavailable())?;
    Ok(record.map(|r| {
        Outcome::confirmed(
            vec![existing::entity("playbook_activation_operation", op, None)],
            vec![existing::entity(BINDING, r.binding, None)],
        )
    }))
}
fn admitted(status: &super::service::AdmissionStatus) -> Option<Outcome> {
    let receipt = status.receipt.as_ref()?;
    Some(Outcome::confirmed(
        vec![existing::entity(
            "playbook_admission",
            status.request_id,
            None,
        )],
        vec![existing::entity(RUN, &receipt.run_id, None)],
    ))
}

#[async_trait]
impl ActionKind for CancelRun {
    type Payload = RunCancel;
    const KIND: &'static str = "playbook.run.cancel";
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &RunCancel,
    ) -> std::result::Result<Scope, ApiError> {
        if !id_ok(&p.run_id)
            || request.subject.entity_type != RUN
            || request.subject.id != p.run_id
            || request.subject.revision.is_some()
            || !request.expected_versions.is_empty()
        {
            return Err(ApiError::invalid());
        }
        owner(state, &self.0, user, request, p.operation_id, p.project_id).await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        lookup(state, &self.0, user, scope, operation, RUN).await
    }
    async fn execute(
        &self,
        _state: &AppState,
        _user: &User,
        _request: &ActionRequest,
        p: RunCancel,
    ) -> Outcome {
        // The engine's cancel is idempotent per run.
        match self.0.cancel_run(p.run_id.clone()).await {
            Ok(()) => Outcome::confirmed(vec![], vec![existing::entity(RUN, p.run_id, None)]),
            Err(e) => outcome(e),
        }
    }
}

#[async_trait]
impl ActionKind for StartRun {
    type Payload = RunStart;
    const KIND: &'static str = "playbook.run.start";
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &RunStart,
    ) -> std::result::Result<Scope, ApiError> {
        binding_subject(request, &p.binding, &p.expected_version)?;
        if !id_ok(&p.artifact)
            || !id_ok(&p.entry)
            || p.inputs.len() > 32
            || p.inputs.keys().any(|k| !id_ok(k))
        {
            return Err(ApiError::invalid());
        }
        owner(state, &self.0, user, request, p.operation_id, p.project_id).await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        lookup(state, &self.0, user, scope, operation, BINDING).await
    }
    async fn reconcile(
        &self,
        _state: &AppState,
        user: &User,
        operation: &actions::Operation,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        // Replays only the retained exact first admission keyed by this
        // operation; the engine admits it at most once.
        Ok(
            match self
                .0
                .admission(user.id, operation.operation_id, false)
                .await
            {
                Ok(status) => admitted(&status),
                Err(_) => None,
            },
        )
    }
    async fn execute(
        &self,
        _state: &AppState,
        user: &User,
        _request: &ActionRequest,
        p: RunStart,
    ) -> Outcome {
        let Some(expected) = version(&p.expected_version) else {
            return rejected();
        };
        let live = match current(&self.0, &p.binding, expected, Some(&p.artifact)).await {
            Ok(live) => live,
            Err(outcome) => return outcome,
        };
        let Ok((grant_id, grant_revision)) = grant_actor(&live.authority) else {
            return rejected();
        };
        // No typed inputs is no input, not an empty object.
        let input = if p.inputs.is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::to_value(&p.inputs) {
                Ok(v) => v,
                Err(_) => return rejected(),
            }
        };
        // The engine refuses the admission if the activation moved since.
        let prepared = self
            .0
            .service
            .prepare(
                user.id,
                PrepareRequest {
                    request_id: p.operation_id,
                    grant_id,
                    grant_revision,
                    binding: p.binding,
                    expected_activation_revision: expected,
                    entry: p.entry,
                    input,
                },
            )
            .await;
        if let Err(e) = prepared {
            return outcome(e);
        }
        match self.0.admission(user.id, p.operation_id, false).await {
            // Prepared durably; the driver admits it on a later quantum and a
            // lookup reconciles the receipt.
            Ok(status) => admitted(&status).unwrap_or(Outcome {
                disposition: Disposition::Pending,
                ..Outcome::unknown()
            }),
            Err(e) => outcome(e),
        }
    }
}

#[async_trait]
impl ActionKind for Activate {
    type Payload = BindingChange;
    const KIND: &'static str = "playbook.activation.activate";
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &BindingChange,
    ) -> std::result::Result<Scope, ApiError> {
        binding_subject(request, &p.binding, &p.expected_version)?;
        if !id_ok(&p.artifact) {
            return Err(ApiError::invalid());
        }
        owner(state, &self.0, user, request, p.operation_id, p.project_id).await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        lookup(state, &self.0, user, scope, operation, BINDING).await
    }
    async fn reconcile(
        &self,
        _state: &AppState,
        _user: &User,
        operation: &actions::Operation,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        recorded(&self.0, operation).await
    }
    async fn execute(
        &self,
        _state: &AppState,
        _user: &User,
        _request: &ActionRequest,
        p: BindingChange,
    ) -> Outcome {
        let Some(expected) = version(&p.expected_version) else {
            return rejected();
        };
        let live = match current(&self.0, &p.binding, expected, None).await {
            Ok(live) => live,
            Err(outcome) => return outcome,
        };
        let Ok((grant, revision)) = grant_actor(&live.authority) else {
            return rejected();
        };
        let catalog = DriverCatalog {
            driver: &self.0,
            grant,
            revision,
        };
        let op = p.operation_id.to_string();
        match artifacts::activate(&catalog, &ledger(&self.0), &op, &p.binding, &p.artifact).await {
            Ok(_) => changed(&self.0, &p.binding, &op).await,
            Err(e) => outcome(e),
        }
    }
}

#[async_trait]
impl ActionKind for Rollback {
    type Payload = BindingChange;
    const KIND: &'static str = "playbook.activation.rollback";
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &BindingChange,
    ) -> std::result::Result<Scope, ApiError> {
        binding_subject(request, &p.binding, &p.expected_version)?;
        if !id_ok(&p.artifact) {
            return Err(ApiError::invalid());
        }
        owner(state, &self.0, user, request, p.operation_id, p.project_id).await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        lookup(state, &self.0, user, scope, operation, BINDING).await
    }
    async fn reconcile(
        &self,
        _state: &AppState,
        _user: &User,
        operation: &actions::Operation,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        recorded(&self.0, operation).await
    }
    async fn execute(
        &self,
        _state: &AppState,
        _user: &User,
        _request: &ActionRequest,
        p: BindingChange,
    ) -> Outcome {
        let Some(expected) = version(&p.expected_version) else {
            return rejected();
        };
        let live = match current(&self.0, &p.binding, expected, Some(&p.artifact)).await {
            Ok(live) => live,
            Err(outcome) => return outcome,
        };
        let Ok((grant, revision)) = grant_actor(&live.authority) else {
            return rejected();
        };
        let catalog = DriverCatalog {
            driver: &self.0,
            grant,
            revision,
        };
        let op = p.operation_id.to_string();
        match artifacts::rollback(&catalog, &ledger(&self.0), &op, &p.binding).await {
            Ok(_) => changed(&self.0, &p.binding, &op).await,
            // Nothing recorded to roll back to.
            Err(Error::Conflict) => rejected(),
            Err(e) => outcome(e),
        }
    }
}
