//! Atomic same-grant renewal. All filesystem/Git/Graph reads precede SQL locks.
use super::{
    current,
    history::{self, Body, Stored},
    now, observations, pinned,
};
use crate::{AppState, error::AppError, worker_api::AuthenticatedNode};
use aq_admission::{
    control::{RenewedAuthority, Versions},
    transport::{Renew, TransportDocument},
};
use sqlx::Row;
use uuid::Uuid;
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("continuing permit expired, changed, or cannot be extended".into())
}
fn unavailable() -> AppError {
    AppError::UpdatesUnavailable("continuing authority temporarily unavailable".into())
}
async fn begin(state: &AppState) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(|_| unavailable())?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(|_| unavailable())?;
    Ok(tx)
}
pub async fn renew(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Renew,
    nonce: Uuid,
) -> Result<RenewedAuthority, AppError> {
    TransportDocument::Renew(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid renewal request".into()))?;
    if nonce.is_nil() {
        return Err(refused());
    }
    let mut tx = begin(state).await?;
    let before = pinned::load_locked(state, &mut tx, auth, request).await?;
    // Changed-content operations fail before any external observation work.
    history::lookup(&mut tx, auth.id(), request).await?;
    tx.commit().await.map_err(|_| unavailable())?;
    let observed = observations::collect(state, &before).await?;
    let mut tx = begin(state).await?;
    let mut after = pinned::load_locked(state, &mut tx, auth, request).await?;
    if !pinned::same_immutable(&before, &after)? {
        return Err(refused());
    }
    // Closure owns this terminal launch fence. Never bypass it on replay.
    super::super::closure::ensure_launch_open_locked(&mut tx, &request.binding).await?;
    let facts = current::fence_locked(state, &mut tx, request, &after, &observed.binding).await?;
    if observed.file.digest != after.material.material_digest
        || observed.file.bytes != after.material.material_bytes
    {
        return Err(refused());
    }
    let previous_expiry = after
        .hot
        .grant
        .as_ref()
        .ok_or_else(|| refused())?
        .expires_at_ms;
    let hard = after.original.execution_deadline_ms;
    let ceiling = hard
        .min(observed.ceiling()?)
        .min(facts.inventory_expires_at_ms);
    let stored = if let Some(stored) = history::lookup(&mut tx, auth.id(), request).await? {
        if stored.body.expires_at_ms != previous_expiry
            || stored.body.versions.grant_revision != after.versions.grant_revision
            || stored.body.versions.record_revision > after.versions.record_revision
            || stored.body.versions.authority_revision > after.versions.authority_revision
            || stored.body.execution_deadline_ms != hard
        {
            return Err(refused());
        }
        if stored.body.expires_at_ms > ceiling {
            return Err(refused());
        }
        stored
    } else {
        let row=sqlx::query("SELECT revision,binding,grant_id FROM profiled_owner_journals WHERE attempt_id=$1 FOR SHARE").bind(request.binding.attempt_id).fetch_optional(&mut *tx).await.map_err(|_|AppError::Internal)?.ok_or_else(|| refused())?;
        if row.get::<serde_json::Value, _>("binding")
            != serde_json::to_value(&request.binding).map_err(|_| AppError::Internal)?
            || row.get::<Option<Uuid>, _>("grant_id") != Some(request.grant_id)
        {
            return Err(refused());
        }
        let physical = u64::try_from(row.get::<i64, _>("revision")).map_err(|_| refused())?;
        let latest = super::latest_renewal_revision_locked(&mut tx, auth.id(), &request.binding)
            .await?
            .unwrap_or(0)
            .max(physical);
        if request.journal_revision <= latest {
            return Err(refused());
        }
        let issued = now(state)?;
        observed.fresh_at(issued)?;
        let expiry = issued
            .checked_add(10_000)
            .ok_or_else(|| refused())?
            .min(ceiling);
        if issued >= previous_expiry || expiry <= previous_expiry {
            return Err(refused());
        }
        after
            .hot
            .grant
            .as_mut()
            .ok_or_else(|| refused())?
            .expires_at_ms = expiry;
        after
            .hot
            .validate_against(&request.binding)
            .map_err(|_| refused())?;
        let row=sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1 RETURNING record_revision,authority_revision,grant_revision").bind(request.binding.attempt_id).bind(serde_json::to_value(&after.hot).map_err(|_|AppError::Internal)?).fetch_one(&mut *tx).await.map_err(|_|AppError::Internal)?;
        let versions = Versions {
            record_revision: row.get::<i64, _>("record_revision") as u64,
            authority_revision: row.get::<i64, _>("authority_revision") as u64,
            grant_revision: row.get::<i64, _>("grant_revision") as u64,
        };
        if versions.grant_revision <= after.versions.grant_revision {
            return Err(refused());
        }
        let stored = Stored::new(Body {
            operation_id: request.operation_id,
            binding: request.binding.clone(),
            grant_id: request.grant_id,
            reservation_id: request.reservation_id,
            guardian_id: request.guardian_id,
            launch_id: request.launch_id,
            journal_revision: request.journal_revision,
            execution_deadline_ms: hard,
            issued_at_ms: issued,
            expires_at_ms: expiry,
            versions,
            health: facts.health,
            registry_revision: facts.registry_revision,
            inventory_revision: facts.inventory_revision,
            binding_observation_digest: aq_selection::digest(&observed.binding.binding_observation)
                .map_err(|_| AppError::Internal)?,
            task_observation_digest: observed.task.response_digest.clone(),
            source_observation_digest: aq_selection::digest(&observed.source)
                .map_err(|_| AppError::Internal)?,
            material_observed_at_ms: observed.file_received_at_ms,
            task_observed_at_ms: observed.task.observed_at_ms,
            task_received_at_ms: observed.task.received_at_ms,
            task_expires_at_ms: observed.task.expires_at_ms,
            source_observed_at_ms: observed.source.observed_at_ms(),
            source_expires_at_ms: observed.source.expires_at_ms(),
            binding_observed_at_ms: observed.binding.binding_observation.observed_at_ms,
            binding_expires_at_ms: observed.binding.binding_observation.expires_at_ms,
            inventory_expires_at_ms: facts.inventory_expires_at_ms,
        })?;
        let value = history::reserve_quota(&mut tx, auth.id(), &request.binding, &stored).await?;
        history::insert(&mut tx, auth.id(), request, value).await?;
        stored
    };
    // Includes quota-lock/trigger/write waits. The old permit, current facts and
    // new permit must all still be live; rollback cannot resurrect an expired one.
    let time = now(state)?;
    observed.fresh_at(time)?;
    if time >= previous_expiry || time >= ceiling {
        return Err(refused());
    }
    let reply = stored.deliver(nonce, time)?;
    tx.commit().await.map_err(|_| unavailable())?;
    Ok(reply)
}
