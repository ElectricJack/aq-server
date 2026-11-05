#![allow(dead_code)]
#[path = "common.rs"]
mod common;
use aq_admission::{execution_spec::*, *};
pub fn fixture() -> (ProfiledExecutionSpec, ProfiledOffer) {
    let mut offer: ProfiledOffer =
        serde_json::from_str(include_str!("../artifacts/shared-launch.json")).unwrap();
    offer.manifest.bindings.source = common::artifact("source");
    offer.manifest.bindings.outcome = common::artifact("outcome");
    offer.manifest.selection_explanation = common::artifact("selection");
    let mut source = common::source();
    source.project_id = offer.manifest.bindings.project_id;
    source.project_revision = offer.manifest.bindings.project_revision;
    let task_context = common::artifact("task-context");
    offer.manifest.context.references[0].artifact = task_context.clone();
    offer.manifest.context.references[0].bytes = "task-context".len() as u64;
    offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&offer.manifest).unwrap();
    let spec = ProfiledExecutionSpec {
        schema_version: 1,
        lifecycle: transport::lifecycle_binding(&offer).unwrap(),
        project_revision: offer.manifest.bindings.project_revision,
        task: TaskRevision {
            workspace_id: offer.manifest.bindings.graph_id,
            task_id: offer.manifest.bindings.task_id,
            version: offer.manifest.bindings.task_revision,
            content_revision: 1,
        },
        source,
        source_artifact: offer.manifest.bindings.source.clone(),
        outcome_artifact: offer.manifest.bindings.outcome.clone(),
        graph_access_profile: offer.manifest.bindings.graph_access_profile.clone(),
        task_observation: common::artifact("task-observation"),
        task_context,
        selection_digest: Digest::of(b"sealed-selection"),
    };
    offer.binding.execution_spec_digest = aq_session_protocol::canonical_digest(&spec).unwrap();
    spec.validate_against(&offer).unwrap();
    (spec, offer)
}
