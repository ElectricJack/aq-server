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
use std::sync::Arc;
use uuid::Uuid;

use aq_dashboard_contract::action_contract::playbooks::{
    BINDING, BindingChange, RUN, RunCancel, RunStart, version,
};
pub(crate) struct StartRun(Arc<Driver>);
pub(crate) struct CancelRun(Arc<Driver>);
pub(crate) struct UpdateBinding<const ROLLBACK: bool>(Arc<Driver>);
type Activate = UpdateBinding<false>;
type Rollback = UpdateBinding<true>;

impl<const ROLLBACK: bool> UpdateBinding<ROLLBACK> {
    fn new(driver: Arc<Driver>) -> Self {
        Self(driver)
    }
}

pub(crate) fn register(driver: Arc<Driver>, r: &mut Registry) {
    r.register(StartRun(driver.clone()))
        .expect("playbook start adapter");
    r.register(CancelRun(driver.clone()))
        .expect("playbook cancel adapter");
    r.register(Activate::new(driver.clone()))
        .expect("playbook activate adapter");
    r.register(Rollback::new(driver))
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

/// Dashboard actor and mounted playbook namespace for one authority lookup.
struct ActionOwner<'a> {
    state: &'a AppState,
    driver: &'a Driver,
    user: &'a User,
}
impl<'a> ActionOwner<'a> {
    fn new(state: &'a AppState, driver: &'a Driver, user: &'a User) -> Self {
        Self {
            state,
            driver,
            user,
        }
    }
    async fn require_owner(&self) -> std::result::Result<(), ApiError> {
        existing::owner(self.state, self.user).await?;
        if self.user.id != self.driver.service.namespace.owner_user_id {
            return Err(ApiError::for_status(StatusCode::FORBIDDEN));
        }
        Ok(())
    }
    async fn scope(
        &self,
        request: &ActionRequest,
        operation_id: Uuid,
        project_id: Uuid,
    ) -> std::result::Result<Scope, ApiError> {
        if request.operation_id != operation_id {
            return Err(ApiError::invalid());
        }
        self.require_owner().await?;
        if project_id != self.driver.service.namespace.project_id {
            return Err(ApiError::for_status(StatusCode::NOT_FOUND));
        }
        Ok(Scope::Project { project_id })
    }
    async fn lookup(
        &self,
        scope: &Scope,
        operation: Option<&actions::Operation>,
        kind: &str,
    ) -> std::result::Result<(), ApiError> {
        self.require_owner().await?;
        if !matches!(scope, Scope::Project {project_id} if *project_id == self.driver.service.namespace.project_id)
        {
            return Err(ApiError::invalid());
        }
        if let Some(op) = operation {
            match op.subject_refs.as_slice() {
                [subject] if subject.entity_type == kind => {}
                _ => return Err(ApiError::invalid()),
            }
        }
        Ok(())
    }
}
fn outcome(error: Error) -> Outcome {
    existing::disposition(match error {
        Error::Invalid | Error::Forbidden | Error::DeadlineExpired => Disposition::Rejected,
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
    const GRANT: &'static str = "playbook.run";
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &RunCancel,
    ) -> std::result::Result<Scope, ApiError> {
        if !p.matches(request) {
            return Err(ApiError::invalid());
        }
        ActionOwner::new(state, &self.0, user)
            .scope(request, p.operation_id, p.project_id)
            .await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        ActionOwner::new(state, &self.0, user)
            .lookup(scope, operation, RUN)
            .await
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
    const GRANT: &'static str = "playbook.run";
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &RunStart,
    ) -> std::result::Result<Scope, ApiError> {
        if !p.matches(request) {
            return Err(ApiError::invalid());
        }
        ActionOwner::new(state, &self.0, user)
            .scope(request, p.operation_id, p.project_id)
            .await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        ActionOwner::new(state, &self.0, user)
            .lookup(scope, operation, BINDING)
            .await
    }
    async fn reconcile(
        &self,
        _state: &AppState,
        user: &User,
        operation: &actions::Operation,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        // Replays only the retained exact first admission keyed by this
        // operation. One held with an unknown outcome is looked up, never
        // admitted again: only the owner's explicit retry may run it twice.
        let id = operation.operation_id;
        let held = (self.0.service.admission(user.id, id).await)
            .is_ok_and(|a| a.status.status == "unknown");
        Ok(match self.0.admission(user.id, id, held).await {
            Ok(status) => admitted(&status),
            Err(_) => None,
        })
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
impl<const ROLLBACK: bool> ActionKind for UpdateBinding<ROLLBACK> {
    type Payload = BindingChange;
    const KIND: &'static str = if ROLLBACK {
        "playbook.activation.rollback"
    } else {
        "playbook.activation.activate"
    };
    const GRANT: &'static str = "playbook.activate";
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &BindingChange,
    ) -> std::result::Result<Scope, ApiError> {
        if !p.matches(request) {
            return Err(ApiError::invalid());
        }
        ActionOwner::new(state, &self.0, user)
            .scope(request, p.operation_id, p.project_id)
            .await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        ActionOwner::new(state, &self.0, user)
            .lookup(scope, operation, BINDING)
            .await
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
        let active_artifact = ROLLBACK.then_some(p.artifact.as_str());
        let live = match current(&self.0, &p.binding, expected, active_artifact).await {
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
        let result = if ROLLBACK {
            artifacts::rollback(&catalog, &ledger(&self.0), &op, &p.binding).await
        } else {
            artifacts::activate(&catalog, &ledger(&self.0), &op, &p.binding, &p.artifact).await
        };
        match result {
            Ok(_) => changed(&self.0, &p.binding, &op).await,
            // No recorded revision to roll back to.
            Err(Error::Conflict) if ROLLBACK => rejected(),
            Err(e) => outcome(e),
        }
    }
}
