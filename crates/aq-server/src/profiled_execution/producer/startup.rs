//! One central render; the worker verifies and consumes these exact bytes.
use super::{
    artifacts::{ArtifactBody, RetainedSet, canonical, reference},
    compose::Prepared,
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
use aq_profiles::roles::StartupPart;
use aq_session_protocol::{ContextDelivery, ContextPlan, ContextRef};
pub struct Startup {
    pub descriptor: Descriptor,
    pub context: ContextPlan,
    pub modules: Vec<ArtifactRef>,
}
/// Where an instruction of the startup text comes from, as the context plan
/// records it.
fn provenance(part: StartupPart, origin: &Origin<'_>) -> String {
    let role = origin.role;
    match part {
        StartupPart::Platform => "aq.platform-text".into(),
        StartupPart::Role => format!("role:{role}"),
        StartupPart::Rules => format!("role:{role}:rules"),
        StartupPart::ProjectNotes => format!("project-notes:{role}"),
        StartupPart::RepositoryNotes => "repository-notes".into(),
        StartupPart::Task => origin.task.into(),
        StartupPart::TaskThread => origin.thread.unwrap_or("aq.task-thread").into(),
        StartupPart::Reporting => super::result_startup::STARTUP_ID.into(),
        StartupPart::RetryEvidence => "aq.previous-delivery-check".into(),
    }
}
/// The names the context plan gives the parts that have an identity of
/// their own: the role, the task's context artifact and the task thread.
pub struct Origin<'a> {
    pub role: &'a str,
    pub task: &'a str,
    pub thread: Option<&'a str>,
}
pub async fn retain(
    state: &AppState,
    set: &mut RetainedSet,
    prepared: &Prepared,
    origin: &Origin<'_>,
    counted: &CountedStartup,
    projection: &Projection,
    renderer: &Renderer,
) -> Result<Startup, AppError> {
    let text = &prepared.startup;
    if counted.proof.launch_digest != prepared.launch.digest
        || counted.proof.renderer != renderer.definition
        || counted.proof.rendered_digest != Digest::of(&counted.bytes)
        || counted.proof.rendered_bytes != counted.bytes.len() as u64
        || counted.bytes != text.rendered.as_bytes()
    {
        return Err(refused());
    }
    let mut reconstructed = Vec::new();
    let mut spans = Vec::new();
    let mut contexts = Vec::new();
    let (mut tasks, mut reports) = (0, 0);
    for (index, (part, instruction)) in text.instructions.iter().enumerate() {
        if !spans.is_empty() {
            reconstructed.extend_from_slice(b"\n\n");
        }
        let offset = reconstructed.len() as u64;
        let bytes = instruction.as_bytes();
        reconstructed.extend_from_slice(bytes);
        let (artifact, owner) = match part {
            StartupPart::Reporting => {
                let reporting = prepared.reporting.as_ref().ok_or_else(|| refused())?;
                if bytes != reporting.text.as_bytes() {
                    return Err(refused());
                }
                reports += 1;
                (reporting.artifact.clone(), OwnerKind::Startup)
            }
            StartupPart::Task => {
                if bytes != projection.bytes {
                    return Err(refused());
                }
                tasks += 1;
                (projection.artifact.clone(), OwnerKind::GraphSnapshot)
            }
            _ => {
                let from = Digest::of(provenance(*part, origin).as_bytes());
                let artifact = reference(&format!("aq.startup-input/{}", from.as_str()), bytes)?;
                (artifact, OwnerKind::Startup)
            }
        };
        let artifact_body = ArtifactBody::new(ArtifactKind::Context, artifact.clone(), bytes);
        set.retain(state, owner, artifact_body).await?;
        let bound = counted
            .proof
            .instructions
            .iter()
            .find(|b| b.instruction_index == index)
            .ok_or_else(|| refused())?;
        contexts.push(ContextRef {
            artifact: artifact.clone(),
            provenance: Text::new(provenance(*part, origin)).map_err(|_| refused())?,
            delivery: ContextDelivery::Startup,
            bytes: bytes.len() as u64,
            tokens: bound.tokens_upper_bound,
            required: true,
        });
        spans.push(InputSpan {
            artifact,
            offset,
            bytes: bytes.len() as u64,
        });
    }
    if let Some(reporting) = &prepared.reporting {
        reporting.validate_rendered(reports, &counted.bytes)?;
    }
    if tasks != 1 || reconstructed != counted.bytes {
        return Err(refused());
    }
    let payload = reference("aq.rendered-startup", &counted.bytes)?;
    let artifact_body = ArtifactBody::new(ArtifactKind::Context, payload.clone(), &counted.bytes);
    set.retain(state, OwnerKind::Startup, artifact_body).await?;
    let qualification = renderer.definition.clone();
    let artifact_body = ArtifactBody::new(
        ArtifactKind::Definition,
        qualification.clone(),
        &renderer.body,
    );
    set.retain(state, OwnerKind::Startup, artifact_body).await?;
    let proof_bytes = canonical(&counted.proof)?;
    let proof = reference("aq.startup-budget-proof", &proof_bytes)?;
    let artifact_body = ArtifactBody::new(ArtifactKind::Definition, proof.clone(), &proof_bytes);
    set.retain(state, OwnerKind::Startup, artifact_body).await?;
    // The wire still carries token fields (spec 0014 §8 removes them with the
    // protocol): each is the byte count, and the renderer adds none.
    let descriptor = Descriptor {
        schema_version: 1,
        payload,
        renderer_qualification: qualification.clone(),
        startup_inputs: spans,
        payload_bytes: counted.bytes.len() as u64,
        payload_tokens_upper_bound: counted.proof.input_tokens_upper_bound,
        implicit_tokens_upper_bound: 0,
    };
    descriptor
        .verify_payload(&counted.bytes)
        .map_err(|_| refused())?;
    let descriptor_ref = descriptor.artifact().map_err(|_| refused())?;
    let descriptor_bytes = canonical(&descriptor)?;
    let artifact_body = ArtifactBody::new(
        ArtifactKind::Definition,
        descriptor_ref.clone(),
        &descriptor_bytes,
    );
    set.retain(state, OwnerKind::Startup, artifact_body).await?;
    let mut modules = vec![descriptor_ref, qualification, proof];
    if let Some(reporting) = &prepared.reporting {
        modules.push(reporting.artifact.clone());
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
