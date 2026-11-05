# C4 Environment Templates, Backend Capabilities and Placement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Resolve every execution environment to exactly one allowlisted template digest at catalog activation. Pin that digest in the frozen launch manifest. Use the B1 backend advertisement for eligibility, ceilings and placement preference, and explain every refusal in `CandidateTrace.reason_codes`.

**Architecture:**
- Activation reads the C3b allowlist from a new optional `[templates]` config.
- It resolves each `Execution.environments` entry to one digest and stores the result per catalog digest (migration 0054).
- At selection time the resolver builds optional `Placement` facts for nodes that have a B1 registration. The inputs are the registration, the stored resolution, ceiling state and a higher-preference peer check.
- aq-selection's `evaluate` turns these facts into typed reasons.
- The manifest pin needs no new contract field. Selection requires the node-reported `InstalledHarness.environment.digest` to equal the resolved template digest, so `ConcreteChoice.environment`, and with it `LaunchManifest.harness.environment.digest`, carries it.

**Tech Stack:** Rust, axum, sqlx/PostgreSQL, serde. Vendored aq-admission 0.3.2 (`sandbox::registration`) and aq-session-protocol 0.1.0.

**Spec:** `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml` (C4), worker spec 0005 §3 and §8.2, aq-worker `packaging/sandbox/templates.md` (C3b allowlist).

## Global Constraints

- The default placement order is `docker-local`, then `boat`, then `cloudflare-containers`. A project may override it.
- A missing required capability refuses with a typed reason and never degrades.
- The template must contain the selected harness CLI version.
- A node at its concurrency or spend ceiling gets no offer, and the trace carries a typed reason.
- Any placement hold is bounded and explained.
- Arbitrary images are not allowed. Running attempts keep their digest.
- There are no secrets in templates, manifests or logs.
- Do not edit `docs/architecture/*` or `docs/specs/*`.
- Out of scope: B5a node-side refusal, C5a1 credential backend lists, C5b leases, the B1 wire schema, and H2 spend accounting.

## Decisions

- **Digest pin (no contract change).** The environment `ArtifactRef.digest` carries the template image digest as 64-hex (the `sha256:` prefix is stripped). CROSS-REPO FOLLOW-UP (aq-worker): sandbox nodes must report `InstalledHarness.environment.digest` = template digest.
- **No `[templates]` configured.** Activation stores no resolution. Nodes with a B1 advertisement are then ineligible with `environment_template_unresolved`, which is a refusal, not degradation. Host-native workers without a B1 registration keep their current behaviour.
- **Spend.** H2 owns accounting. Until it lands, spend is `None`. A configured spend ceiling with unknown spend refuses with `node_spend_unaccounted`.
- **Hold bound.** The hold applies only while a higher-preference peer has an unexpired capability snapshot, is under its ceilings, and advertises the required capabilities and the environment's template. It therefore ends no later than that peer's snapshot TTL.

## Reason codes (exact strings)

`environment_template_missing`, `environment_template_ambiguous` (activation, 409); `node_concurrency_ceiling_reached`, `node_spend_ceiling_reached`, `node_spend_unaccounted`, `placement_preferred_backend_available` (Unavailable); `required_backend_capability_unsupported`, `environment_template_unresolved`, `environment_template_digest_mismatch`, `template_not_advertised`, `template_missing_selected_harness_version` (Unsupported).

## Review Focus

- A bare environment name that matches two allowlist versions must refuse as ambiguous, not pick one. Covered by the Task 1 unit test.
- A `name@version` entry with the same name and version listed twice under different digests must refuse as ambiguous. Covered by Task 1.
- A project override that omits a backend kind must rank the omitted kind after the listed kinds. It does not exclude it. Covered by Task 3.
- Old persisted decisions and snapshots with no `placement` key must still deserialize. `serde(default)` and skip-if-none, covered by Task 2.
- Enabling templates without the aq-worker digest follow-up makes every sandbox node ineligible. This is documented in the evidence as a limit.

---

### Task 1: Allowlist, activation resolution and storage

**Files:**
- Create: `crates/aq-server/src/templates.rs`, `crates/aq-server/migrations/0054_environment_templates.sql`
- Modify: `crates/aq-server/src/config.rs` (`templates` field), `crates/aq-server/src/lib.rs` (mod), `crates/aq-server/src/store.rs` (migration 54, `STATE_FORMAT_VERSION`), `crates/aq-server/src/profile_catalog.rs` (`activate`)
- Test: unit tests in `templates.rs` and `crates/aq-server/tests/profile_catalog.rs`

**Interfaces (produces):**
- `pub struct Allowlist { entries: Vec<Entry> }` with `Entry { name: String, version: String, digest: aq_admission::Digest }`
- `pub fn Allowlist::parse(bytes: &[u8]) -> Result<Allowlist, &'static str>` requires `schema_version == 1`, at most 1 MiB and 1024 entries, and `image_digest` matching `sha256:<64 hex>`. Unknown entry fields are ignored because aq-worker owns provenance.
- `pub struct Resolution { pub environment: String, pub template: aq_admission::Digest }`
- `pub fn resolve(environments: impl IntoIterator<Item=&str>, allowlist: &Allowlist) -> Result<Vec<Resolution>, &'static str>`. A `name@version` entry must match name and version exactly. A bare `name` matches by name. Zero matches gives `environment_template_missing`. More than one distinct digest gives `environment_template_ambiguous`.
- `pub async fn store(tx, catalog_digest: &str, &[Resolution])` deletes then inserts. `pub async fn load(pool, catalog_digest) -> Result<Vec<Resolution>, AppError>`.
- Config: `[templates] allowlist = "<path>"` becomes `Config.templates: Option<Arc<Allowlist>>`, loaded at deserialize time.

- [ ] Step 1: Write the failing unit test `resolves_each_environment_to_exactly_one_digest` (exact match, bare match, missing, ambiguous-by-version, ambiguous-by-duplicate). Add the integration test `activation_pins_environment_template_or_refuses`:
  - With an allowlist that lacks `confined-worktree`, activation returns 409 with `environment_template_missing`.
  - With the entry present, activation returns 200 and `environment_templates[0].template_digest` is the hex.
- [ ] Step 2: Run `cargo test --locked -p aq-server resolves_each_environment` and confirm it fails to compile.
- [ ] Step 3: Implement. Migration 0054 creates `profile_catalog_environment_templates(catalog_digest TEXT REFERENCES profile_catalog_artifacts(digest), environment TEXT, template_digest TEXT CHECK hex64, PRIMARY KEY(catalog_digest, environment))` and sets `state_format_version` to 54. `activate` resolves after `prepare_activation` and before the registry update when `config.templates` is set, then stores the rows and adds `environment_templates` to the response.
- [ ] Step 4: Run both tests and confirm they pass.
- [ ] Step 5: Commit.

### Task 2: Selection placement facts and typed reasons

**Files:**
- Modify: `crates/aq-selection/src/domain.rs`, `candidate_core.rs`, `select.rs`, `central.rs` (`placement: None`), `validate.rs`
- Test: `crates/aq-selection/tests/selection.rs`

**Interfaces (produces):**
- `SelectionInputs.placement: Option<Placement>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`)
- `pub struct Placement { pub backend: BackendKind, pub capabilities: Vec<String>, pub required_capabilities: Vec<String>, pub templates: Vec<AdvertisedTemplate>, pub environment_templates: Vec<EnvironmentTemplate>, pub ceiling: Option<NodeCeiling>, pub preferred_peer: Option<BackendKind> }`
- `pub struct AdvertisedTemplate { pub digest: Digest, pub harnesses: Vec<AdvertisedHarness> }`, `pub struct AdvertisedHarness { pub harness: String, pub version: String }`, `pub struct EnvironmentTemplate { pub environment: String, pub template: Digest }`
- `pub enum NodeCeiling { Concurrency, Spend, SpendUnaccounted }` with `fn reason(self) -> &'static str`
- `CoreInputs.placement: Option<&Placement>`. The backend kind is `node.backend_kind.or(placement.backend)`.
- Check order after the freshness check: ceiling, then preferred_peer, then required caps, then environment resolution (by `id@version` or `id`), then digest equality, then advertised template, then harness version.

- [ ] Step 1: Write the failing tests:
  - `template_missing_selected_harness_version_is_ineligible`: the template advertises the harness at another version. Expect no resolution, a trace state of `Unsupported`, and `reason_codes` containing `template_missing_selected_harness_version`.
  - `node_at_spend_ceiling_refuses_with_reason_in_trace`: `ceiling: Some(Spend)` puts `node_spend_ceiling_reached` in every trace.
  - The matching template is eligible, and the choice's environment digest equals the template digest.
- [ ] Step 2: Run `cargo test --locked -p aq-selection template_missing_selected_harness_version_is_ineligible` and confirm it fails.
- [ ] Step 3: Implement. In `validate.rs`, enforce at most 64 templates, 32 harnesses per template, 64 environment templates and 16 capabilities, plus the text bounds.
- [ ] Step 4: Run `cargo test --locked -p aq-selection` and confirm it passes.
- [ ] Step 5: Commit.

### Task 3: Placement preference, ceilings and resolver wiring

**Files:**
- Create: `crates/aq-server/src/profiled_execution/placement.rs`
- Modify: `crates/aq-server/src/profiled_execution/config.rs` (`ProjectPolicy.placement`), `crates/aq-server/src/intelligence/resolver.rs` (`placement` field), and `ProjectPolicy` literals (`placement: None`)

**Interfaces (produces):**
- `ProjectPolicy.placement: Option<PlacementPolicy>` (serde default, skip-if-none, so configuration digests are unchanged)
- `pub struct PlacementPolicy { pub preference: Vec<BackendKind>, pub required_capabilities: Vec<BackendCapability> }`, validated as distinct, at most 3 kinds and at most 4 capabilities
- `pub const DEFAULT_PREFERENCE: [BackendKind; 3] = [DockerLocal, Boat, CloudflareContainers]`
- `pub fn ceiling(adv: &BackendAdvertisement, active: u32, spend: Option<&Money>) -> Option<NodeCeiling>`. `active >= max` gives Concurrency. With a ceiling set, a spend in the same currency that is at or above the ceiling gives Spend, and None or another currency gives SpendUnaccounted.
- `pub struct Peer { pub kind: BackendKind, pub eligible: bool }` and `pub fn preferred_peer(own: BackendKind, preference: &[BackendKind], peers: &[Peer]) -> Option<BackendKind>`. This returns the best-ranked eligible peer kind ranked strictly above `own`. A kind not in the list ranks after all listed kinds.
- `pub async fn facts(state, node_id, catalog_digest, project_id, environment) -> Result<Option<aq_selection::Placement>, AppError>`. With no `delegated_node_boots` row it returns None. Peers are other registered nodes (at most 256) whose snapshot is unexpired, that are under their ceilings, and that have the required caps and the environment's template.

- [ ] Step 1: Write the failing unit tests:
  - `placement_prefers_local_then_boat_then_cloudflare`: by default, local gets no hold and boat and cloudflare are held for local. With local ineligible, boat runs and cloudflare is held for boat. The project override `[cloudflare-containers, boat, docker-local]` reverses this.
  - `node_at_spend_ceiling_refuses_with_reason`: spend of 100 against a ceiling of 100 gives `Spend` (`node_spend_ceiling_reached`), 99 gives None, None gives `SpendUnaccounted`, and `active == max` gives `Concurrency`.
- [ ] Step 2: Run `cargo test --locked -p aq-server placement_prefers_local_then_boat_then_cloudflare` and confirm it fails.
- [ ] Step 3: Implement, and wire `placement: facts(...)` into `resolver.rs`'s `SelectionInputs`.
- [ ] Step 4: Run the three acceptance tests with `--workspace` and confirm they pass.
- [ ] Step 5: Commit.

### Task 4: Definition of done and evidence

- [ ] Run `cargo fmt --check` and `cargo clippy --locked --all-targets -- -D warnings` for aq-selection and aq-server. Run the targeted `cargo test --locked` tests from Tasks 1–3.
- [ ] Write `docs/implementation/C4.md` (commands, environment, commit, limits, cross-repo follow-up, proposed design-doc notes).
- [ ] Squash, run `git merge origin/main`, then `aq git push`.
