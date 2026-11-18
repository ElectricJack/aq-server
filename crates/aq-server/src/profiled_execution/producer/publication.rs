//! The only native initial-offer publisher. External reads/exports finish before
//! this transaction. It neither accepts preparation nor issues a grant.
use super::{build::Built, now, observation::StoredOffer, on_heap, refused};
use crate::{
    AppState,
    error::AppError,
    profiled_execution::{artifacts, registration, source_exports},
    source_retention::profiled_base::{RetentionObservation, VerifiedExport},
    worker_api::AuthenticatedNode,
};
use aq_admission::startup::NativeOffer;
use aq_selection::ExecutionTarget;

use crate::error::db;
use uuid::Uuid;
pub async fn publish(
    state: &AppState,
    auth: &AuthenticatedNode,
    built: Built,
    export: &source_exports::ReservedExport,
    fact: &VerifiedExport,
    inspection: &RetentionObservation,
    slot: u32,
) -> Result<Option<NativeOffer>, AppError> {
    let mut digests = built
        .native
        .offered
        .artifacts
        .iter()
        .map(|a| a.artifact.digest.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if digests.len() != built.native.offered.artifacts.len()
        || !digests.insert(fact.manifest_digest().clone())
        || fact.bundle_bytes() == 0
        || !digests.insert(fact.bundle_digest().clone())
    {
        return Err(refused());
    }
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    // Recheck stop/drain/availability after the poll's early gate.
    let mc = |_| refused();
    if !aq_machine_control::lock_publishing_node(&mut tx, auth.id())
        .await
        .map_err(mc)?
        || aq_machine_control::offer_hold(&mut tx, auth.id())
            .await
            .map_err(mc)?
            .is_some()
    {
        return Ok(None);
    }
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    let (node, boot) = match built.decision.inputs.node.target {
        ExecutionTarget::Worker { node_id, boot_id } => (node_id, boot_id),
        _ => return Err(refused()),
    };
    if node != auth.id() {
        return Err(refused());
    }
    let runtime = registration::current_runtime_locked(state, &mut tx, node, boot).await?;
    if runtime.runtime != built.runtime.runtime
        || runtime.qualification != built.runtime.qualification
    {
        return Err(refused());
    }
    // SERIALIZABLE and the common task-claim index arbitrate schedulers.
    let slot_free = super::super::compute::free_locked(&mut tx, node, slot).await?;
    let task_held: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE holds_task AND task_id=$1)",
    )
    .bind(built.spec.task.task_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?
        || super::super::disposition::task_disposed(&mut tx, built.spec.task.task_id).await?;
    if !slot_free {
        // The slot was taken since the poll's early gate: no offer, no refusal.
        aq_machine_control::recent::forget(node).await;
        return Ok(None);
    } else if task_held {
        return Err(refused());
    }
    crate::intelligence::resolver::fence_for_offer(state, &mut tx, &built.decision).await?;
    if let Some(replacement) = &built.decision.inputs.replacement {
        let retained = super::super::replacement::retained_source_locked(
            &mut tx,
            built.spec.task.task_id,
            replacement.predecessor_attempt_id,
            &replacement.retained_source_receipt,
        )
        .await?;
        let shared = retained.token().request_digest() == export.request_digest()
            && retained.fact().canonical_bytes()? == fact.canonical_bytes()?;
        // Spec 0007: a handoff successor's own export of the saved commit.
        if !shared
            && !super::super::handoff::export_is_handoff(
                &mut tx,
                replacement.predecessor_attempt_id,
                export.request(),
            )
            .await?
        {
            return Err(refused());
        }
    }
    let expected_generation=sqlx::query_scalar::<_,Option<i64>>("SELECT max(COALESCE((state#>>'{offer,binding,assignment_generation}')::BIGINT,(state#>>'{offer,attempt,generation}')::BIGINT)) FROM execution_attempts WHERE task_id=$1").bind(built.spec.task.task_id).fetch_one(&mut *tx).await.map_err(db)?.unwrap_or(0).checked_add(1).ok_or_else(|| refused())?;
    let binding = &built.state.offer.binding;
    if expected_generation as u64 != binding.assignment_generation {
        return Err(refused());
    }
    let time = now(state)?;
    let stored: StoredOffer =
        serde_json::from_value(built.central_observation.clone()).map_err(|_| refused())?;
    stored.read_provenance.validate_at(time)?;
    if time >= built.deadline
        || time < built.source.admission.observed_at_ms
        || time >= built.source.admission.expires_at_ms
        || time < inspection.observed_at_ms()
        || time >= inspection.expires_at_ms()
    {
        return Err(refused());
    }
    // Final publication clock is sampled after locks, while absolute original
    // expiry is fixed. No persisted original is later retimed on replay.
    let mut native = built.native;
    native.offered.clock =
        aq_admission::transport::DeliveryClock::new(time, built.deadline).map_err(|_| refused())?;
    native.validate().map_err(|_| refused())?;
    let original = StoredOffer {
        schema_version: 1,
        read_provenance: stored.read_provenance,
        native_offer: native.clone(),
        #[cfg(feature = "delivery_playbook")]
        git: stored.git,
    }
    .value()?;
    let session_id = match binding.session {
        aq_session_protocol::SessionIdentity::Remote { session_id, .. } => session_id,
        _ => return Err(refused()),
    };
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state) VALUES($1,$2,$3,$4,$5,'profiled_v1',$6)").bind(binding.attempt_id).bind(built.spec.task.task_id).bind(node).bind(binding.offer_id).bind(session_id).bind(serde_json::to_value(&built.state).map_err(|_|AppError::Internal)?).execute(&mut *tx).await.map_err(db)?;
    let observation_id = Uuid::new_v5(&binding.attempt_id, b"native-source-admission-v1");
    sqlx::query("INSERT INTO admission_source_observations(observation_id,project_id,source_policy_revision,observation,observed_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6)").bind(observation_id).bind(built.source.admission.project_id).bind(state.config.profiled_execution.as_ref().ok_or_else(|| refused())?.revision as i64).bind(serde_json::to_value(&built.source.admission).map_err(|_|AppError::Internal)?).bind(built.source.admission.observed_at_ms as i64).bind(built.source.admission.expires_at_ms as i64).execute(&mut *tx).await.map_err(db)?;
    sqlx::query("INSERT INTO profiled_launches(attempt_id,decision_id,source_observation_id,launch_binding,launch_manifest,manifest_digest,execution_spec,execution_spec_digest,graph_task_observation,source_binding,offer_expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)").bind(binding.attempt_id).bind(built.decision.decision_id).bind(observation_id).bind(serde_json::to_value(binding).map_err(|_|AppError::Internal)?).bind(serde_json::to_value(&built.state.offer.manifest).map_err(|_|AppError::Internal)?).bind(binding.launch_manifest_digest.as_str()).bind(serde_json::to_value(&built.spec).map_err(|_|AppError::Internal)?).bind(binding.execution_spec_digest.as_str()).bind(original).bind(serde_json::to_value(&built.state.source).map_err(|_|AppError::Internal)?).bind(built.deadline as i64).execute(&mut *tx).await.map_err(db)?;
    let selection = built
        .decision
        .outcome
        .resolved
        .as_ref()
        .ok_or_else(|| refused())?;
    sqlx::query("INSERT INTO intelligence_attempt_selections(attempt_id,selection_digest,selection) VALUES($1,$2,$3)").bind(binding.attempt_id).bind(aq_selection::digest(selection).map_err(|_|AppError::Internal)?.as_str()).bind(serde_json::to_value(selection).map_err(|_|AppError::Internal)?).execute(&mut *tx).await.map_err(db)?;
    // Spec 0014 §7.1: what the attempt ran with is part of its record, and
    // its snapshot stays while the record does.
    if crate::role_settings::tables(state) {
        let ran_with = aq_role_settings::attempts::RanWith {
            role: built.decision.project.role.clone(),
            level: built.decision.project.intelligence_profile.clone(),
            provider: selection.choice.provider.clone(),
            model: selection.choice.model.clone(),
            reasoning: selection.choice.native_reasoning.clone(),
            harness: selection.choice.harness.clone(),
            execution_profile: built.decision.project.execution_profile.clone(),
            snapshot_digest: Some(built.decision.project.snapshot_digest.clone()),
        };
        aq_role_settings::attempts::record(&mut tx, binding.attempt_id, &ran_with)
            .await
            .map_err(crate::role_settings::error)?;
    }
    let hot = crate::execution::authority::hot::HotState::from_compat(&built.state)
        .map_err(|_| refused())?;
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(binding.attempt_id).bind(serde_json::to_value(hot).map_err(|_|AppError::Internal)?).execute(&mut *tx).await.map_err(db)?;
    let ids = built
        .retained
        .entries
        .iter()
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    let linked = artifacts::link_locked(
        &mut tx,
        binding.attempt_id,
        built.source.admission.project_id,
        &ids,
        time,
        built.retained.until,
    )
    .await?;
    let mut linked_bytes = linked
        .iter()
        .map(super::artifacts::canonical)
        .collect::<Result<Vec<_>, _>>()?;
    let mut offered_bytes = native
        .offered
        .artifacts
        .iter()
        .map(super::artifacts::canonical)
        .collect::<Result<Vec<_>, _>>()?;
    linked_bytes.sort();
    offered_bytes.sort();
    if linked_bytes != offered_bytes {
        return Err(refused());
    }
    on_heap(|| {
        source_exports::link_locked(state, &mut tx, &binding.session, export, fact, inspection)
    })
    .await?;
    let m = &built.state.offer.manifest;
    let effective: Vec<_> = m
        .permissions
        .effective
        .iter()
        .map(|p| (p.operation.as_str(), p.resource.as_str()))
        .collect();
    let (project, task) = (m.bindings.project_id, m.bindings.task_id);
    let grants =
        crate::agent_access::grant_check::resolve_native(&mut tx, state, &effective, project, task);
    let grants = grants.await.map_err(db)?;
    on_heap(|| {
        crate::agent_access::native_binding::seal_locked(
            state,
            &mut tx,
            &binding.session,
            &built.graph_profile,
            &built.context,
            &grants,
        )
    })
    .await?;
    let end = now(state)?;
    if end >= built.deadline || end >= inspection.expires_at_ms() {
        return Err(refused());
    }
    super::load_original_offer_locked(&mut tx, binding.attempt_id).await?;
    super::super::closure::history::for_attempt_locked(&mut tx, binding).await?;
    aq_machine_control::slots::assign(&mut tx, binding.attempt_id, node, slot)
        .await
        .map_err(crate::machine_api::mapped)?;
    match tx.commit().await {
        Err(e) if aq_machine_control::is_serialization_failure(&e) => Ok(None),
        done => done.map(|_| Some(native)).map_err(db),
    }
}
