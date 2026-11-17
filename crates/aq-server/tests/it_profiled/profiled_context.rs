//! Exact daemon-owned context facts: synthetic evidence, no model/provider calls.
use crate::common;
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
