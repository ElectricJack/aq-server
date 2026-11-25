use super::*;

fn sandbox_placement(input: &SelectionInputs, harness_version: &str) -> Placement {
    let template = input.node.harnesses[0].environment.digest.clone();
    Placement {
        capabilities: vec!["vm-isolation".into()],
        required_capabilities: vec![],
        templates: vec![AdvertisedTemplate {
            digest: template.clone(),
            harnesses: ["codex", "opencode"]
                .map(|harness| AdvertisedHarness {
                    harness: harness.into(),
                    version: harness_version.into(),
                })
                .into(),
        }],
        environment_templates: vec![EnvironmentTemplate {
            environment: ENVIRONMENT.into(),
            template,
        }],
        ceiling: None,
        preferred_peer: None,
        backend: None,
    }
}
fn all_traces_refuse(out: &SelectionOutcome, state: CandidateState, reason: &str) {
    assert!(out.resolved.is_none());
    assert!(!out.traces.is_empty());
    for trace in &out.traces {
        assert_eq!(trace.state, state, "{trace:?}");
        assert!(trace.reason_codes.contains(&reason.into()), "{trace:?}");
    }
}
#[test]
fn template_missing_selected_harness_version_is_ineligible() {
    let (catalog, mut input) = fixture();
    input.placement = Some(sandbox_placement(&input, "fixture-1"));
    let selected = select(&catalog, &input).unwrap().resolved.unwrap();
    assert_eq!(
        selected.choice.environment.digest,
        input.placement.as_ref().unwrap().environment_templates[0].template,
        "the frozen environment pin is the resolved template digest"
    );
    input.placement = Some(sandbox_placement(&input, "fixture-2"));
    let out = select(&catalog, &input).unwrap();
    all_traces_refuse(
        &out,
        CandidateState::Unsupported,
        "template_missing_selected_harness_version",
    );
    let mut placement = sandbox_placement(&input, "fixture-1");
    placement.environment_templates.clear();
    input.placement = Some(placement);
    let out = select(&catalog, &input).unwrap();
    all_traces_refuse(
        &out,
        CandidateState::Unsupported,
        "environment_template_unresolved",
    );
    let mut placement = sandbox_placement(&input, "fixture-1");
    placement.environment_templates[0].template = Digest::of(b"other-template");
    input.placement = Some(placement);
    let out = select(&catalog, &input).unwrap();
    all_traces_refuse(
        &out,
        CandidateState::Unsupported,
        "environment_template_digest_mismatch",
    );
    let mut placement = sandbox_placement(&input, "fixture-1");
    placement.templates.clear();
    input.placement = Some(placement);
    let out = select(&catalog, &input).unwrap();
    all_traces_refuse(&out, CandidateState::Unsupported, "template_not_advertised");
    let mut placement = sandbox_placement(&input, "fixture-1");
    placement.required_capabilities = vec!["nested-docker".into()];
    input.placement = Some(placement);
    let out = select(&catalog, &input).unwrap();
    all_traces_refuse(
        &out,
        CandidateState::Unsupported,
        "required_backend_capability_unsupported",
    );
}
#[test]
fn node_at_spend_ceiling_refuses_with_reason_in_trace() {
    let (catalog, mut input) = fixture();
    for (ceiling, reason) in [
        (NodeCeiling::Spend, "node_spend_ceiling_reached"),
        (NodeCeiling::Concurrency, "node_concurrency_ceiling_reached"),
        (NodeCeiling::SpendUnaccounted, "node_spend_unaccounted"),
        (NodeCeiling::DiskPressure, "node_disk_pressure"),
    ] {
        let mut placement = sandbox_placement(&input, "fixture-1");
        placement.ceiling = Some(ceiling);
        input.placement = Some(placement);
        let out = select(&catalog, &input).unwrap();
        all_traces_refuse(&out, CandidateState::Unavailable, reason);
    }
    let mut placement = sandbox_placement(&input, "fixture-1");
    placement.preferred_peer = Some(BackendKind::DockerLocal);
    input.placement = Some(placement);
    let out = select(&catalog, &input).unwrap();
    all_traces_refuse(
        &out,
        CandidateState::Unavailable,
        "placement_preferred_backend_available",
    );
    // Old persisted inputs without placement still deserialize.
    let mut json = serde_json::to_value(&input).unwrap();
    json.as_object_mut().unwrap().remove("placement");
    let old: SelectionInputs = serde_json::from_value(json).unwrap();
    assert!(old.placement.is_none());
}
#[test]
fn container_only_backend_admits_only_permitted_container_confinement() {
    let (catalog, mut input) = fixture();
    let mut placement = sandbox_placement(&input, "fixture-1");
    placement.backend = Some(BackendKind::DockerLocal);
    input.placement = Some(placement);
    // Default execution profile denies container confinement.
    let out = select(&catalog, &input).unwrap();
    all_traces_refuse(
        &out,
        CandidateState::PolicyDenied,
        "container_confinement_not_permitted",
    );
    let native = ExecutionConfinement {
        permit_container: false,
        require_native: true,
        require_container: false,
    };
    let required = change_confinement(&catalog, Confinement::NativeRequired, &mut input);
    let out = select(&required, &input).unwrap();
    all_traces_refuse(
        &out,
        CandidateState::Unsupported,
        "execution_requires_native_confinement",
    );
    let permit = ExecutionConfinement {
        permit_container: true,
        require_native: false,
        require_container: false,
    };
    let permitted = change_confinement(&catalog, Confinement::ContainerPermitted, &mut input);
    assert!(select(&permitted, &input).unwrap().resolved.is_some());
    assert_eq!(
        admit_confinement(input.placement.as_ref(), &permit, &input.node.harnesses[0]),
        Ok(ConfinementMode::Container)
    );
    // Non-container-only backends (and nodes without a sandbox) stay native,
    // including for native-required profiles.
    for backend in [Some(BackendKind::Boat), None] {
        input.placement.as_mut().unwrap().backend = backend;
        assert_eq!(
            admit_confinement(input.placement.as_ref(), &native, &input.node.harnesses[0]),
            Ok(ConfinementMode::Native)
        );
    }
    assert_eq!(
        admit_confinement(
            None,
            &ExecutionConfinement::default(),
            &input.node.harnesses[0]
        ),
        Ok(ConfinementMode::Native)
    );
    let required = change_confinement(&catalog, Confinement::NativeRequired, &mut input);
    assert!(select(&required, &input).unwrap().resolved.is_some());
}

fn required_container() -> ExecutionConfinement {
    ExecutionConfinement {
        permit_container: true,
        require_native: false,
        require_container: true,
    }
}

#[test]
fn container_required_profile_needs_a_qualified_adapter_and_advertised_capability() {
    let (_, input) = fixture();
    let always = required_container();
    // Only for the node-sandbox adapter whose node snapshot advertises
    // container confinement (MS1 review should-fix 1).
    let fixture_harness = input.node.harnesses[0].clone();
    assert_eq!(
        admit_confinement(None, &always, &fixture_harness),
        Err((
            CandidateState::Unsupported,
            "container_required_adapter_unqualified"
        ))
    );
    let mut sandboxed = fixture_harness.clone();
    sandboxed.adapter.id = aq_session_protocol::Text::new(NODE_SANDBOX_ADAPTER).unwrap();
    sandboxed
        .capabilities
        .retain(|c| c.id.as_str() != "aq.harness.container-confinement");
    assert_eq!(
        admit_confinement(None, &always, &sandboxed),
        Err((
            CandidateState::Unsupported,
            "container_confinement_not_advertised"
        ))
    );
    sandboxed
        .capabilities
        .push(aq_session_protocol::Capability {
            id: aq_session_protocol::Text::new("aq.harness.container-confinement").unwrap(),
            version: 1,
        });
    assert_eq!(
        admit_confinement(None, &always, &sandboxed),
        Ok(ConfinementMode::Container)
    );
    // The docker-local Codex adapter is a node sandbox adapter on the same
    // terms; the interactive Codex adapter is not.
    let mut codex = sandboxed.clone();
    codex.adapter.id = aq_session_protocol::Text::new(NODE_SANDBOX_CODEX_ADAPTER).unwrap();
    assert_eq!(
        admit_confinement(None, &always, &codex),
        Ok(ConfinementMode::Container)
    );
    codex
        .capabilities
        .retain(|c| c.id.as_str() != "aq.harness.container-confinement");
    assert_eq!(
        admit_confinement(None, &always, &codex),
        Err((
            CandidateState::Unsupported,
            "container_confinement_not_advertised"
        ))
    );
    let mut interactive = sandboxed.clone();
    interactive.adapter.id =
        aq_session_protocol::Text::new("aq.codex-result-local-adapter").unwrap();
    assert_eq!(
        admit_confinement(None, &always, &interactive),
        Err((
            CandidateState::Unsupported,
            "container_required_adapter_unqualified"
        ))
    );
}

#[test]
fn contradictory_confinement_is_denied_on_qualified_adapters() {
    let mut sandboxed = installed("codex");
    sandboxed.adapter.id = Text::new(NODE_SANDBOX_ADAPTER).unwrap();
    sandboxed
        .capabilities
        .push(aq_session_protocol::Capability {
            id: Text::new("aq.harness.container-confinement").unwrap(),
            version: 1,
        });
    for contradictory in [
        ExecutionConfinement {
            permit_container: true,
            require_native: true,
            require_container: true,
        },
        ExecutionConfinement {
            permit_container: false,
            require_native: false,
            require_container: true,
        },
    ] {
        assert_eq!(
            admit_confinement(None, &contradictory, &sandboxed),
            Err((
                CandidateState::PolicyDenied,
                "execution_confinement_contradictory"
            ))
        );
    }
}
type PlacementMutation = (fn(&mut Placement), &'static str);

#[test]
fn fence_recheck_refuses_frozen_choice_with_selection_reasons() {
    let (catalog, mut input) = fixture();
    input.placement = Some(sandbox_placement(&input, "fixture-1"));
    let choice = select(&catalog, &input).unwrap().resolved.unwrap().choice;
    assert_eq!(recheck(input.placement.as_ref().unwrap(), &choice), Ok(()));
    let mutations: Vec<PlacementMutation> = vec![
        (
            |p| p.ceiling = Some(NodeCeiling::Concurrency),
            "node_concurrency_ceiling_reached",
        ),
        (
            |p| p.ceiling = Some(NodeCeiling::SpendUnaccounted),
            "node_spend_unaccounted",
        ),
        (
            |p| p.ceiling = Some(NodeCeiling::DiskPressure),
            "node_disk_pressure",
        ),
        (
            |p| p.required_capabilities = vec!["nested-docker".into()],
            "required_backend_capability_unsupported",
        ),
        (
            |p| p.environment_templates.clear(),
            "environment_template_unresolved",
        ),
        (
            |p| p.environment_templates[0].template = Digest::of(b"other-template"),
            "environment_template_digest_mismatch",
        ),
        (|p| p.templates.clear(), "template_not_advertised"),
        (
            |p| {
                for harness in &mut p.templates[0].harnesses {
                    harness.version = "fixture-2".into();
                }
            },
            "template_missing_selected_harness_version",
        ),
        (
            |p| p.preferred_peer = Some(BackendKind::DockerLocal),
            "placement_preferred_backend_available",
        ),
    ];
    for (mutate, reason) in mutations {
        let mut placement = sandbox_placement(&input, "fixture-1");
        mutate(&mut placement);
        // The fence refuses the frozen choice with the reason selection would
        // now record for it.
        assert_eq!(recheck(&placement, &choice), Err(reason));
        let mut now = input.clone();
        now.placement = Some(placement);
        let out = select(&catalog, &now).unwrap();
        assert!(out.resolved.is_none());
        assert!(
            out.traces
                .iter()
                .all(|t| t.reason_codes.contains(&reason.into()))
        );
    }
}
