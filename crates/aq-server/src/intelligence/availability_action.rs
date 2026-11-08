//! `swarm.availability.set` (spec 0007 Q2): the owner's sticky availability
//! switch. On records an owner `available` observation for every enabled
//! credential, which the keeper then keeps fresh; off stops new offers.
use crate::AppState;
use crate::dashboard_api::{
    actions::{ActionKind, ActionRequest, Operation, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use crate::error::AppError;
use crate::machine_actions::{generation, lookup, unavailable};
use crate::store::User;
use aq_machine_control as mc;
use aq_selection::Availability;
use async_trait::async_trait;
use axum::http::StatusCode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

const SUBJECT: (&str, &str) = ("swarm", "owner-availability");
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AvailabilityChange {
    pub operation_id: Uuid,
    pub available: bool,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_generation: String,
}
pub(crate) struct SetAvailability;
pub(crate) fn register(r: &mut Registry) {
    r.register(SetAvailability).expect("availability adapter");
}
pub(crate) fn schemas(r: &mut Registry) {
    r.register_schema::<SetAvailability>()
        .expect("availability schema");
}
fn confirmed(operation_id: Uuid, generation: u64) -> Outcome {
    let g = || Some(generation.to_string());
    let receipt = existing::entity("owner_availability", operation_id, g());
    Outcome::confirmed(
        vec![receipt],
        vec![existing::entity(SUBJECT.0, SUBJECT.1, g())],
    )
}
fn other<E>(_: E) -> mc::Error {
    mc::Error::Invalid("availability")
}

#[async_trait]
impl ActionKind for SetAvailability {
    type Payload = AvailabilityChange;
    const KIND: &'static str = "swarm.availability.set";
    const GRANT: &'static str = "swarm.availability";
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &AvailabilityChange,
    ) -> Result<Scope, ApiError> {
        existing::owner(state, user).await?;
        let s = &request.subject;
        if request.operation_id != p.operation_id
            || (s.entity_type.as_str(), s.id.as_str()) != SUBJECT
            || s.revision.is_some()
            || !request.expected_versions.is_empty()
            || generation(&p.expected_generation).is_none()
        {
            return Err(ApiError::invalid());
        }
        if state.config.intelligence.is_none() {
            return Err(ApiError::for_status(StatusCode::CONFLICT));
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
        lookup(state, user, scope, op, SUBJECT.0).await
    }
    async fn reconcile(
        &self,
        state: &AppState,
        _: &User,
        op: &Operation,
    ) -> Result<Option<Outcome>, ApiError> {
        let generation: Option<i64> =
            sqlx::query_scalar("SELECT generation FROM owner_availability WHERE operation_id=$1")
                .bind(op.operation_id)
                .fetch_optional(&state.store.pool)
                .await
                .map_err(unavailable)?;
        Ok(generation.map(|g| confirmed(op.operation_id, g as u64)))
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        _: &ActionRequest,
        p: AvailabilityChange,
    ) -> Outcome {
        let Some(expected) = generation(&p.expected_generation) else {
            return Outcome::unknown();
        };
        let run = async {
            if p.available {
                super::registry::register(state).await.map_err(other)?;
            }
            let mut tx = state.store.pool.begin().await?;
            let (available, actor, now) = (p.available, user.id, (state.clock)());
            let set = mc::set_owner_availability(
                &mut tx,
                available,
                expected,
                actor,
                p.operation_id,
                now,
            )
            .await?;
            if p.available {
                sqlx::query("SELECT pg_advisory_xact_lock(712009)")
                    .execute(&mut *tx)
                    .await?;
                for c in super::loaded(state)
                    .map_err(other)?
                    .credentials
                    .iter()
                    .filter(|c| c.enabled)
                {
                    let scope = super::config::credential_scope(c);
                    let on = Availability::Available;
                    super::record_operator_observation(
                        state, &mut tx, actor, scope, on, 3_600_000, 0, None, None,
                    )
                    .await
                    .map_err(other)?;
                }
            }
            tx.commit().await?;
            Ok::<_, mc::Error>(set.generation)
        };
        match run.await {
            Ok(g) => confirmed(p.operation_id, g),
            Err(mc::Error::Conflict { generation: g }) => Outcome::conflict(BTreeMap::from([(
                "owner_availability".into(),
                g.to_string(),
            )])),
            Err(_) => existing::failure(AppError::Internal),
        }
    }
}
