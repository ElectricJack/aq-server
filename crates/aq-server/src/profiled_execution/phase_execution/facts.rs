//! Project actual owning observations only after validation and final SQL fences.
use super::phase::{FreshConfirmation, PrepareRequest};
use super::*;
use uuid::Uuid;

pub(super) fn confirmation(
    p: &pinned::Pinned,
    q: &PrepareRequest,
    o: &observations::Observed,
    c: &current::Current,
    budget_revision: u64,
    time: u64,
) -> Result<FreshConfirmation, AppError> {
    o.validate_for(&p.input, time)?;
    c.fresh_at(time)?;
    let confirmation_id = Uuid::new_v4();
    let inputs = stored::FactInputs {
        proposal: &p.history.proposal,
        accepted: &p.history.receipt,
        accepted_request: &p.history.request,
        request: q,
        spec: &p.input.spec,
        offer: &p.input.offer,
        material: &p.input.material,
        full_context_digest: &p.input.full_context_digest,
        capabilities: &p.capabilities,
        registry_fact: super::registry_fact(p, c),
        registry: &c.registry,
        principal: &c.principal,
        inventory: &c.inventory,
        inventory_sequence: c.inventory_sequence,
        inventory_expires_at_ms: c.inventory_expires_at_ms(),
        availability: &c.availability,
        availability_expires_at_ms: c.availability_expires_at_ms,
        checked_at_ms: c.checked_at_ms,
        binding_expires_at_ms: o.binding.binding_observation.expires_at_ms,
        graph: &o.graph,
        source_digest: digest(&o.source)?,
        source_observed_at_ms: o.source.observed_at_ms(),
        source_expires_at_ms: o.source.expires_at_ms(),
        transfer_id: p.input.source.manifest().transfer_id,
        file_digest: &o.file.digest,
        file_started_at_ms: o.file_started_at_ms,
    };
    stored::confirmation(&inputs, budget_revision, time, confirmation_id)
        .map_err(history::helper_error)
}
