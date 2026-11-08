//! Owner actions on a service machine (spec 0007 phase 1): `worker.start`,
//! `worker.stop`, `worker.drain` and `worker.revoke`. Control applies to the
//! whole machine; the subject is the swarm read's `SwarmMachine.id`.
use crate::AppState;
use crate::dashboard_api::{
    actions::{ActionKind, ActionRequest, Operation, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use crate::error::AppError;
use crate::store::User;
use aq_machine_control::{self as mc, DesiredState};
use async_trait::async_trait;
use axum::http::StatusCode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, marker::PhantomData};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerControl {
    pub operation_id: Uuid,
    /// SwarmMachine.id of a service machine (a machine key).
    #[schemars(regex(pattern = "^[a-z0-9][a-z0-9-]{0,62}$"))]
    pub machine: String,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_generation: String,
}
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerRevoke {
    pub operation_id: Uuid,
    /// SwarmMachine.id: a machine key, or "node:<uuid>" for a single legacy node.
    #[schemars(regex(
        pattern = "^([a-z0-9][a-z0-9-]{0,62}|node:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$"
    ))]
    pub machine: String,
}
pub(crate) trait Desired: Send + Sync + 'static {
    const KIND: &'static str;
    const DESIRED: DesiredState;
    const HANDOFF: bool;
}
pub(crate) struct Start;
pub(crate) struct Stop;
pub(crate) struct Drain;
impl Desired for Start {
    const KIND: &'static str = "worker.start";
    const DESIRED: DesiredState = DesiredState::Running;
    const HANDOFF: bool = false;
}
impl Desired for Stop {
    const KIND: &'static str = "worker.stop";
    const DESIRED: DesiredState = DesiredState::Stopped;
    const HANDOFF: bool = true;
}
impl Desired for Drain {
    const KIND: &'static str = "worker.drain";
    const DESIRED: DesiredState = DesiredState::Draining;
    const HANDOFF: bool = false;
}
pub(crate) struct SetDesired<D>(PhantomData<D>);
pub(crate) struct RevokeMachine;

pub(crate) fn register(r: &mut Registry) {
    r.register(SetDesired::<Start>(PhantomData))
        .expect("worker control adapter");
    r.register(SetDesired::<Stop>(PhantomData))
        .expect("worker control adapter");
    r.register(SetDesired::<Drain>(PhantomData))
        .expect("worker control adapter");
    r.register(RevokeMachine).expect("worker control adapter");
}
pub(crate) fn schemas(r: &mut Registry) {
    r.register_schema::<SetDesired<Start>>()
        .expect("worker control schema");
    r.register_schema::<SetDesired<Stop>>()
        .expect("worker control schema");
    r.register_schema::<SetDesired<Drain>>()
        .expect("worker control schema");
    r.register_schema::<RevokeMachine>()
        .expect("worker control schema");
}

fn generation(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|g| *g > 0 && *g <= i64::MAX as u64 && g.to_string() == value)
}
/// The request names the machine as its only subject and repeats the payload's operation id.
fn bound(request: &ActionRequest, operation_id: Uuid, machine: &str) -> Result<(), ApiError> {
    if request.operation_id != operation_id
        || request.subject.entity_type != "machine"
        || request.subject.id != machine
        || request.subject.revision.is_some()
        || !request.expected_versions.is_empty()
    {
        return Err(ApiError::invalid());
    }
    Ok(())
}
fn unavailable<E>(_: E) -> ApiError {
    ApiError::unavailable()
}
fn not_found() -> ApiError {
    ApiError::for_status(StatusCode::NOT_FOUND)
}
async fn lookup(
    state: &AppState,
    user: &User,
    scope: &Scope,
    op: Option<&Operation>,
) -> Result<(), ApiError> {
    existing::owner(state, user).await?;
    let one_machine =
        op.is_none_or(|o| o.subject_refs.len() == 1 && o.subject_refs[0].entity_type == "machine");
    if !matches!(scope, Scope::AllAuthorized) || !one_machine {
        return Err(ApiError::invalid());
    }
    Ok(())
}

#[async_trait]
impl<D: Desired> ActionKind for SetDesired<D> {
    type Payload = WorkerControl;
    const KIND: &'static str = D::KIND;
    const GRANT: &'static str = "worker.control";
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &WorkerControl,
    ) -> Result<Scope, ApiError> {
        existing::owner(state, user).await?;
        bound(request, p.operation_id, &p.machine)?;
        generation(&p.expected_generation).ok_or_else(ApiError::invalid)?;
        let mut conn = state.store.pool.acquire().await.map_err(unavailable)?;
        if !mc::machine_exists(&mut conn, &p.machine)
            .await
            .map_err(unavailable)?
        {
            return Err(not_found());
        }
        Ok(Scope::AllAuthorized)
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        op: Option<&Operation>,
    ) -> Result<(), ApiError> {
        lookup(state, user, scope, op).await
    }
    async fn reconcile(
        &self,
        state: &AppState,
        _: &User,
        op: &Operation,
    ) -> Result<Option<Outcome>, ApiError> {
        let key = &op.subject_refs[0].id;
        let generation: Option<i64> = sqlx::query_scalar(
            "SELECT generation FROM machine_controls WHERE machine_key=$1 AND operation_id=$2",
        )
        .bind(key)
        .bind(op.operation_id)
        .fetch_optional(&state.store.pool)
        .await
        .map_err(unavailable)?;
        Ok(generation.map(|g| confirmed(op.operation_id, key, Some(g.to_string()))))
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        _: &ActionRequest,
        p: WorkerControl,
    ) -> Outcome {
        let Some(expected) = generation(&p.expected_generation) else {
            return Outcome::unknown();
        };
        let run = async {
            let now = (state.clock)();
            let mut tx = state.store.pool.begin().await?;
            let c = mc::set_desired(
                &mut tx,
                &p.machine,
                D::DESIRED,
                expected,
                user.id,
                p.operation_id,
                now,
            )
            .await?;
            if D::HANDOFF {
                mc::request_handoffs(&mut tx, &p.machine, now).await?;
            }
            tx.commit().await.map_err(mc::Error::from)?;
            Ok::<_, mc::Error>(c)
        };
        match run.await {
            Ok(c) => confirmed(p.operation_id, &p.machine, Some(c.generation.to_string())),
            Err(mc::Error::Conflict { generation }) => {
                Outcome::conflict(BTreeMap::from([("machine".into(), generation.to_string())]))
            }
            Err(_) => existing::failure(AppError::Internal),
        }
    }
}
fn confirmed(operation_id: Uuid, machine: &str, revision: Option<String>) -> Outcome {
    let (receipt, kind) = if revision.is_some() {
        ("machine_control", "machine")
    } else {
        ("machine_revocation", "machine")
    };
    Outcome::confirmed(
        vec![existing::entity(receipt, operation_id, revision.clone())],
        vec![existing::entity(kind, machine, revision)],
    )
}

#[async_trait]
impl ActionKind for RevokeMachine {
    type Payload = WorkerRevoke;
    const KIND: &'static str = "worker.revoke";
    const GRANT: &'static str = "worker.control";
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &WorkerRevoke,
    ) -> Result<Scope, ApiError> {
        existing::owner(state, user).await?;
        bound(request, p.operation_id, &p.machine)?;
        // Known to the server, live or already revoked: revoking twice stays idempotent.
        let known: bool = match p.machine.strip_prefix("node:") {
            Some(id) => {
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1)")
                    .bind(id.parse::<Uuid>().map_err(|_| ApiError::invalid())?)
                    .fetch_one(&state.store.pool)
                    .await
            }
            None => {
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE machine_key=$1)")
                    .bind(&p.machine)
                    .fetch_one(&state.store.pool)
                    .await
            }
        }
        .map_err(unavailable)?;
        if !known {
            return Err(not_found());
        }
        Ok(Scope::AllAuthorized)
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        op: Option<&Operation>,
    ) -> Result<(), ApiError> {
        lookup(state, user, scope, op).await
    }
    async fn execute(
        &self,
        state: &AppState,
        _: &User,
        _: &ActionRequest,
        p: WorkerRevoke,
    ) -> Outcome {
        let run = async {
            let mut tx = state.store.pool.begin().await?;
            mc::revoke_machine(&mut tx, &p.machine, (state.clock)()).await?;
            tx.commit().await.map_err(mc::Error::from)
        };
        match run.await {
            Ok(()) => confirmed(p.operation_id, &p.machine, None),
            Err(_) => existing::failure(AppError::Internal),
        }
    }
}
