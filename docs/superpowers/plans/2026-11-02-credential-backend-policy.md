# Credential Backend Policy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Enforce operator credential backend allowlists at selection, provisioning and lease issuance.

**Architecture:** Define closed credential-kind and backend enums in aq-selection. Startup CredentialConfig owns kind and backend policy, copying the latter to AuthorizedCredential. CapabilitySnapshot carries an optional trusted backend kind; existing producers supply None until C4 connects advertisements. A shared policy function returns explicit unknown/disallowed reasons. All supported backends are allowed by default, including legacy unknown nodes; an empty or narrower allowlist fails closed for unknown nodes.

**Tech Stack:** Rust, serde, schemars, Cargo, PostgreSQL test fixtures.

**Spec:** Task calm-delta-18.12, C5a1 contract in docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml.

## Global Constraints

- Rust uses the committed lockfile with `--locked`; no sibling dependencies.
- No secrets in templates, manifests, prompts, logs, assignment JSON or task descriptions.
- Unsupported required capabilities refuse explicitly; nothing silently degrades.
- Do not edit docs/architecture or docs/specs; document proposed changes in evidence.
- Kind-specific behavior, permits, write-back and advertisement consumption are out of scope.
- Run formatting, clippy and task-focused Cargo tests through AQ resource controls. Full-suite validation belongs to CI under the worker prime policy.
- Record exact commands, environment, commit and remaining limits in docs/implementation/C5a1.md.
- Obtain cross-family adversarial review through the supervisor before passing close; publish through the assigned branch and PR policy.

### Task 1: Closed policy types and test-first enforcement

**Files:** crates/aq-selection/src/{credential_policy.rs,lib.rs,domain.rs,candidate_core.rs,select.rs,central.rs,validate.rs}; tests/{selection.rs,common/mod.rs}.

**Interfaces:** BackendKind::{DockerLocal,Boat,CloudflareContainers}; CredentialKind::{ApiKey,SubscriptionLogin}; default_allowed_backends() -> Vec<BackendKind>; credential_backend_policy(&[BackendKind], Option<BackendKind>) -> Result<(), &'static str>. AuthorizedCredential.allowed_backends is derived server authority, CapabilitySnapshot.backend_kind is trusted eligibility input. CoreInputs carries the same backend kind.

- [x] Add and run the named failing test:
```rust
#[test]
fn credential_restricted_backend_is_ineligible() {
    let (catalog, mut input) = fixture();
    input.node.backend_kind = Some(BackendKind::Boat);
    for c in &mut input.credentials {
        c.allowed_backends = vec![BackendKind::DockerLocal];
    }
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(out.traces.iter().all(|t| t.state == CandidateState::PolicyDenied));
    assert!(out.traces[0].reason_codes.contains(&"credential_backend_not_allowed".into()));
}
```
- [x] Run `cargo test --locked -p aq-selection credential_restricted_backend_is_ineligible`; confirm red.
- [x] Add closed kebab-case enums, default all-three backend vector, and a shared predicate. Check policy immediately after credential enabled status and before delivery. Return `credential_backend_unknown` for unknown restricted input and `credential_backend_not_allowed` for a known denied backend. Wire worker snapshot and central unknown input.
- [x] Cover allowed backend, unknown restricted/default, empty list, both kinds, and explicit fallback semantics. Validate at most three distinct backend entries in pure snapshots.
- [x] Run aq-selection area tests and commit implementation with Task 2.

### Task 2: Startup registry and material authority

**Files:** crates/aq-server/src/intelligence/{config.rs,registry.rs,inventory.rs}; src/profiled_execution/{material.rs,material/issuance.rs,preparation.rs,accepted_preparation/accept.rs,presence/private_inventory.rs,presence/inventory.rs}; existing CredentialConfig/AuthorizedCredential fixtures; tests/{intelligence.rs,native_material_issuance.rs}.

**Interfaces:** CredentialConfig.kind defaults ApiKey for compatibility; allowed_backends defaults all supported backends. `load` copies configured allowed_backends into the authorized snapshot. Reject customized nested derived policy in the input credential. Provisioning and issuance accept Option<BackendKind>; existing callers pass None pending C4.

- [x] Add configuration tests for unknown values, duplicates, length bound, defaults and both kind strings. Add material tests where a restricted backend or missing identity cannot provision or issue a lease, while matching backend can.
- [x] Implement after the failing selection test. Material tests were added before the implementation, but their first completed run was green; no material-specific red is claimed:
```rust
if credential_backend_policy(&configured.allowed_backends, backend_kind).is_err() {
    return Err(MaterialError::Scope);
}
```
Use the same check before provisioning reads. Include kind and allowlist in registry configuration digest; changing either requires increasing config revision. Test digest changes without a database, and use existing revision integration coverage where available.
- [x] Update existing literals with defaults; preserve old JSON decoding defaults. Regenerate owner schemas/fixtures if serialization output changes.
- [x] Run `cargo test --locked --workspace credential_restricted_backend_is_ineligible --test selection`, aq-selection tests, server intelligence/material unit and integration suites. Confirm the named test ran, rather than accepting zero matches.

### Task 3: Evidence, publication and review

- [x] Run `cargo fmt --check`, scoped locked clippy with warnings denied, and required focused checks through the AQ test semaphore using a temporary pytest Cargo adapter with one worker.
- [x] Record results, limits and downstream C4 input wiring in docs/implementation/C5a1.md.
- [ ] Squash own unreviewed feature history to one commit on its original base. Merge origin/dev, resolve if needed and repeat targeted tests for any changes.
- [ ] Push via guarded aq git push; request supervisor cross-family review of exact pushed SHA (maximum three rounds). Resolve findings, revalidate, push and seek fresh review if code changes.
- [ ] Open the required PR, settle task subtasks and close with concrete changes, verification and test evidence.
