//! Same-boot continuation from retained phase issuance and fresh owning facts.
use super::*;
use crate::delegated_permits::{PermitRecord, handoff, runtime};
use crate::worker_api::AuthenticatedNode;
use aq_admission_sandbox::sandbox::{
    PhaseExecutionAuthority, PhaseProfile, RenewalReply, RunnerRenewalRequest,
    RunnerRenewalResponse,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retained {
    current: runtime::Current,
    provenance: serde_json::Value,
}

async fn load(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    q: &RunnerRenewalRequest,
) -> Result<(pinned::Pinned, Option<Retained>), AppError> {
    crate::worker_api::lock_current_node(tx, auth).await?;
    let installed = &auth.delegated().ok_or_else(refused)?.current;
    if q.permit.permit_id != installed.record.permit_id
        || q.permit.binding != installed.record.binding
        || !same(
            &q.authority,
            &installed.authority.as_ref().ok_or_else(refused)?,
        )?
    {
        return Err(refused());
    }
    installed
        .record
        .verify_credential(q.permit.credential.expose())
        .map_err(|_| refused())?;
    let replay = handoff::replay(tx, auth.id(), q.operation_id, "renewal", q)
        .await?
        .map(runtime::parse::<Retained>)
        .transpose()?;
    if let Some(replay) = &replay {
        if !same(installed, &replay.current)? {
            return Err(refused());
        }
    } else if q.revision != installed.revision
        || !same(
            &PermitRecord::from_issued(&q.permit).map_err(|_| refused())?,
            &installed.record,
        )?
    {
        return Err(refused());
    }
    let origin = history::load_optional_locked(
        tx,
        auth.id(),
        &q.authority.grant.binding,
        q.authority.grant.grant_id,
    )
    .await?
    .ok_or_else(refused)?;
    let authority = match q.authority.profile {
        PhaseProfile::PhaseV2 => PhaseExecutionAuthority::from_phase_v2(&runtime::parse(
            runtime::json(&origin.receipt)?,
        )?),
        PhaseProfile::PhaseRefreshV1 => PhaseExecutionAuthority::from_phase_refresh(
            &runtime::parse(runtime::json(&origin.receipt)?)?,
        ),
    }
    .map_err(|_| refused())?;
    if !same(&authority, &q.authority)? {
        return Err(refused());
    }
    // Preserve the issuance-time registry floor as well as the accepted floor.
    super::validate_registry_replay(state, tx, &origin).await?;
    let pinned = pinned::load_continuing_locked(state, tx, auth, &origin).await?;
    Ok((pinned, replay))
}

pub async fn renew_delegated(
    state: &AppState,
    auth: &AuthenticatedNode,
    q: &RunnerRenewalRequest,
    nonce: Uuid,
) -> Result<RunnerRenewalResponse, AppError> {
    q.validate().map_err(|_| refused())?;
    if nonce.is_nil() {
        return Err(refused());
    }
    // This lane is independent of initial preparation and its bulk material work.
    static REQUESTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    let _slot = REQUESTS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(4)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| refused())?;
    let mut tx = begin(state).await?;
    let (before, _) = load(state, &mut tx, auth, q).await?;
    tx.commit().await.map_err(db)?;
    let observed = observations::collect_continuing(state, &before.input).await?;
    let mut tx = begin(state).await?;
    let (mut after, replay) = load(state, &mut tx, auth, q).await?;
    if !same(&before.history.proposal, &after.history.proposal)?
        || !same(&before.history.receipt, &after.history.receipt)?
        || before.qualification != after.qualification
        || before.registration_id != after.registration_id
        || before.input.source.fact().canonical_bytes()?
            != after.input.source.fact().canonical_bytes()?
    {
        return Err(refused());
    }
    validate_registry_loads(&before, &after)?;
    observed.validate_for(&after.input, now(state)?)?;
    if observed.binding.project.revision != after.input.spec.project_revision
        || observed.binding.node_id != auth.id()
        || !same(
            &observed.binding.project.binding,
            &after.input.decision.project.project.binding,
        )?
    {
        return Err(refused());
    }
    crate::project_bindings::fence_for_offer(state, &mut tx, &observed.binding).await?;
    let current = current::fence_locked(
        state,
        &mut tx,
        &current::Input {
            proposal: &after.history.proposal,
            selection: &after.input.selection,
            decision: &after.input.decision,
            policy: &after.policy,
            material: &after.input.material,
            registry_at_preflight: &before.registry_at_load,
        },
    )
    .await?;
    let old = &auth.delegated().ok_or_else(refused)?.current;
    let previous_expiry = after.hot.grant.as_ref().ok_or_else(refused)?.expires_at_ms;
    if previous_expiry != old.record.expires_at_ms {
        return Err(refused());
    }
    let retained = if let Some(replay) = replay {
        replay
    } else {
        let time = now(state)?;
        observed.validate_for(&after.input, time)?;
        current.fresh_at(time)?;
        // As at phase issuance, fresh facts authorize a bounded execution lease.
        // Their observation timestamps are never rewritten. The original hard
        // deadline and the negotiated maximum lease remain absolute ceilings.
        let expiry = time
            .checked_add(
                after
                    .history
                    .proposal
                    .policy
                    .maximum_execution_lease_ms
                    .min(120_000),
            )
            .ok_or_else(refused)?
            .min(q.authority.hard_deadline_ms);
        if time >= previous_expiry || expiry <= previous_expiry {
            return Err(refused());
        }
        let mut next = old.clone();
        next.record.issued_at_ms = time;
        next.record.expires_at_ms = expiry;
        next.revision = next.revision.checked_add(1).ok_or_else(refused)?;
        after.hot.grant.as_mut().ok_or_else(refused)?.expires_at_ms = expiry;
        after
            .hot
            .validate_against(&q.authority.grant.binding)
            .map_err(|_| refused())?;
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(q.permit.binding.attempt_id)
            .bind(runtime::json(&after.hot)?)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        runtime::save(&mut tx, &next).await?;
        next.record
            .authorize(
                &runtime::context(&mut tx, &next).await?,
                aq_admission_sandbox::sandbox::RunnerOperation::Renew,
                time,
            )
            .map_err(|_| refused())?;
        let retained = Retained {
            current: next,
            provenance: runtime::json(&make_provenance(&after, &observed, &current))?,
        };
        // Bounded replay history shares a node lock with issuance and is capped
        // globally under a separate short-lived quota lock.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('aq-delegated-history-quota',0))",
        )
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        let (total, local): (i64, i64) = sqlx::query_as("SELECT COALESCE(sum(octet_length(evidence::text)+512),0)::BIGINT,COALESCE(sum(octet_length(evidence::text)+512) FILTER(WHERE node_id=$1),0)::BIGINT FROM delegated_operations")
            .bind(auth.id()).fetch_one(&mut *tx).await.map_err(db)?;
        let incoming = serde_json::to_vec(&retained).map_err(|_| refused())?.len() as i64 + 512;
        if total + incoming > 67_108_864 || local + incoming > 4_194_304 {
            return Err(refused());
        }
        handoff::remember(&mut tx, auth.id(), q.operation_id, "renewal", q, &retained).await?;
        retained
    };
    let time = now(state)?;
    observed.validate_for(&after.input, time)?;
    current.fresh_at(time)?;
    if time >= previous_expiry || time >= after.issuance_ceiling {
        return Err(refused());
    }
    let permit = retained
        .current
        .record
        .delivery(runtime::key(state)?)
        .map_err(|_| refused())?;
    let reply = RunnerRenewalResponse {
        schema_version: 1,
        revision: retained.current.revision,
        delivery: Some(aq_admission_sandbox::control::DeliveryStamp {
            nonce,
            clock: aq_admission_sandbox::transport::DeliveryClock::new_interactive_session(
                time,
                permit.expires_at_ms,
            )
            .map_err(|_| refused())?,
        }),
        reply: RenewalReply::Renewed {
            operation_id: q.operation_id,
            permit,
        },
    };
    reply.validate().map_err(|_| refused())?;
    tx.commit().await.map_err(|_| {
        AppError::UpdatesUnavailable(
            "renewal commit outcome unknown; preserve exact operation intent".into(),
        )
    })?;
    Ok(reply)
}
