//! Bounded immutable input owner for a fresh phase execution confirmation.
use super::accepted_owner::{self, AcceptedHistory};
use super::phase::PrepareRequest;
use super::*;
use crate::{
    execution::{
        authority::{self, Policy, hot::HotState},
        profiled::Status,
    },
    profiled_execution::{artifacts, registration, source_exports},
    worker_api::AuthenticatedNode,
};
use aq_admission::{RetainedArtifact, control::Versions, transport::ArtifactReceipt};
use aq_profiled_state::phase_execution as support;
use sqlx::Row;
use uuid::Uuid;
pub(super) struct Pinned {
    pub history: AcceptedHistory,
    pub input: observations::Input,
    pub policy: aq_profiles::SelectionPolicy,
    pub capabilities: Vec<aq_session_protocol::Capability>,
    pub hot: HotState,
    pub versions: Versions,
    pub registration_id: Uuid,
    pub qualification: ArtifactRef,
    pub issuance_ceiling: u64,
    pub registry_at_load: super::RegistrySnapshot,
}
// Preparation, immutable replay, and delegated continuation have distinct
// grant checks. Only continuation permits the live grant expiry to advance.
#[derive(Clone, Copy)]
enum LoadMode<'a> {
    Fresh,
    Replay(&'a history::PhaseExecutionHistory),
    Continuing(&'a history::PhaseExecutionHistory),
}
impl<'a> LoadMode<'a> {
    fn issued(self) -> Option<&'a history::PhaseExecutionHistory> {
        match self {
            Self::Fresh => None,
            Self::Replay(issued) | Self::Continuing(issued) => Some(issued),
        }
    }
}

pub(super) async fn load_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    q: &PrepareRequest,
) -> Result<Pinned, AppError> {
    Box::pin(load_for_locked(state, tx, auth, q, LoadMode::Fresh)).await
}

/// Only the actual immutable issued-history owner supplies this receipt. This
/// mode verifies existing execution authority without reviving preparation or
/// private retrieval permission after either original preparation expiry.
#[allow(dead_code)] // Frozen profile deliberately has no current-replay preflight.
pub(super) async fn load_replay_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    q: &PrepareRequest,
    issued: &history::PhaseExecutionHistory,
) -> Result<Pinned, AppError> {
    Box::pin(load_for_locked(
        state,
        tx,
        auth,
        q,
        LoadMode::Replay(issued),
    ))
    .await
}

/// Continuing permission is authenticated against the live delegated record.
/// Only its expiry may advance beyond the immutable phase issuance receipt.
pub(super) async fn load_continuing_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    issued: &history::PhaseExecutionHistory,
) -> Result<Pinned, AppError> {
    if auth.delegated().is_none() {
        return Err(refused());
    }
    Box::pin(load_for_locked(
        state,
        tx,
        auth,
        &issued.request,
        LoadMode::Continuing(issued),
    ))
    .await
}

async fn load_for_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    q: &PrepareRequest,
    mode: LoadMode<'_>,
) -> Result<Pinned, AppError> {
    // Kept as one body on purpose: unoptimized builds give every nested async
    // helper its own poll frame (boxing does not remove the caller's
    // temporary), which overflowed the stack of deep database test callers.
    // Authenticate preparation or retained issuance before consulting current
    // runtime and authority, all under the same authenticated node lock.
    use aq_session_protocol::Validate;
    q.original.binding.validate().map_err(|_| refused())?;
    let session_matches = matches!(
        q.original.binding.session,
        aq_admission::SessionIdentity::Remote { node_id, boot_id, .. }
            if node_id == auth.id() && boot_id == q.original.identity.boot_id
    );
    if auth.id() != q.original.identity.node_id || !session_matches {
        return Err(refused());
    }
    crate::worker_api::lock_current_node(tx, auth).await?;
    let h = accepted_owner::load_accepted_history_locked(tx, &q.original.binding).await?;
    q.validate_for(&h.receipt, &h.request, &h.proposal)
        .map_err(|_| refused())?;
    let issued = mode.issued();
    if let Some(issued) = issued {
        if !same(&issued.request, q)?
            || !same(&issued.accepted.receipt, &h.receipt)?
            || !same(&issued.accepted.request, &h.request)?
            || !same(&issued.accepted.proposal, &h.proposal)?
        {
            return Err(refused());
        }
        // This validates the same actual current material row and policy. No
        // lease bytes, expiry, operation or installation receipt are rewritten.
        accepted_owner::require_installed_material_locked(state, auth, tx, &h).await?;
    } else {
        accepted_owner::require_live_accepted_locked(state, tx, auth, &h).await?;
    }
    let runtime =
        registration::current_runtime_locked(state, tx, auth.id(), q.original.identity.boot_id)
            .await?;
    let qualification = policy::current(state, &h, &runtime)?;
    // Tie the current admitted runtime to the attempt's immutable registration
    // before loading execution authority under the authenticated node lock.
    let current_registration =
        registration::admitted_runtime_locked(tx, auth.id(), q.original.identity.boot_id).await?;
    if current_registration.request.identity != q.original.identity {
        return Err(refused());
    }
    let retained =
        super::super::closure::history::for_attempt_locked(tx, &q.original.binding).await?;
    if retained.metadata.semantic_digest != current_registration.semantic_digest
        || retained.metadata.request.identity != q.original.identity
    {
        return Err(refused());
    }
    let registration_id = retained.id;
    let view = authority::load_locked(state, tx, &q.original.binding.session, Policy::IdentityOnly)
        .await?;
    super::super::closure::ensure_launch_open_locked(tx, &q.original.binding).await?;
    let (expected_status, expected_grant) = match issued {
        Some(issued) => (Status::Granted, Some(issued.receipt.grant.grant_id)),
        None => (Status::Preparing, None),
    };
    if !view.holds_task
        || view.status != expected_status
        || view.grant_id != expected_grant
        || view.cancellation_id.is_some()
        || view.revoked
        || view.stopped
    {
        return Err(refused());
    }
    let (hot, versions) = support::load_hot_locked(tx, &q.original.binding, &h.receipt.versions)
        .await
        .map_err(history::helper_error)?;
    let grant_mode = match mode {
        LoadMode::Fresh => stored::GrantMode::Fresh,
        LoadMode::Replay(issued) => stored::GrantMode::Replay(&issued.receipt),
        LoadMode::Continuing(issued) => stored::GrantMode::Continuing(&issued.receipt),
    };
    stored::require_hot_state(grant_mode, &hot, q, &h.request).map_err(history::helper_error)?;
    if issued.is_some() {
        view.active_at(now(state)?)?;
    }
    let offer =
        super::super::native_offer::load_original_offer_locked(tx, q.original.binding.attempt_id)
            .await?;
    if !same(&offer, &h.proposal.original)? {
        return Err(refused());
    }
    require_runtime_capabilities(state, &runtime, &current_registration, &offer)?;

    let row = sqlx::query(
        "SELECT l.execution_spec,l.execution_spec_digest,l.graph_task_observation,l.decision_id,d.decision,s.selection,s.selection_digest FROM profiled_launches l JOIN intelligence_decisions d USING(decision_id) JOIN intelligence_attempt_selections s USING(attempt_id) WHERE l.attempt_id=$1 FOR SHARE OF l,d,s",
    )
    .bind(q.original.binding.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let spec = pinned_spec_from_row(&row, q, &h)?;
    let decision: crate::intelligence::resolver::Decision =
        serde_json::from_value(row.get("decision")).map_err(|_| refused())?;
    let selection: aq_selection::ResolvedSelection =
        serde_json::from_value(row.get("selection")).map_err(|_| refused())?;
    if decision.decision_id != row.get::<Uuid, _>("decision_id")
        || decision
            .outcome
            .resolved
            .as_ref()
            .is_none_or(|s| !same(s, &selection).unwrap_or(false))
        || digest(&selection)? != spec.selection_digest
        || spec.selection_digest != h.proposal.choice.selection_digest
        || row.get::<String, _>("selection_digest") != spec.selection_digest.as_str()
        || selection.target
            != (aq_selection::ExecutionTarget::Worker {
                node_id: auth.id(),
                boot_id: q.original.identity.boot_id,
            })
        || selection.lineage.project_id != view.project_id
        || selection.lineage.task_id != view.task_id
        || selection.lineage.workspace_id != spec.task.workspace_id
    {
        return Err(refused());
    }
    let material = h.receipt.lease.materials[0].clone();
    if material.credential_profile != selection.choice.credential_profile
        || material.credential_revision != selection.credential_revision
        || material.provider.as_str() != selection.choice.provider
        || material.account_reference.as_str() != selection.choice.account_reference
        || material.endpoint_reference.as_str() != selection.choice.endpoint_reference
    {
        return Err(refused());
    }
    let source = source_exports::load_linked_locked(tx, q.original.binding.attempt_id).await?;
    if source.manifest().attempt != view.attempt
        || source.manifest().source != view.source.ok_or_else(|| refused())?
        || source.manifest().resolved_spec_digest != view.execution_spec_digest
        || source.token().request().project_revision != spec.project_revision
        || source.token().request().project_id != view.project_id
    {
        return Err(refused());
    }
    let ceiling = match issued {
        Some(issued) => hot
            .grant
            .as_ref()
            .ok_or_else(|| refused())?
            .expires_at_ms
            .min(issued.receipt.execution_deadline_ms),
        None => h.receipt.lease.expires_at_ms.min(material.expires_at_ms),
    };
    let mut pinned = PinnedArtifacts {
        receipts: Vec::new(),
        context: None,
        ceiling,
    };
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT artifact_id FROM profiled_launch_artifacts WHERE attempt_id=$1 ORDER BY artifact_id LIMIT 127",
    )
    .bind(q.original.binding.attempt_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(db)?;
    if ids.len() != offer.offered.artifacts.len() || ids.len() > 126 {
        return Err(refused());
    }
    let time = now(state)?;
    for id in ids {
        let a = artifacts::load_locked(tx, id).await?;
        pinned.include(
            a,
            &offer.offered.artifacts,
            source.token().request().project_id,
            &spec.task_context,
            time,
        )?;
    }
    pinned.receipts.extend(
        support::source_artifact_receipts(source.manifest()).map_err(history::helper_error)?,
    );
    support::require_artifact_receipts(&pinned.receipts, &q.original.artifacts)
        .map_err(history::helper_error)?;
    if time >= pinned.ceiling {
        return Err(refused());
    }
    let policy = super::super::continuation_policy::load_locked(tx, &q.original.binding).await?;
    require_supported_budget(&offer, &policy, &decision, &h)?;
    let input = observations::Input {
        full_context_digest: h.proposal.full_context_digest.clone(),
        spec,
        context: pinned.context.ok_or_else(|| refused())?,
        decision,
        selection,
        offer,
        source,
        material,
    };
    let registry_at_load = super::registry_at_load(state, tx, &h).await?;
    Ok(Pinned {
        registry_at_load,
        history: h,
        input,
        policy,
        capabilities: current_registration.request.capabilities.clone(),
        hot,
        versions,
        registration_id,
        qualification,
        issuance_ceiling: pinned.ceiling,
    })
}

fn require_runtime_capabilities(
    state: &AppState,
    runtime: &registration::RegisteredRuntime,
    current_registration: &registration::NativeRegistration,
    offer: &aq_admission::startup::NativeOffer,
) -> Result<(), AppError> {
    // Protocol/host capabilities belong to the actual worker runtime. Installed
    // harness entries separately prove the selected harness/model configuration.
    let qualified = state
        .config
        .profiled_execution
        .as_ref()
        .and_then(|c| {
            c.qualified_runtimes
                .iter()
                .find(|r| r.runtime == runtime.runtime && r.qualification == runtime.qualification)
        })
        .ok_or_else(|| refused())?;
    if qualified.material_delivery.is_none()
        || offer.offered.required_features.iter().any(|f| {
            !qualified.features.contains(f) || !current_registration.request.features.contains(f)
        })
    {
        return Err(refused());
    }
    support::require_runtime_capabilities(
        offer,
        [
            &qualified.capabilities,
            &current_registration.request.capabilities,
        ],
    )
    .map_err(history::helper_error)
}

// Validate the immutable spec and its stored offer provenance before decoding
// the decision and selection from the same locked row.
fn pinned_spec_from_row(
    row: &sqlx::postgres::PgRow,
    q: &PrepareRequest,
    h: &AcceptedHistory,
) -> Result<aq_admission::execution_spec::ProfiledExecutionSpec, AppError> {
    let spec: aq_admission::execution_spec::ProfiledExecutionSpec =
        serde_json::from_value(row.get("execution_spec")).map_err(|_| refused())?;
    if !same(&spec, &h.proposal.spec)?
        || spec.artifact().map_err(|_| refused())?.digest
            != q.original.binding.execution_spec_digest
        || row.get::<String, _>("execution_spec_digest")
            != q.original.binding.execution_spec_digest.as_str()
    {
        return Err(refused());
    }
    let stored: crate::profiled_execution::native_offer::observation::StoredOffer =
        serde_json::from_value(row.get("graph_task_observation")).map_err(|_| refused())?;
    if stored.read_provenance.context_response_digest != h.proposal.full_context_digest
        || !same(&stored.native_offer.native, &h.proposal.original)?
    {
        return Err(refused());
    }
    Ok(spec)
}

// Collect only this project's exact offered artifacts, retaining the earliest
// expiry and at most one task context. SQL reads stay ordered in the loader.
struct PinnedArtifacts {
    receipts: Vec<ArtifactReceipt>,
    context: Option<serde_json::Value>,
    ceiling: u64,
}

impl PinnedArtifacts {
    fn include(
        &mut self,
        artifact: artifacts::Retained,
        offered: &[RetainedArtifact],
        project: Uuid,
        task_context: &ArtifactRef,
        time: u64,
    ) -> Result<(), AppError> {
        if artifact.project_id != project
            || !offered
                .iter()
                .any(|d| same(d, &artifact.descriptor).unwrap_or(false))
        {
            return Err(refused());
        }
        artifact
            .descriptor
            .verify(&artifact.bytes, time)
            .map_err(|_| refused())?;
        self.ceiling = self.ceiling.min(artifact.descriptor.retain_until_ms);
        if artifact.descriptor.artifact == *task_context {
            if self.context.is_some() {
                return Err(refused());
            }
            self.context = Some(serde_json::from_slice(&artifact.bytes).map_err(|_| refused())?);
        }
        self.receipts.push(ArtifactReceipt {
            artifact: artifact.descriptor.artifact.clone(),
            bytes: artifact.bytes.len() as u64,
            digest: artifact.descriptor.artifact.digest,
        });
        Ok(())
    }
}

fn require_supported_budget(
    offer: &aq_admission::startup::NativeOffer,
    policy: &aq_profiles::SelectionPolicy,
    decision: &crate::intelligence::resolver::Decision,
    h: &AcceptedHistory,
) -> Result<(), AppError> {
    let ceilings = &decision.project.project.binding.ceilings;
    // The protocol's lineage time window is the attempt's own: its execution
    // profile's wall time, which the offer's manifest carries.
    if offer.offered.offer.manifest.resources.wall_time_ms
        != h.proposal.choice.maximum_lineage_elapsed_ms
        || offer.offered.offer.manifest.model.spending_budget.is_some()
        || policy.budget.max_cost_microunits.is_some()
        || policy.budget.require_complete_accounting
        || policy.budget.require_hard_cost_enforcement
        || ceilings.cost_limit_microunits.is_some()
        || ceilings.require_hard_cost_enforcement
        // A first attempt starts its lineage at zero. A B4 replacement carries
        // the lineage time already used, which limits nothing: each attempt
        // has its own window.
        || (decision.inputs.replacement.is_none() && decision.inputs.budget.elapsed_ms != Some(0))
    {
        return Err(refused());
    }
    Ok(())
}

#[cfg(test)]
mod artifact_tests {
    use super::*;
    use aq_admission::{ArtifactKind, Text};

    fn retained(bytes: &[u8]) -> artifacts::Retained {
        let reference = ArtifactRef {
            id: Text::new("test/context").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(bytes),
        };
        artifacts::Retained {
            artifact_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            owner: artifacts::OwnerKind::GraphSnapshot,
            descriptor: RetainedArtifact {
                artifact: reference,
                kind: ArtifactKind::Context,
                bytes: bytes.len() as u64,
                retention_receipt: ArtifactRef {
                    id: Text::new("test/retention").unwrap(),
                    version: Text::new("1").unwrap(),
                    digest: Digest::of(b"retention"),
                },
                retained_at_ms: 10,
                retain_until_ms: 100,
            },
            receipt: Vec::new(),
            bytes: bytes.to_vec(),
        }
    }

    fn pins(ceiling: u64) -> PinnedArtifacts {
        PinnedArtifacts {
            receipts: Vec::new(),
            context: None,
            ceiling,
        }
    }

    #[test]
    fn pins_exact_receipts_context_and_earliest_retention() {
        let context = retained(b"{}");
        let project = context.project_id;
        let task_context = context.descriptor.artifact.clone();
        let mut other = retained(b"opaque non-context bytes");
        other.project_id = project;
        other.descriptor.artifact.id = Text::new("test/other").unwrap();
        other.descriptor.retain_until_ms = 50;
        let offered = [context.descriptor.clone(), other.descriptor.clone()];
        let mut pinned = pins(90);
        pinned
            .include(context, &offered, project, &task_context, 10)
            .unwrap();
        assert_eq!(pinned.ceiling, 90);
        pinned
            .include(other, &offered, project, &task_context, 49)
            .unwrap();
        assert_eq!(pinned.ceiling, 50);
        assert_eq!(pinned.context, Some(serde_json::json!({})));
        assert_eq!(pinned.receipts.len(), 2);
        for (receipt, descriptor) in pinned.receipts.iter().zip(offered) {
            assert_eq!(receipt.artifact, descriptor.artifact);
            assert_eq!(receipt.digest, descriptor.artifact.digest);
            assert_eq!(receipt.bytes, descriptor.bytes);
        }
    }

    #[test]
    fn refuses_foreign_unoffered_changed_or_expired_artifacts() {
        for case in 0..5 {
            let mut artifact = retained(b"{}");
            let offered = [artifact.descriptor.clone()];
            let mut project = artifact.project_id;
            let task_context = artifact.descriptor.artifact.clone();
            let mut time = 20;
            match case {
                0 => project = Uuid::new_v4(),
                1 => artifact.descriptor.retain_until_ms = 101,
                2 => artifact.bytes = b"[]".to_vec(),
                3 => time = 9,
                4 => time = 100,
                _ => unreachable!(),
            }
            let mut pinned = pins(200);
            assert!(
                pinned
                    .include(artifact, &offered, project, &task_context, time)
                    .is_err(),
                "case {case}"
            );
            assert!(pinned.receipts.is_empty());
            assert!(pinned.context.is_none());
        }
    }

    #[test]
    fn null_context_still_counts_as_a_context_and_cannot_repeat() {
        let mut pinned = pins(200);
        let mut context = retained(b"null");
        let project = context.project_id;
        let task_context = context.descriptor.artifact.clone();
        let offered = [context.descriptor.clone()];
        pinned
            .include(context, &offered, project, &task_context, 20)
            .unwrap();
        context = retained(b"null");
        context.project_id = project;
        assert!(
            pinned
                .include(context, &offered, project, &task_context, 20)
                .is_err()
        );
        assert_eq!(pinned.context, Some(serde_json::Value::Null));
        assert_eq!(pinned.receipts.len(), 1);
    }

    #[test]
    fn context_requires_json_even_when_its_bytes_match_the_offer() {
        let mut pinned = pins(200);
        let context = retained(b"not JSON");
        let project = context.project_id;
        let task_context = context.descriptor.artifact.clone();
        let offered = [context.descriptor.clone()];
        assert!(
            pinned
                .include(context, &offered, project, &task_context, 20)
                .is_err()
        );
        assert!(pinned.context.is_none());
        assert!(pinned.receipts.is_empty());
    }
}
