#![allow(dead_code)]
//! The selection fixture: one role on one execution profile, a level with two
//! candidates, and a node that has a harness for each.
//!
//! An execution profile names one credential profile per harness (spec 0014
//! §5.3), so the two candidates run on different harnesses: `primary`
//! (fixture-a) on `codex` with `account-a`, `fallback` (fixture-b) on
//! `opencode` with `account-b`.
use aq_profiles::roles::{Candidate, Confinement, Execution, Intelligence, Launch, Limits, Role};
use aq_profiles::*;
use aq_selection::*;
use aq_session_protocol::{ArtifactRef, Digest, Text};
use uuid::Uuid;

/// The recorded identity of the first candidate.
pub const PRIMARY: &str = "fixture-a:model-a:high";
/// The recorded identity of the second candidate.
pub const FALLBACK: &str = "fixture-b:model-b:default";
pub const ENVIRONMENT: &str = "confined-worktree";

pub fn artifact(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
pub fn policy() -> SelectionPolicy {
    SelectionPolicy {
        schema_version: 1,
        fallback_on: vec![
            FallbackReason::Unavailable,
            FallbackReason::MissingCredentials,
            FallbackReason::Unsupported,
            FallbackReason::StaleUnknown,
            FallbackReason::PolicyDenied,
        ],
        allow_unknown_availability: false,
        observation_max_age_ms: 5000,
        max_output_tokens: 128,
        max_attempts: 3,
        max_lineage_elapsed_ms: 60_000,
        max_lineage_sandbox_ms: None,
        max_uncharged_continuations: None,
        required_capabilities: vec![],
        budget: IntelligenceBudgetPolicy {
            max_cost_microunits: None,
            currency: None,
            require_complete_accounting: false,
            require_hard_cost_enforcement: false,
            unknown_cost: UnknownCostPolicy::Deny,
        },
    }
}

/// The documents a launch is resolved from, and the selection policy in
/// effect. Tests edit the parts and select again.
#[derive(Clone)]
pub struct Fx {
    pub role: Role,
    pub intelligence: Intelligence,
    pub execution: Execution,
    pub policy: SelectionPolicy,
}
impl Fx {
    /// What `Snapshot::resolve` yields for these documents: each candidate
    /// keeps the harnesses the execution profile may use.
    pub fn launch(&self) -> Launch<'_> {
        let candidates = (self.intelligence.candidates.iter())
            .map(|c| Candidate {
                harnesses: (c.harnesses.iter())
                    .filter(|h| self.execution.harnesses.contains(h))
                    .cloned()
                    .collect(),
                ..c.clone()
            })
            .filter(|c| !c.harnesses.is_empty())
            .collect();
        Launch {
            role: &self.role,
            intelligence: &self.intelligence,
            execution: &self.execution,
            candidates,
            selection_policy: self.policy.clone(),
        }
    }
}
/// Select for an offer of the fixture's launch.
pub fn select(fx: &Fx, input: &SelectionInputs) -> aq_selection::Result<SelectionOutcome> {
    select_for_offer(&fx.launch(), input)
}
fn candidate(provider: &str, model: &str, reasoning: &str, harness: &str) -> Candidate {
    Candidate {
        provider: provider.into(),
        model: model.into(),
        reasoning: reasoning.into(),
        harnesses: vec![harness.into()],
    }
}
pub fn installed(harness: &str) -> InstalledHarness {
    InstalledHarness {
        harness: harness.into(),
        cli_version: "fixture-1".into(),
        adapter: artifact(&format!("{harness}-adapter")),
        environment: artifact(ENVIRONMENT),
        qualification: artifact("synthetic-qualification"),
        failure_classifier: Some(artifact("synthetic-error-classifier")),
        capabilities: vec![],
        limits: NativeLimits {
            context_bytes: 8192,
            max_output_tokens: 512,
            wall_time_ms: 60000,
            memory_bytes: 268435456,
            cpu_millis: 60000,
            process_limit: 8,
            hard_cost_enforcer: None,
        },
    }
}
pub fn prepared(harness: &str) -> PreparedLaunch {
    PreparedLaunch {
        digest: Digest::of(format!("startup text for {harness}").as_bytes()),
        harness: harness.into(),
        environment: ENVIRONMENT.into(),
        startup_bytes: 400,
        startup_limit_bytes: 8192,
    }
}
pub fn fixture() -> (Fx, SelectionInputs) {
    let fx = Fx {
        role: Role {
            id: "developer".into(),
            name: "Developer".into(),
            description: "Makes changes.".into(),
            execution: "sandbox".into(),
            operations: vec!["task.read".into()],
            enabled: true,
            role_md: "You make the change.".into(),
            rules_md: String::new(),
        },
        intelligence: Intelligence {
            id: "standard-high".into(),
            name: "Standard, high".into(),
            description: "Ordinary work.".into(),
            candidates: vec![
                candidate("fixture-a", "model-a", "high", "codex"),
                candidate("fixture-b", "model-b", "default", "opencode"),
            ],
            selection: Default::default(),
        },
        execution: Execution {
            id: "sandbox".into(),
            name: "Sandbox".into(),
            description: "The fixture node.".into(),
            harnesses: vec!["codex".into(), "opencode".into()],
            environment: ENVIRONMENT.into(),
            confinement: Confinement::Native,
            credentials: [("codex", "account-a"), ("opencode", "account-b")]
                .into_iter()
                .map(|(harness, profile)| (harness.into(), profile.into()))
                .collect(),
            limits: Limits {
                wall_time_ms: 60000,
                memory_bytes: 268435456,
                cpu_millis: 60000,
                process_limit: 8,
                startup_context_bytes: 8192,
                max_output_tokens: 128,
            },
        },
        policy: policy(),
    };
    let lineage = TaskLineage {
        project_id: Uuid::from_u128(1),
        workspace_id: Uuid::from_u128(2),
        task_id: Uuid::from_u128(3),
    };
    let target = ExecutionTarget::Worker {
        node_id: Uuid::from_u128(4),
        boot_id: Uuid::from_u128(5),
    };
    let credentials: Vec<_> = [("a", "fixture-a"), ("b", "fixture-b")]
        .iter()
        .map(|(suffix, provider)| AuthorizedCredential {
            profile: artifact(&format!("account-{suffix}")),
            provider: (*provider).into(),
            account_reference: format!("billing-{suffix}"),
            endpoint_reference: format!("endpoint-{suffix}"),
            revision: 1,
            enabled: true,
            provisionable: true,
            material_owner: None,
            allowed_backends: aq_selection::default_allowed_backends(),
        })
        .collect();
    let node = CapabilitySnapshot {
        backend_kind: None,
        material_delivery: None,
        target: target.clone(),
        revision: 1,
        observed_at_ms: 1000,
        expires_at_ms: 20000,
        harnesses: vec![installed("codex"), installed("opencode")],
        credential_bindings: credentials
            .iter()
            .map(|c| CredentialBinding {
                credential_revision: 1,
                credential_profile: c.profile.clone(),
                available: true,
                preparation: None,
                observed_at_ms: 1000,
                expires_at_ms: 20000,
            })
            .collect(),
    };
    let observations = credentials
        .iter()
        .enumerate()
        .map(|(index, c)| AvailabilityObservation {
            observation_id: Uuid::from_u128(10 + index as u128),
            scope: ObservationScope {
                credential_revision: 1,
                provider: c.provider.clone(),
                credential_profile: c.profile.clone(),
                account_reference: c.account_reference.clone(),
                endpoint_reference: c.endpoint_reference.clone(),
                model: Some(format!("model-{}", if index == 0 { "a" } else { "b" })),
                node_id: None,
            },
            source: ObservationSource::Operator {
                principal_id: Uuid::from_u128(99),
            },
            sequence: 1,
            revision: 1,
            state: Availability::Available,
            received_at_ms: 1000,
            expires_at_ms: 10000,
            cooldown_until_ms: 0,
        })
        .collect();
    let input = SelectionInputs {
        project: ProjectSnapshot {
            lineage: lineage.clone(),
            revision: 1,
            role_snapshot_digest: Digest::of(b"role-snapshot"),
            admission_snapshot_digest: Digest::of(b"project-admission"),
            expires_at_ms: 15000,
            target,
            ceilings: ResourceCeilings {
                context_bytes: 8192,
                wall_time_ms: 60000,
                memory_bytes: 268435456,
                cpu_millis: 60000,
                process_limit: 8,
                cost_limit_microunits: None,
                currency: None,
                require_hard_cost_enforcement: false,
            },
            allowed_environments: vec![ENVIRONMENT.into()],
            permission_ceiling: vec![aq_session_protocol::Scope {
                operation: Text::new("task.read").unwrap(),
                resource: Text::new(format!("project:{}", Uuid::from_u128(1))).unwrap(),
            }],
            role: fx.role.id.clone(),
            intelligence_profile: fx.intelligence.id.clone(),
            execution_profile: fx.execution.id.clone(),
        },
        node,
        credentials,
        observations,
        registry_revision: 1,
        budget: BudgetSnapshot {
            lineage,
            revision: 1,
            attempts_started: 0,
            exempt_attempts: 0,
            last_attempt_id: None,
            elapsed_ms: Some(0),
            spent_microunits: None,
            currency: None,
            accounting: AccountingCompleteness::Unavailable,
            accounting_source: artifact("unavailable-ledger"),
            hard_budget_provider: None,
            observed_at_ms: 1000,
            expires_at_ms: 20000,
        },
        replacement: None,
        estimates: vec![],
        now_ms: 2000,
        placement: None,
        route: None,
        launches: vec![prepared("codex"), prepared("opencode")],
    };
    (fx, input)
}
/// The fixture under another selection policy.
pub fn change_policy(fx: &Fx, policy: SelectionPolicy, _input: &mut SelectionInputs) -> Fx {
    Fx {
        policy,
        ..fx.clone()
    }
}
/// The fixture with another confinement on its execution profile.
pub fn change_confinement(fx: &Fx, confinement: Confinement, _input: &mut SelectionInputs) -> Fx {
    let mut changed = fx.clone();
    changed.execution.confinement = confinement;
    changed
}
