//! MS1 review: the long (schema 3) phase admits interactive Codex and the
//! docker-local Claude or Codex manifest only when it declares container
//! confinement.

use aq_admission::preparation_v2::PhasePolicy;
use aq_session_protocol::{Capability, LaunchManifest, Text};

fn manifest() -> LaunchManifest {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../artifacts/preparation-v2.fixture.json")).unwrap();
    serde_json::from_value(fixture["proposal"]["original"]["offered"]["offer"]["manifest"].clone())
        .unwrap()
}
fn long() -> PhasePolicy {
    PhasePolicy {
        schema_version: 3,
        maximum_preparation_ms: 30000,
        maximum_execution_lease_ms: 900000,
        maximum_observation_ms: 2000,
    }
}
fn claude(capabilities: &[&str]) -> LaunchManifest {
    let mut m = manifest();
    m.harness.harness = Text::new("claude").unwrap();
    m.harness.cli_version = Text::new("2.1.318").unwrap();
    m.harness.adapter.id = Text::new("aq.claude-result-local-adapter").unwrap();
    m.terminal.input_enabled = false;
    m.harness.required_capabilities = capabilities
        .iter()
        .map(|id| Capability {
            id: Text::new(*id).unwrap(),
            version: 1,
        })
        .collect();
    m
}

#[test]
fn long_phase_claude_manifest_requires_container_confinement() {
    let container = "aq.harness.container-confinement";
    let native = "aq.harness.confined-worktree";
    let base = ["aq.harness.startup-context", "aq.harness.stop"];
    let with = |extra: &[&str]| claude(&[&base[..], extra].concat());
    assert!(long().validate_for_manifest(&with(&[container])).is_ok());
    assert!(
        long().validate_for_manifest(&with(&[])).is_err(),
        "no confinement"
    );
    assert!(
        long().validate_for_manifest(&with(&[native])).is_err(),
        "native"
    );
    assert!(
        long()
            .validate_for_manifest(&with(&[container, native]))
            .is_err(),
        "both"
    );
    let mut input = with(&[container]);
    input.terminal.input_enabled = true;
    assert!(
        long().validate_for_manifest(&input).is_err(),
        "terminal input"
    );
    let mut other = with(&[container]);
    other.harness.cli_version = Text::new("2.1.319").unwrap();
    assert!(long().validate_for_manifest(&other).is_err(), "other CLI");
}

fn codex_sandbox(capabilities: &[&str]) -> LaunchManifest {
    let mut m = claude(capabilities);
    m.harness.harness = Text::new("codex").unwrap();
    m.harness.cli_version = Text::new("0.166.0").unwrap();
    m.harness.adapter.id = Text::new("aq.codex-docker-result-local-adapter").unwrap();
    m
}

#[test]
fn long_phase_docker_local_codex_manifest_requires_exact_adapter_and_container_confinement() {
    let container = "aq.harness.container-confinement";
    let native = "aq.harness.confined-worktree";
    let base = ["aq.harness.startup-context", "aq.harness.stop"];
    let with = |extra: &[&str]| codex_sandbox(&[&base[..], extra].concat());
    assert!(long().validate_for_manifest(&with(&[container])).is_ok());
    assert!(long().validate_for_manifest(&with(&[])).is_err(), "none");
    assert!(
        long().validate_for_manifest(&with(&[native])).is_err(),
        "native"
    );
    assert!(
        long()
            .validate_for_manifest(&with(&[container, native]))
            .is_err(),
        "both"
    );
    let mut input = with(&[container]);
    input.terminal.input_enabled = true;
    assert!(
        long().validate_for_manifest(&input).is_err(),
        "terminal input"
    );
    let mut other = with(&[container]);
    other.harness.cli_version = Text::new("0.167.0").unwrap();
    assert!(long().validate_for_manifest(&other).is_err(), "other CLI");
    // The adapter identity is exact: neither the interactive Codex adapter
    // nor the Claude adapter admits a container-confined Codex manifest,
    // and the Codex docker adapter does not admit Claude.
    for adapter in [
        "aq.codex-result-local-adapter",
        "aq.claude-result-local-adapter",
        "aq.codex-docker-result-local-adapter-x",
    ] {
        let mut wrong = with(&[container]);
        wrong.harness.adapter.id = Text::new(adapter).unwrap();
        assert!(long().validate_for_manifest(&wrong).is_err(), "{adapter}");
    }
    let mut claude_harness = with(&[container]);
    claude_harness.harness.harness = Text::new("claude").unwrap();
    assert!(
        long().validate_for_manifest(&claude_harness).is_err(),
        "claude harness"
    );
    // The Claude path is unchanged.
    assert!(
        long()
            .validate_for_manifest(&claude(&[&base[..], &[container]].concat()))
            .is_ok()
    );
}
