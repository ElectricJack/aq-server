/// Compare the governing inputs before admitting observations collected outside
/// the transaction. New grants and delivery replays share this freshness fence.
fn check_preflight(
    preparation: &PhasePreparation<'_>,
    before: &pinned::Pinned,
    after: &pinned::Pinned,
    observed: &observations::Observed,
) -> Result<(), AppError> {
    compare_pinned_inputs(before, after)?;
    validate_registry_loads(before, after)?;
    observed.validate_for(&after.input, now(preparation.state)?)?;
    verify_observed_binding(preparation.auth.id(), after, observed)
}

fn compare_pinned_inputs(before: &pinned::Pinned, after: &pinned::Pinned) -> Result<(), AppError> {
    require_same(&before.history.proposal, &after.history.proposal)?;
    require_same(&before.history.request, &after.history.request)?;
    require_same(&before.history.receipt, &after.history.receipt)?;
    if before.qualification != after.qualification
        || before.registration_id != after.registration_id
    {
        return Err(refused());
    }
    if before.input.source.fact().canonical_bytes()?
        != after.input.source.fact().canonical_bytes()?
    {
        return Err(refused());
    }
    Ok(())
}

fn verify_observed_binding(
    node: uuid::Uuid,
    after: &pinned::Pinned,
    observed: &observations::Observed,
) -> Result<(), AppError> {
    if observed.binding.project.revision != after.input.spec.project_revision
        || observed.binding.node_id != node
    {
        return Err(refused());
    }
    require_same(
        &observed.binding.project.binding,
        &after.input.decision.project.project.binding,
    )
}

fn require_same(a: &impl Serialize, b: &impl Serialize) -> Result<(), AppError> {
    if same(a, b)? { Ok(()) } else { Err(refused()) }
}

fn current_fence_input<'a>(
    before: &'a pinned::Pinned,
    after: &'a pinned::Pinned,
) -> current::Input<'a> {
    current::Input {
        proposal: &after.history.proposal,
        selection: &after.input.selection,
        decision: &after.input.decision,
        policy: &after.policy,
        material: &after.input.material,
        registry_at_preflight: &before.registry_at_load,
    }
}

fn verify_replayed_receipt(
    original: &history::PhaseExecutionHistory,
    current: &history::PhaseExecutionHistory,
) -> Result<(), AppError> {
    require_same(&original.receipt, &current.receipt)?;
    require_same(&original.provenance, &current.provenance)?;
    require_same(&original.request, &current.request)?;
    if original.receipt_digest != current.receipt_digest {
        return Err(refused());
    }
    Ok(())
}
