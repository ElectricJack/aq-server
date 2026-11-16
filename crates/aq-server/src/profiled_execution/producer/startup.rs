//! One central render; the worker verifies and consumes these exact bytes.
use super::{
    artifacts::{RetainedSet, canonical, reference},
    refused,
};
use crate::{
    AppState,
    error::AppError,
    profiled_execution::{
        artifacts::OwnerKind,
        context_budget::{CountedStartup, Renderer},
        context_projection::Projection,
    },
};
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest, Text,
    startup::{Descriptor, InputSpan},
};
use aq_profiles::{Delivery, FrozenComposition};
use aq_session_protocol::{ContextDelivery, ContextPlan, ContextRef};
pub struct Startup {
    pub descriptor: Descriptor,
    pub context: ContextPlan,
    pub modules: Vec<ArtifactRef>,
}
pub async fn retain(
    state: &AppState,
    set: &mut RetainedSet,
    composition: &FrozenComposition,
    counted: &CountedStartup,
    projection: &Projection,
    renderer: &Renderer,
    adapter: &ArtifactRef,
) -> Result<Startup, AppError> {
    if composition
        .instruction_order
        .iter()
        .any(|i| i.delivery != Delivery::Startup)
        || counted.proof.composition_digest
            != aq_selection::digest(composition).map_err(|_| AppError::Internal)?
        || counted.proof.renderer() != &renderer.definition
        || counted.proof.rendered_digest != Digest::of(&counted.bytes)
        || counted.proof.rendered_bytes != counted.bytes.len() as u64
    {
        return Err(refused());
    }
    let reporting = super::result_startup::for_adapter(state, adapter)?;
    let mut reporting_count = 0;
    let mut reconstructed = Vec::new();
    let mut spans = Vec::new();
    let mut contexts = Vec::new();
    let mut task_count = 0;
    for (index, instruction) in composition.instruction_order.iter().enumerate() {
        if !spans.is_empty() {
            reconstructed.extend_from_slice(b"\n\n");
        }
        let offset = reconstructed.len() as u64;
        let bytes = instruction.text.as_bytes();
        reconstructed.extend_from_slice(bytes);
        let artifact = if instruction.source.identity == super::result_startup::STARTUP_ID {
            let reporting = reporting.as_ref().ok_or_else(|| refused())?;
            if !reporting.matches(&instruction.source)
                || bytes != reporting.text.as_bytes()
                || !instruction.required
            {
                return Err(refused());
            }
            reporting_count += 1;
            reporting.artifact.clone()
        } else if instruction.source.identity == projection.artifact.id.as_str() {
            task_count += 1;
            if bytes != projection.bytes
                || instruction.source.digest != projection.artifact.digest.as_str()
                || !instruction.required
            {
                return Err(refused());
            }
            projection.artifact.clone()
        } else {
            let origin =
                aq_selection::digest(&instruction.source).map_err(|_| AppError::Internal)?;
            reference(&format!("aq.startup-input/{}", origin.as_str()), bytes)?
        };
        let owner = if artifact == projection.artifact {
            OwnerKind::GraphSnapshot
        } else {
            OwnerKind::Startup
        };
        set.retain(state, owner, ArtifactKind::Context, artifact.clone(), bytes)
            .await?;
        let bound = counted
            .proof
            .instructions
            .iter()
            .find(|b| b.instruction_index == index)
            .ok_or_else(|| refused())?;
        contexts.push(ContextRef {
            artifact: artifact.clone(),
            provenance: Text::new(&instruction.source.identity).map_err(|_| refused())?,
            delivery: ContextDelivery::Startup,
            bytes: bytes.len() as u64,
            tokens: bound.tokens_upper_bound,
            required: instruction.required,
        });
        spans.push(InputSpan {
            artifact,
            offset,
            bytes: bytes.len() as u64,
        });
    }
    if let Some(reporting) = &reporting {
        reporting.validate_rendered(reporting_count, &counted.bytes)?;
    }
    if task_count != 1 || reconstructed != counted.bytes {
        return Err(refused());
    }
    let payload = reference("aq.rendered-startup", &counted.bytes)?;
    set.retain(
        state,
        OwnerKind::Startup,
        ArtifactKind::Context,
        payload.clone(),
        &counted.bytes,
    )
    .await?;
    let qualification = renderer.definition.clone();
    set.retain(
        state,
        OwnerKind::Startup,
        ArtifactKind::Definition,
        qualification.clone(),
        &renderer.body,
    )
    .await?;
    let proof_bytes = canonical(&counted.proof)?;
    let proof = reference("aq.startup-budget-proof", &proof_bytes)?;
    set.retain(
        state,
        OwnerKind::Startup,
        ArtifactKind::Definition,
        proof.clone(),
        &proof_bytes,
    )
    .await?;
    let descriptor = Descriptor {
        schema_version: 1,
        payload,
        renderer_qualification: qualification.clone(),
        startup_inputs: spans,
        payload_bytes: counted.bytes.len() as u64,
        payload_tokens_upper_bound: counted
            .proof
            .input_tokens_upper_bound
            .checked_sub(renderer.implicit_tokens)
            .ok_or_else(|| refused())?,
        implicit_tokens_upper_bound: renderer.implicit_tokens,
    };
    descriptor
        .verify_payload(&counted.bytes)
        .map_err(|_| refused())?;
    let descriptor_ref = descriptor.artifact().map_err(|_| refused())?;
    set.retain(
        state,
        OwnerKind::Startup,
        ArtifactKind::Definition,
        descriptor_ref.clone(),
        &canonical(&descriptor)?,
    )
    .await?;
    let mut modules = vec![descriptor_ref, qualification, proof];
    if let Some(reporting) = reporting {
        modules.push(reporting.artifact);
    }
    Ok(Startup {
        descriptor,
        context: ContextPlan {
            references: contexts,
            startup_token_budget: counted.proof.input_tokens_upper_bound,
            startup_byte_budget: counted.proof.rendered_bytes,
            reserved_output_tokens: counted.proof.reserved_output_tokens,
            context_window_tokens: counted.proof.window_tokens().ok_or_else(|| refused())?,
        },
        modules,
    })
}
