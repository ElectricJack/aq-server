//! Exact daemon-owned context facts: synthetic evidence, no model/provider calls.
use crate::common;
#[cfg(feature = "any-model")]
use aq_server::profiled_execution::context_budget;
use aq_server::profiled_execution::{
    config::{ProfiledConfig, QualifiedRuntime},
    context_budget::{CapacityKind, Qualification, Rendering, Subject},
    policies,
};
use common::profiled::{NOW, artifact, fixture};
use std::sync::Arc;
#[tokio::test]
async fn context_fact_changes_are_fenced_by_installed_global_revision_and_digest() {
    let f = fixture().await;
    let c = &f.selection.choice;
    let runtime = artifact("synthetic-context-runtime", b"runtime");
    let runtime_qualification =
        artifact("synthetic-context-runtime-qualification", b"qualification");
    let fact = Qualification {
        schema_version: 1,
        subject: Subject {
            runtime: runtime.clone(),
            runtime_qualification: runtime_qualification.clone(),
            harness: c.harness.clone(),
            cli_version: c.cli_version.clone(),
            adapter: c.adapter.clone(),
            environment: c.environment.clone(),
            harness_qualification: c.qualification.clone(),
            provider: c.provider.clone(),
            model: c.model.clone(),
            native_reasoning: c.native_reasoning.clone(),
            reasoning_configuration_reference: c.reasoning_configuration_reference.clone(),
            credential_profile: c.credential_profile.clone(),
            credential_revision: f.selection.credential_revision,
            account_reference: c.account_reference.clone(),
            endpoint_reference: c.endpoint_reference.clone(),
        },
        rendering: Rendering::Utf8DoubleNewlineV1,
        maximum_tokens_per_utf8_byte: 2,
        fixed_overhead_tokens: 37,
        counting_evidence: artifact("synthetic-only-count-proof", b"counting"),
        capacity_kind: CapacityKind::QualifiedConservativePlanningCeiling,
        capacity_tokens: 16384,
        capacity_evidence: artifact("synthetic-only-capacity-proof", b"capacity"),
        observed_at_ms: NOW - 10,
        expires_at_ms: NOW + 30000,
    };
    let original = ProfiledConfig {
        definition_bodies: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime,
            qualification: runtime_qualification,
            capabilities: vec![],
            features: vec![],
            material_delivery: None,
        }],
        projects: vec![],
        context_qualifications: vec![fact],
    };
    let mut current = f.state.clone();
    let mut cfg = (*current.config).clone();
    cfg.profiled_execution = Some(Arc::new(original.clone()));
    current.config = Arc::new(cfg);
    policies::install(&current).await.unwrap();
    let mut changed = original.clone();
    changed.context_qualifications[0].capacity_tokens -= 1;
    assert_ne!(
        original.context_qualifications[0].artifact().unwrap(),
        changed.context_qualifications[0].artifact().unwrap()
    );
    let mut stale = current.clone();
    let mut cfg = (*stale.config).clone();
    cfg.profiled_execution = Some(Arc::new(changed.clone()));
    stale.config = Arc::new(cfg);
    assert!(policies::install(&stale).await.is_err());
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        policies::fence_configuration_locked(&mut tx, &changed)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    changed.revision = 2;
    let mut newer = current.clone();
    let mut cfg = (*newer.config).clone();
    cfg.profiled_execution = Some(Arc::new(changed.clone()));
    newer.config = Arc::new(cfg);
    policies::install(&newer).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        policies::fence_configuration_locked(&mut tx, &original)
            .await
            .is_err()
    );
    policies::fence_configuration_locked(&mut tx, &changed)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

/// A real selection over the selection fixture, its runtime boot and composition.
#[cfg(feature = "any-model")]
fn case() -> (
    aq_server::profiled_execution::registration::RegisteredRuntime,
    aq_selection::ResolvedSelection,
    aq_profiles::FrozenComposition,
) {
    let raw: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/selection-v2.json")).unwrap();
    let catalog = aq_profiles::ValidatedCatalog::from_artifact(
        raw["catalog_artifact_json"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    let input: aq_selection::SelectionInputs =
        serde_json::from_value(raw["inputs"].clone()).unwrap();
    let selection = aq_selection::select(&catalog, &input)
        .unwrap()
        .resolved
        .unwrap();
    let aq_selection::ExecutionTarget::Worker { boot_id, .. } = selection.target else {
        unreachable!()
    };
    let runtime = aq_server::profiled_execution::registration::RegisteredRuntime {
        runtime: artifact("synthetic-only-runtime", b"runtime"),
        qualification: artifact("synthetic-only-runtime-qualification", b"qualification"),
        boot_id,
    };
    (
        runtime,
        selection,
        input.compositions[0].composition.clone(),
    )
}
/// `any-model`: no fact is consulted. The token numbers are the byte counts, so
/// the worker-side inequalities hold by construction, and the byte limits still refuse.
#[cfg(feature = "any-model")]
#[test]
fn bytes_alone_count_the_startup_and_the_byte_limit_still_refuses() {
    let (runtime, mut selection, mut composition) = case();
    let (counted, renderer) =
        context_budget::evaluate(&runtime, &selection, &composition, 2000).unwrap();
    let p = &counted.proof;
    assert_eq!(p.rendered_bytes, counted.bytes.len() as u64);
    assert_eq!(p.input_tokens_upper_bound, p.rendered_bytes);
    assert_eq!(
        p.instructions
            .iter()
            .map(|i| i.tokens_upper_bound)
            .sum::<u64>(),
        p.rendered_bytes
    );
    assert_eq!(p.reserved_output_tokens, selection.choice.max_output_tokens);
    assert_eq!(
        p.window_tokens(),
        Some(p.rendered_bytes + p.reserved_output_tokens)
    );
    assert_eq!(p.renderer(), &renderer.definition);
    assert_eq!(
        renderer.definition.digest,
        aq_admission::Digest::of(&renderer.body)
    );
    assert_eq!(
        (renderer.implicit_tokens, renderer.expires_at_ms),
        (0, None)
    );
    assert!(renderer.evidence.is_empty());
    // The rule names no model: another model and reasoning get the same Definition.
    let mut other = selection.clone();
    other.choice.model = "another-model".into();
    other.choice.native_reasoning = "default".into();
    let (_, same) = context_budget::evaluate(&runtime, &other, &composition, 2000).unwrap();
    assert_eq!(same.definition, renderer.definition);
    assert_eq!(
        String::from_utf8(renderer.body).unwrap(),
        r#"{"rendering":"utf8_double_newline_v1","schema_version":1}"#
    );
    // One byte over the selected context limit refuses, as before.
    let limit = (selection.choice.resources.context_bytes).min(composition.context_limit_bytes);
    selection.choice.resources.context_bytes = counted.bytes.len() as u64 - 1;
    assert_eq!(
        context_budget::evaluate(&runtime, &selection, &composition, 2000).err(),
        Some(context_budget::Error::Input)
    );
    selection.choice.resources.context_bytes = limit;
    let assignment = composition.instruction_order.last_mut().unwrap();
    assignment.text = "x".repeat(limit as usize + 1);
    assignment.source.digest = aq_admission::Digest::of(assignment.text.as_bytes())
        .as_str()
        .into();
    selection.choice.composition_digest = aq_selection::digest(&composition).unwrap();
    assert_eq!(
        context_budget::evaluate(&runtime, &selection, &composition, 2000).err(),
        Some(context_budget::Error::Input)
    );
    selection.choice.max_output_tokens = 0;
    assert!(context_budget::evaluate(&runtime, &selection, &composition, 2000).is_err());
}
