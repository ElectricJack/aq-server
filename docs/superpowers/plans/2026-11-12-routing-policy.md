# Routing Policy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every task under an automatic-router project is routed by a playbook that applies the owner's plain-language policy, and placement honors the route.

**Architecture:** A pure crate, `aq-routing`, holds the route, the compiled policy and the deterministic planner. The policy Markdown in Settings is compiled to that structure by a model through internal work, validated deterministically and activated at once. A server-built playbook on a new host profile (catalog 2.2.0) is started by a server-originated `aq.work.route_needed` event, plans, asks for a classification through internal work, and writes the route into the task's graph metadata. Placement substitutes the route's class profile for the automatic router and filters candidates.

**Tech Stack:** Rust (axum, sqlx/PostgreSQL), the vendored aq-playbooks engine, aq-graph over HTTP, TypeScript/React (aq-dashboard).

**Spec:** [`docs/specs/0011-routing-policy.md`](../../specs/0011-routing-policy.md). Read it, and [`0012-internal-work.md`](../../specs/0012-internal-work.md), before any task.

**Depends on:** the internal work plan ([`2026-11-12-internal-work.md`](2026-11-12-internal-work.md)). Part R1 needs none of it. Part R2 needs its tasks 1 to 5 (the service). Part R4 needs its tasks 11 and 13 (runs API, failure path). Live acceptance needs its Part I2 (logins).

**Branches:** `feat/internal-work` in aq-server, aq-dashboard and aq-playbooks.

## Global Constraints

- The policy source is plain language. No task may require the owner to write identifiers, blocks or schemas in it. Structure exists only in the compiled policy.
- A model never produces a route. It produces a compiled policy (once per save) and a classification (per task). `aq_routing::plan` is deterministic and has no I/O.
- Routing has no provider, credential or fallback code. Every model call is `InternalWork::run`/`submit` with purpose `playbook_compilation` or `routing_classification`.
- A route applies only where the project binding's intelligence profile is an automatic router (`automatic_routing == Some(true)`). A binding pinned to a named profile is never changed by a route.
- The route in graph metadata is the key `aq:route`, at most 2048 bytes serialized. Graph metadata is replaced whole on update: clone the task's metadata object and change one key, as `TaskReprioritize` does (`agent_access/human/graph_actions.rs:216-245`), with `expected_version`.
- An owner route (`source: owner`) is never overwritten by the router.
- Classes are `<parent>-<effort>` with parent in `simple|standard|complex|deep` and effort in `low|medium|high|max`. A class resolves to a profile only through the Intelligence structure assignments.
- Request and selection digests of tasks with no route must not change: every new field on a serialized selection or compile input is `#[serde(default, skip_serializing_if = …)]`.
- A saved policy that compiles takes effect immediately. One that does not compile changes nothing.
- Build health, migrations and route registration rules are as in the internal work plan's Global Constraints. The internal work plan takes migration 0119; this plan uses 0120 and 0121, renumbered before pushing if the branch moved.

## Review Focus

1. **A task is edited while it is being routed.** The route write carries the version the plan was made against; a newer task version makes the write `stale` and the task is routed again from the new content. Pinned in Task 2 and Task 12.
2. **The class a route names loses its profile.** The owner deletes or reassigns a profile after tasks were routed. Placement must refuse those tasks with a typed reason and they must be routed again, not launched on the router's default. Pinned in Task 4.
3. **The compiler returns a policy that names a level, harness or provider that does not exist.** It is refused, the previous policy keeps routing, and the owner sees which sentence failed. Pinned in Task 6 and Task 8.
4. **The classifier is down for an hour.** Tasks still route with risk assumed medium (`classification_failed`), none wait forever, and no attention item is raised per task. Pinned in Task 7 and Task 14.
5. **Hundreds of routing runs in one playbook namespace.** Tick time must stay bounded or routing stops for everyone. Measured in Task 9 before anything is built on it.

---

## File Structure

```
crates/aq-routing/                    new pure crate, no tests/ directory (unit tests in src)
  src/lib.rs                          Class, Route, CompactRoute, RouteLimits
  src/policy.rs                       Policy and its parts, validate, summary
  src/classify.rs                     Classification, its JSON Schema, Options
  src/plan.rs                         plan()
crates/aq-server/migrations/0120_routing.sql
crates/aq-server/migrations/0121_playbook_host_catalog_2_2.sql
crates/aq-server/src/routing.rs               state, route read/write, history, switch
crates/aq-server/src/routing/compile.rs       compile-on-save through internal work
crates/aq-server/src/routing/http.rs          /api/settings/routing, /api/tasks/{id}/routes
crates/aq-server/src/playbooks/routing.rs     the routing artifact and its default install
crates/aq-server/src/playbooks/host_adapters/routing.rs   routing_plan view, route variant, route_needed collector
vendor/aq-host-catalog-2.2.0/                 vendored from aq-playbooks
aq-playbooks: contracts/aq-host-2.2.0/, crates/aq-playbooks-cli/src/catalog.rs
aq-dashboard: src/features/work/inspector/TaskRoute.tsx, src/api/routing.ts,
              src/features/settings/RoutingPolicyEditor.tsx (extended)
```

---

# Part R1: the route, and placement that honors it

### Task 1: `aq-routing` crate with the route types

**Files:**
- Create: `crates/aq-routing/Cargo.toml` (deps: `serde`, `serde_json`, `sha2`, `thiserror` only), `src/lib.rs`
- Modify: root `Cargo.toml:3` members

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Parent { Simple, Standard, Complex, Deep }
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effort { Low, Medium, High, Max }
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Class { pub parent: Parent, pub effort: Effort }   // Display/FromStr/serde as "standard-high"; Ord is (parent, effort)

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum Source { Router, Owner }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum ProviderIntent { Any, Pinned }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteReason { pub code: String, pub detail: String }
// codes: hint_clamped, class_raised_for_risk, risk_relaxed, risk_assumed, escalated_after_failure,
//        deep_requires_reasoning, lane_matched, reserved_harness_removed, classification_failed, owner_set

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub class: Class, pub harnesses: Vec<String>, pub providers: Vec<String>, pub prefer: Vec<String>,
    pub lane: Option<String>, pub provider_intent: ProviderIntent, pub reasons: Vec<RouteReason>,
    pub classification: Option<classify::Classification>, pub policy_digest: Option<String>,
    pub structure_revision: Option<u64>, pub source: Source,
}
/// What is stored under `aq:route`. No reasons, no classification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactRoute { pub c: Class, #[serde(default)] pub h: Vec<String>, #[serde(default)] pub p: Vec<String>,
    #[serde(default)] pub pf: Vec<String>, pub l: Option<String>, pub i: ProviderIntent, pub d: Option<String>, pub s: Source, pub n: u64 }
impl Route { pub fn compact(&self, sequence: u64) -> Result<CompactRoute, Error>; }   // Err when the JSON exceeds 2048 bytes
impl CompactRoute { pub fn from_metadata(metadata: &serde_json::Value) -> Result<Option<Self>, Error>; pub fn limits(&self) -> RouteLimits; }
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteLimits { pub harnesses: Vec<String>, pub providers: Vec<String>, pub prefer: Vec<String>, pub pinned: bool }
```

`d` is the first 16 hex characters of the policy digest. An empty `harnesses` or `providers` means no restriction.

- [ ] **Step 1: Write the failing tests** (`#[cfg(test)] mod tests` in `src/lib.rs`)

```rust
#[test] fn class_round_trips_as_parent_dash_effort_and_orders_by_parent_then_effort()   // "standard-high" < "complex-low"
#[test] fn unknown_parent_or_effort_is_an_error()                                       // "fast-high", "standard-ultra"
#[test] fn compact_route_fits_2048_bytes_and_refuses_an_oversized_allowlist()
#[test] fn from_metadata_reads_aq_route_and_treats_a_missing_key_as_none_and_garbage_as_an_error()
```

- [ ] **Step 2: Run to verify they fail**: `timeout 900 cargo test -p aq-routing`
- [ ] **Step 3: Implement.** **Step 4: Run to verify they pass**; `python3 scripts/build-budget.py` passes.
- [ ] **Step 5: Commit** `feat(routing): route types`

### Task 2: Route storage, history and the switch

**Files:**
- Create: `crates/aq-server/migrations/0120_routing.sql`, `crates/aq-server/src/routing.rs`
- Modify: `crates/aq-server/src/lib.rs` (module), `crates/aq-profile-settings/src/structure.rs` (add a typed read)
- Test: `crates/aq-server/tests/it_access/routing.rs` (add to `it_access/main.rs`)

**Interfaces:**
- Consumes: `graph_actions.rs:71-75 update()` body shape and `:913` envelope; `agent_access::settlement_post(rt, workspace, bytes)`; `graph_actions.rs:677 current(rt, workspace, id, kind)`; the 409 `conflict` mapping at `:933-944`.
- Produces:

```rust
// structure.rs (new, beside `read`)
pub async fn document(pool: &PgPool) -> Result<(Document, u64 /*revision*/, u64 /*catalog_revision*/)>;

// routing.rs
pub enum Written { Routed { sequence: u64 }, Stale }
pub async fn write(state: &AppState, workspace: Uuid, task: Uuid, expected_version: u64, route: &Route) -> Result<Written, AppError>;
pub async fn clear(state: &AppState, workspace: Uuid, task: Uuid, expected_version: u64, by: Source) -> Result<Written, AppError>;
pub async fn history(pool: &PgPool, task: Uuid) -> Result<Vec<StoredRoute>, AppError>;   // newest first
pub struct StoredRoute { pub sequence: u64, pub route: Option<Route> /* None = cleared */, pub at_ms: i64 }
pub async fn enabled(pool: &PgPool) -> Result<bool, AppError>;
pub async fn set_enabled(pool: &PgPool, on: bool) -> Result<(), AppError>;               // refuses `true` when no policy is active
pub async fn class_profile(pool: &PgPool, catalog: &ValidatedCatalog, class: Class) -> Result<Option<String /*key id@version*/>, AppError>;
```

- Tables (0120): `task_routes(task_id UUID, sequence BIGINT, route JSONB /* null = cleared */, at_ms BIGINT, PRIMARY KEY(task_id, sequence))`; `routing_state(singleton BOOLEAN PK DEFAULT TRUE CHECK(singleton), enabled BOOLEAN NOT NULL DEFAULT FALSE, active_policy_digest TEXT)`, seeded with one row; `routing_policies(digest TEXT PK, structure_revision BIGINT, source_digest TEXT, compiled JSONB, summary JSONB, status TEXT CHECK(status IN ('active','superseded','failed')), error TEXT, internal_task_id UUID, created_at_ms BIGINT)`; `route_needed_emissions(task_id UUID PK, task_version BIGINT, emitted_at_ms BIGINT)`.
- `write`: read the node; if its `aq:route` has `s: owner` and the new route's source is `router`, return `Stale`. Insert the history row with the next sequence, then post the metadata update with `expected_version`. A graph 409 deletes that history row and returns `Stale`.
- `class_profile`: find the assignment for `(class.parent, class.effort)` in `structure::document`, then the catalog intelligence profile with that id that is not an automatic router, and return `aq_profiles::key(id, version)`.

- [ ] **Step 1: Write the failing tests** (use the fake graph the neighbouring `it_access::graph_mutations` module uses):

```rust
#[tokio::test] async fn write_sets_aq_route_keeps_other_metadata_and_records_history()
#[tokio::test] async fn write_against_an_older_task_version_is_stale_and_leaves_no_history_row()     // Review Focus 1
#[tokio::test] async fn router_never_overwrites_an_owner_route_but_the_owner_can_replace_a_router_route()
#[tokio::test] async fn clear_removes_the_key_and_records_a_cleared_entry()
#[tokio::test] async fn class_profile_follows_structure_assignments_and_is_none_for_an_unassigned_slot()
#[tokio::test] async fn routing_cannot_be_enabled_without_an_active_policy()
```

- [ ] **Step 2: Run to verify they fail**: `timeout 900 cargo nextest run -p aq-server --test it_access -E 'test(/^routing::/)'`
- [ ] **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(routing): route storage, history and the routing switch`

### Task 3: Owner sets and clears a route

**Files:**
- Modify: `crates/aq-server/src/agent_access/human/graph_actions.rs` (two actions beside `TaskReprioritize`, registered at ~567 and ~593), `crates/aq-command-registry/src/lib.rs:270` area, `crates/aq-server/src/agent_access/human/enqueue/supervisor.rs:138` area, `crates/aq-server/src/dashboard_api/service.rs:293-297`, `crates/aq-dashboard-contract/src/release_manifest.rs:55`, `protocol/dashboard-actions/v1/{schema.json,README.md,check.py,fixtures/}`, `protocol/commands/v1/commands.json`
- Test: `crates/aq-server/tests/it_dashboard/dashboard_task_knowledge.rs`, `crates/aq-server/src/dashboard_api/actions/tests.rs`

**Interfaces:**
- Consumes: `routing::{write, clear}`; the `TaskReprioritize` action as the template for every file above.
- Produces: action kinds `task.route.set` with fields `{class: "<parent>-<effort>", harnesses?: string[], providers?: string[]}` and `task.route.clear` with no fields; capability `tasks.route`. `task.route.set` builds a `Route` with `source: Owner`, one reason `owner_set`, no policy digest. It is refused (422) when the class has no assigned profile.

- [ ] **Step 1: Write the failing tests**: `task_route_set_writes_an_owner_route_and_is_refused_for_an_unassigned_class`, `task_route_clear_removes_it`, `stale_task_version_is_reported_as_stale`, plus the contract fixtures `request-task-route-set.json`, `request-task-route-clear.json` and two entries in `fixtures/invalid.json` (unknown class, unknown field).
- [ ] **Step 2: Run to verify they fail.**
- [ ] **Step 3: Implement.** Regenerate: `cargo run -p aq-server --example dashboard_actions_contract --offline`, then `AQ_REGENERATE_COMMANDS=1 timeout 900 cargo nextest run -p aq-server --test it_server -E 'test(/^commands::/)'`.
- [ ] **Step 4: Run to verify they pass**, plus `cargo run -p aq-server --example dashboard_actions_contract --offline -- --check` and `python3 protocol/dashboard-actions/v1/check.py`.
- [ ] **Step 5: Commit** `feat(routing): owner sets and clears a task's route`

### Task 4: Placement honors the route

**Files:**
- Modify: `crates/aq-server/src/profiled_execution/producer/workflow.rs` (ready loop ~474-513), `producer/compose.rs:32-103,202-214`, `crates/aq-profiles/src/compile.rs:183-185,227`, `crates/aq-server/src/project_bindings.rs:522-525`, `crates/aq-selection/src/domain.rs` (`SelectionInputs`), `src/select.rs:175-184`, `src/candidate_core.rs:209-263,488-501`, `crates/aq-server/src/intelligence/resolver.rs:17,312-316`
- Test: `crates/aq-server/tests/it_native/native_offers/` (new module `routed.rs`, listed in that module's `mod` file); `crates/aq-selection` unit tests beside `candidate_core`

**Interfaces:**
- Consumes: `CompactRoute::from_metadata`, `RouteLimits`, `routing::{enabled, class_profile}`.
- Produces:
  - `ResolveRequest`, `SelectionInputs`, `CompileRequest`'s `ProfileRequest` and `EligibilityRequest` each gain `#[serde(default, skip_serializing_if = "Option::is_none")] pub intelligence_override: Option<String>`; `SelectionInputs` also gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub route: Option<RouteLimits>`.
  - Wherever the intelligence profile was taken from `agent.intelligence`, it is now `intelligence_override.as_deref().unwrap_or(&agent.intelligence)`: `compose.rs:65-75,90-103`, `compile.rs:183-185,227`, `project_bindings.rs:522-525`, `select.rs:175-184`, and the `classify` call at `resolver.rs:312-316`.
  - `compose::compile` gains a parameter `route: Option<&CompactRoute>`. When the agent's intelligence is an automatic router and `route` is `Some`, the override is `routing::class_profile(route.c)`. A binding that allows the automatic router is treated as allowing every class profile, for the check at `compose.rs:65-75` only.
  - `candidate_core::evaluate`: a candidate whose harness is not in a non-empty `route.harnesses` is `(PolicyDenied, "route_excludes_harness")`; a provider not in a non-empty `route.providers` is `(PolicyDenied, "route_excludes_provider")`. Candidates whose harness is in `route.prefer` are tried first, otherwise profile order is kept. With `route.pinned`, falling back past the first allowed provider is refused with outcome code `route_provider_pinned`.
- Ready loop rules, applied per candidate task before `scheduling_context_current`:
  - Binding not on an automatic router: unchanged behavior, route ignored.
  - Routing off and no `aq:route`: unchanged behavior.
  - Routing on and no `aq:route`: skip the task (not offered). No hold is recorded.
  - `aq:route` present but unreadable, or its class has no profile: skip the task, call `routing::clear(..., Source::Router)` so it is routed again, and record the refusal reason `route_class_unassigned` through the existing `holds::selection_refused` path only if the task has an attempt.

- [ ] **Step 1: Write the failing tests**

```rust
// aq-selection
#[test] fn route_harness_and_provider_allowlists_deny_other_candidates_with_typed_codes()
#[test] fn preferred_harness_is_tried_first_and_profile_order_is_kept_otherwise()
#[test] fn pinned_route_refuses_fallback_with_route_provider_pinned()
#[test] fn inputs_without_a_route_serialize_exactly_as_before()            // golden JSON of an existing fixture is unchanged
// it_native::native_offers::routed
#[tokio::test] async fn routed_task_is_offered_on_the_class_profile_not_the_router()         // selection's intelligence_profile == assigned key
#[tokio::test] async fn unrouted_task_is_not_offered_when_routing_is_on_and_is_offered_when_off()
#[tokio::test] async fn route_is_ignored_for_a_binding_pinned_to_a_named_profile()
#[tokio::test] async fn route_whose_class_lost_its_profile_is_cleared_and_not_launched_on_the_router()   // Review Focus 2
#[tokio::test] async fn route_excluding_every_candidate_is_refused_and_no_offer_is_published()
```

- [ ] **Step 2: Run to verify they fail**: `timeout 900 cargo test -p aq-selection` and `timeout 900 cargo nextest run -p aq-server --test it_native -E 'test(/^native_offers::routed::/)'`
- [ ] **Step 3: Implement.** The `routed` tests build selection fixtures, so add them to the `selection-fixture` group filter in `.config/nextest.toml`.
- [ ] **Step 4: Run to verify they pass**, then the whole `it_native` binary and `it_profiled` (digest stability).
- [ ] **Step 5: Commit** `feat(placement): honor a task's route for automatic-router projects`

### Task 5: The task shows its route

**Files:**
- aq-server: `crates/aq-human-work/src/lib.rs:188-211`, `crates/aq-server/src/agent_access/human/work.rs:75-79`, `protocol/human-graph/v1/{schema.json,README.md}`, `crates/aq-server/src/routing/http.rs` (create), `protocol/commands/v1/routes.json`
- aq-dashboard: `src/features/work/taskWork.ts:42-75`, create `src/features/work/inspector/TaskRoute.tsx` (+ test), modify `src/features/work/inspector/TaskStatus.tsx`, `src/features/actions/actionClient.ts:5-12`, `src/features/actions/RecordActions.tsx:72,140,205`, re-pin `dashboard-actions`
- Test: `it_dashboard` human work module; `src/features/work/taskWork.test.ts`, `TaskInspector.test.tsx`, `RecordActions.test.tsx`, `tests/work-inspector.spec.ts`

**Interfaces:**
- Produces (server): the task work read gains an optional `routing` object: `{"state":"routed","route":{class,harnesses,providers,prefer,lane,provider_intent,source,policy_digest},"since_ms":…}` or `{"state":"waiting","since_ms":…}` or absent (binding not on a router, or routing off with no route). `since_ms` is the newest `task_routes.at_ms`, or the task's ready time when waiting. `GET /api/tasks/{id}/routes` (`RequireOwner`, class `owner_admin`) returns `routing::history` with reasons and classification.
- Produces (dashboard): `TaskWork.routing?: { state: "routed" | "waiting"; route?: TaskRouteView; since_ms: number }`; `export function TaskRoute({ routing, taskId }: …)` renders "Waiting for routing · 4 min" or "Standard · High" with the lane, who may take it, and a disclosure that loads `/routes` and lists each reason's `detail`. The owner actions "Set route" (a class select listing assigned classes, optional harness and provider checkboxes) and "Clear route" appear in `RecordActions` under capability `tasks.route`.

- [ ] **Step 1: Write the failing tests** (server: `work_read_reports_waiting_then_routed`, `routes_history_lists_reasons_newest_first`; dashboard: adapter accepts and rejects `routing` shapes, component states, action payloads, one Playwright case setting and clearing a route on a fixture task).
- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** Re-pin with `scripts/pin-contract.py ../aq-server dashboard-actions <commit>`.
- [ ] **Step 4: Run to verify they pass**; `pnpm typecheck && pnpm lint`.
- [ ] **Step 5: Commit** in each repository: `feat(routing): show and edit a task's route`

---

# Part R2: the compiled policy and the planner

### Task 6: Compiled policy, validation and the plain summary

**Files:**
- Create: `crates/aq-routing/src/policy.rs`, `src/classify.rs`

**Interfaces:**
- Produces:

```rust
// classify.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum Risk { Low, Medium, High, VeryHigh }
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum Flag { Narrow, TestVerified, DeepReasoning }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] #[serde(deny_unknown_fields)]
pub struct Classification { pub task_type: String, pub intelligence_class: Class, pub deep_reasoning: bool, pub narrow: bool,
    pub test_verified: bool, pub risk: Risk, pub risk_reason: String /* <=400 */, pub reason: String /* <=400 */ }
pub fn schema(allowed_kinds: &[String], allowed_classes: &[Class]) -> serde_json::Value;   // JSON Schema for AnswerShape::Json
/// What Settings currently offers.
pub struct Options { pub classes: BTreeMap<Class, Vec<Offer>> }      // only classes with an assigned profile
pub struct Offer { pub provider: String, pub harness: String }       // one per profile candidate harness that is qualified

// policy.rs
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)] #[serde(deny_unknown_fields)]
pub struct Policy { pub version: u32 /* 2 */, pub default_kind: String, pub kinds: BTreeMap<String, KindRule>,
    #[serde(default)] pub origins: BTreeMap<String, OriginRule>, pub risk: BTreeMap<Risk, RiskRule>,
    #[serde(default)] pub lanes: Vec<Lane>, #[serde(default)] pub reserved: Vec<Reserved>, pub classifier_instructions: String }
pub struct KindRule { pub class: Class, pub max_class: Class, pub lane: Option<String>, #[serde(default)] pub narrow: bool,
    #[serde(default)] pub prefer_harnesses: Vec<String>, pub escalate: Option<Escalate> }
pub struct Escalate { pub after_failures: u32, pub to_class: Class }
pub struct OriginRule { pub narrow: Option<bool>, pub lane: Option<String> }
pub struct RiskRule { pub min_class: Class, pub harnesses: Vec<String>, pub relax: Option<Relax> }
pub struct Relax { pub class: Class, pub requires: Vec<Flag> }
pub struct Lane { pub id: String, pub class: Option<Class>, #[serde(default)] pub classes: BTreeMap<Class, Class>,
    #[serde(default)] pub harnesses: Vec<String>, #[serde(default)] pub providers: Vec<String>, #[serde(default)] pub prefer: Vec<String>,
    #[serde(default)] pub hold: bool, #[serde(default)] pub requires: Vec<Flag>, pub max_risk: Option<Risk>, #[serde(default)] pub cheap: bool }
pub struct Reserved { pub classes: Vec<Class>, pub harness: String, pub only_lanes: Vec<String> }

pub struct Problem { pub path: String, pub message: String }
pub fn validate(policy: &Policy, options: &Options) -> Vec<Problem>;
pub fn digest(policy: &Policy) -> String;                 // "sha256:" + hex of canonical JSON
pub fn summary(policy: &Policy) -> Vec<String>;           // plain sentences for the owner
```

- `validate` reports: `version != 2`; `default_kind` not in `kinds`; any class with no entry in `options.classes`; `class > max_class`; a `lane` reference to an unknown lane id; duplicate lane ids; a harness or provider that appears in no `Offer`; a lane with neither `class` nor `classes`; `relax.class > min_class`; `classifier_instructions` empty or over 32 KiB; no `risk` entry for `Medium`.
- `summary` produces one sentence per kind, risk level and lane, in the owner's vocabulary, for example `Bug fixes: Standard, high effort; Complex, high effort after 1 failed attempt.` and `High risk: at least Standard, high effort; Claude or Codex only.` Parent and effort are spelled as words; harness ids are capitalised (`claude` to `Claude`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test] fn the_reference_policy_validates_against_the_live_shaped_options()     // a fixture policy matching docs/specs/0011-routing-policy-default.md
#[test] fn each_problem_kind_is_reported_with_its_path()                        // table test, one row per rule above; e.g. path "kinds.bugfix.escalate.to_class"
#[test] fn digest_is_stable_across_key_order_and_changes_with_content()
#[test] fn summary_reads_in_plain_words_and_contains_no_identifier_with_a_dash_or_underscore()
#[test] fn classification_schema_limits_kinds_and_classes_and_rejects_extra_fields()
```

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(routing): compiled policy, validation and summary`

### Task 7: The planner

**Files:**
- Create: `crates/aq-routing/src/plan.rs`

**Interfaces:**
- Produces:

```rust
pub struct TaskFacts { pub kind: Option<String>, pub class_hint: Option<Class>, pub origin: Option<String>, pub failed_attempts: u32 }
pub enum Classified { Missing, Failed, Answered(Classification) }
pub enum Plan {
    Planned(Route),
    NeedsClassification { allowed_kinds: Vec<String>, allowed_classes: Vec<Class> },
    NoCandidates { code: String, detail: String },    // codes: class_unassigned, allowlist_excludes_all
}
pub fn plan(policy: &Policy, policy_digest: &str, structure_revision: u64, options: &Options, task: &TaskFacts, classified: &Classified) -> Plan;
```

- `TaskFacts` come from task metadata: `aq:task_type`, `aq:intelligence_hint` (parsed as a `Class`; unparsable is ignored), `aq:origin` (nothing writes it yet), and the charged failed-attempt count.
- The algorithm, in order. Each numbered rule that changes the class or the allowlists appends the named reason.
  1. `Classified::Missing` returns `NeedsClassification` with every kind in the policy and every class in `options`.
  2. Kind: `task.kind` if it is in `policy.kinds`, else the classification's `task_type` if it is, else `default_kind`.
  3. Risk: the classification's, or `Medium` when `Failed` (`classification_failed`, `risk_assumed`).
  4. Starting class. If the kind has `escalate`: `to_class` when `failed_attempts >= after_failures` (`escalated_after_failure`), otherwise the kind's `class`; skip rules 5 and 8. Otherwise `class_hint` if present, else the classification's `intelligence_class`, else the kind's `class`.
  5. Clamp to the kind's `max_class` (`hint_clamped`).
  6. A deep class without the `deep_reasoning` flag becomes `complex-high` (`deep_requires_reasoning`).
  7. Lane by kind or origin: the kind's `lane`, else the origin's. Its `class`, when set, replaces the class unless rule 4 escalated; its `harnesses`, `providers`, `prefer` and `hold` apply (`lane_matched`).
  8. Risk floor: with a rule for the risk level, the floor is `relax.class` when every `relax.requires` flag is set (`risk_relaxed`), else `min_class`. A class below the floor is raised to it (`class_raised_for_risk`). The floor applies after the clamp in rule 5.
  9. Risk harnesses: with a rule, the harness allowlist is the rule's `harnesses`, intersected with the lane's when rule 7 set one.
  10. Cheap lanes, only when no lane matched in rule 7, the kind is `narrow` and the origin does not set `narrow: false`: the first lane in policy order with `cheap`, whose `requires` flags are all set, whose `max_risk` is at least the risk, whose `classes` has the current class as a key, and which has at least one `Offer` in `options` for its mapped class among its providers and harnesses. The class becomes the mapped class and the lane's providers and harnesses replace the allowlists (`lane_matched`). A `Failed` classification never matches a cheap lane.
  11. Preference: `prefer` is the lane's when set, else the kind's `prefer_harnesses`.
  12. Reserved: for each entry whose `classes` contains the class and whose `only_lanes` does not contain the matched lane, remove its `harness` from the allowlist; an empty allowlist first becomes every harness offered for the class (`reserved_harness_removed`).
  13. `NoCandidates(class_unassigned)` when the class is not in `options`; `NoCandidates(allowlist_excludes_all)` when no `Offer` for the class passes the allowlists. Otherwise `Planned`, with `provider_intent: Pinned` when the lane has `hold`.

- [ ] **Step 1: Write the failing tests**, all against the reference policy fixture from Task 6:

```rust
#[test] fn missing_classification_asks_for_one()
#[test] fn ordinary_feature_medium_risk_is_standard_high_on_claude_or_codex_preferring_codex()
#[test] fn docs_low_risk_narrow_tested_goes_to_the_local_lane()
#[test] fn a_provider_the_policy_never_names_is_excluded_above_low_risk()                 // e.g. a free hosted model offered under the same class
#[test] fn migration_very_high_risk_is_raised_to_complex_low_and_relaxed_to_standard_high_when_narrow_and_tested()
#[test] fn design_goes_to_complex_high_preferring_claude()
#[test] fn art_design_is_pinned_to_codex()
#[test] fn deep_hint_without_deep_reasoning_becomes_complex_high_and_with_it_stays_deep()
#[test] fn bugfix_escalates_to_complex_high_after_one_charged_failure_and_ignores_a_lower_hint()
#[test] fn hint_above_max_class_is_clamped()
#[test] fn failed_classification_routes_at_standard_high_on_claude_or_codex_and_never_a_cheap_lane()   // Review Focus 4
#[test] fn deep_claude_is_removed_for_an_escalated_bugfix_but_kept_for_design()
#[test] fn gemini_only_candidates_are_excluded_above_low_risk()
#[test] fn class_without_a_profile_is_no_candidates_class_unassigned()
#[test] fn same_inputs_give_the_same_route()
```

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(routing): deterministic planner`

### Task 8: Compile the policy when it is saved

**Files:**
- Create: `crates/aq-server/src/routing/compile.rs`
- Modify: `crates/aq-server/src/profile_catalog/settings.rs:39` (after a successful structure save whose `routing_markdown` changed, call `compile::requested`), `crates/aq-server/src/routing/http.rs`, `crates/aq-server/src/internal_work.rs` (a completion hook), `protocol/commands/v1/routes.json`
- aq-dashboard: `src/api/routing.ts` (create), `src/features/settings/RoutingPolicyEditor.tsx` (+ test, CSS), `tests/intelligence-structure.spec.ts`
- Test: `crates/aq-server/tests/it_profiled/profile_catalog.rs`

**Interfaces:**
- Consumes: `InternalWork::submit`, `Outcome`, `Purpose::PlaybookCompilation`, `AnswerShape::Json`; `aq_routing::policy::{Policy, validate, digest, summary}`; `routing::class_profile`; `structure::document`.
- Produces:

```rust
pub async fn options(state: &AppState) -> Result<Options, AppError>;      // from structure assignments, the active catalog and qualified harnesses
pub async fn requested(state: &AppState, structure_revision: u64) -> Result<Uuid /* internal task */, AppError>;
pub async fn completed(state: &AppState, internal_task: Uuid, outcome: &Outcome) -> Result<(), AppError>;
pub async fn active(pool: &PgPool) -> Result<Option<(Policy, String /*digest*/, u64 /*structure_revision*/)>, AppError>;
```

- `requested` submits `Request { purpose: PlaybookCompilation, blocking: false, escalate: true, deadline_ms: 600_000, max_output_bytes: 262_144, affected: Affected { kind: "routing_policy", id: "<structure_revision>" }, answer: Json { schema: <schemars-free hand-written JSON Schema of Policy> }, instructions: COMPILER_INSTRUCTIONS, input: <see below> }`.
- The compiler input is one text: the source Markdown; then "What Settings offers" listing each assigned level in words with its providers and harnesses; then the four parent descriptions from the structure document. `COMPILER_INSTRUCTIONS` is a constant in `compile.rs` that explains each field of `Policy` in two or three sentences, says to use only levels, harnesses and providers from the offered list, to write `classifier_instructions` as guidance for a model classifying one task (kinds, how to judge risk, the flags), and to copy the owner's rules without adding any.
- `completed` is called by the internal work loop when a `playbook_compilation` task finishes. On `Answered`: parse as `Policy`, run `validate`; with no problems, insert `routing_policies` as `active`, mark the previous `superseded`, set `routing_state.active_policy_digest`. With problems, or on `Failed`, insert a `failed` row whose `error` lists each problem as `path: message`; the active policy is untouched. A result whose `structure_revision` is older than the newest saved revision is stored `superseded` and never activated.
- Routes (`RequireOwner`, class `owner_admin`): `GET /api/settings/routing` returning `{enabled, active:{digest, structure_revision, summary[], created_at_ms}|null, latest:{status, structure_revision, error, internal_task_id, created_at_ms}|null}`; `POST /api/settings/routing` with `{enabled: bool}`.
- Dashboard: below the textarea, the editor shows "Compiling…" while `latest` is newer than `active` and has no status yet; the `active.summary` sentences under "How this policy routes"; on a failed latest compile, "This version did not compile. The previous policy is still routing." with the problems listed, and `<InternalRun affectedKind="routing_policy" affectedId={revision} />`; a switch "Route tasks with this policy", disabled until a policy is active. Replace the caption "Saved policy source; playbook execution will be connected later." with "Saved changes are compiled and take effect when they compile."

- [ ] **Step 1: Write the failing tests** (the internal work API backend is the local fake endpoint, returning a fixture policy):

```rust
#[tokio::test] async fn saving_changed_markdown_submits_one_compilation_and_an_unchanged_save_submits_none()
#[tokio::test] async fn a_valid_compiled_policy_becomes_active_immediately_with_its_summary()
#[tokio::test] async fn an_invalid_compiled_policy_is_stored_failed_with_paths_and_the_previous_stays_active()   // Review Focus 3
#[tokio::test] async fn a_slow_compile_of_an_older_revision_never_replaces_a_newer_active_policy()
#[tokio::test] async fn options_list_only_assigned_classes_and_qualified_harnesses()
```

Dashboard unit tests for each editor state and the switch; one Playwright case: save, see the summary, turn routing on.

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** in each repository: `feat(routing): compile the policy on save and activate it`

---

# Part R3: the host surface

### Task 9: Measure tick time over many routing runs (spike, decides scope)

**Files:**
- Create: `crates/aq-server/tests/it_playbook/routing_scale.rs` (one `#[ignore]` test), `docs/implementation/0011-R3-scale.md`

- [ ] **Step 1: Write the measurement.** `#[tokio::test] #[ignore] async fn five_hundred_routing_runs_in_one_protected_namespace()`: reuse `Pilot::new()` from `real_adapter_pilots.rs:527` (Host8, protected storage, `FakeModel`), start `pilots::routing()` 500 times sequentially with distinct subjects, drive each to completion, and print the tick duration at runs 1, 50, 100, 250 and 500.
- [ ] **Step 2: Run it in a release build**: `timeout 3600 cargo nextest run --release -p aq-server --test it_playbook --run-ignored only -E 'test(/routing_scale/)' --no-capture`
- [ ] **Step 3: Record** the five numbers and the machine in `docs/implementation/0011-R3-scale.md`.
- [ ] **Step 4: Decide.** If the tick at run 500 is under 1 s, continue with Task 10. If not, stop and report to the owner: bounded storage for protected namespaces in aq-playbooks (writer format 3 currently requires `protection_key_id IS NULL`, `aq-playbooks-postgres/migrations/0005_transition_records.sql:12-14`) and re-vendoring the engine must be planned and done before Task 14. Do not work around it by rotating namespaces.
- [ ] **Step 5: Commit** `test(routing): tick time over 500 routing runs`

### Task 10: Host catalog 2.2.0 (aq-playbooks)

**Files (aq-playbooks, branch `feat/internal-work`):** mirror commit `913461d` exactly.
- Create: `contracts/aq-host-2.2.0/{README.md,fixtures.json,manifest.json,registry.json}`, `crates/aq-playbooks-cli/tests/catalog_v2_2_contract.rs`
- Modify: `crates/aq-playbooks-cli/src/{catalog.rs,fake_host.rs,pilots.rs}`, `docs/aq-host-integration.md`

**Interfaces:**
- Produces, as the only differences from 2.1.0:
  - `context.read` input `view` values add `routing_plan`. For this view the input also accepts `judgment` (an operation id string, optional) and the subject is a work item. Its output `snapshot` is `{"plan": "planned"|"needs_classification"|"no_candidates"|"already_routed", "reference": string}`.
  - `work.change` input `variant` values add `route`, with field `plan` (the `reference` string from a `routing_plan` read). Its outcomes add `routed` and `stale`.
  - `judgment.request` input adds optional `answer_shape` with values `choice` (default) and `routing_classification`. With `routing_classification`, `choices` may be empty.
  - Event `aq.work.route_needed`: `trusted_producers ["aq.host.bridge@1"]`, scope `project`, `subject_field "subject.id"`, payload `{"subject":{"kind":"work","id":string,"revision":integer}}`.
  - `manifest.json`: `version "2.2.0"`, `supersedes` 2.1.0 with its fingerprint `sha256:24407a41…1716` (copy the full value from the 2.1.0 manifest).
- The CLI's routing pilot and fake host move to the 2.2.0 catalog: `routing_plan`, optional classification judgment, second `routing_plan` with `judgment`, `work.change route`.

- [ ] **Step 1: Write the failing contract test** modelled on `catalog_v2_1_contract.rs`: the fingerprint matches the manifest; each of the four additions is present; a 2.1.0 artifact still validates against the 2.2.0 registry; the routing pilot runs against the fake host through `planned` and through `needs_classification`.
- [ ] **Step 2: Run to verify it fails**: `timeout 900 cargo test -p aq-playbooks-cli --test catalog_v2_2_contract`
- [ ] **Step 3: Implement.** **Step 4: Run to verify it passes**, plus `timeout 900 cargo test --workspace --locked`.
- [ ] **Step 5: Commit** `feat(catalog): publish AQ host catalog 2.2.0 with routing` and push the branch so aq-server can vendor a commit.

### Task 11: Adopt 2.2.0 as `CatalogProfile::Host9` (aq-server)

**Files:** mirror commit `ad729968`.
- Create: `vendor/aq-host-catalog-2.2.0/` (the four files), `vendor/aq-host-catalog-2.2.0-source.json`, `crates/aq-server/migrations/0121_playbook_host_catalog_2_2.sql` (`CHECK(catalog_profile IN (1,…,9))`, state format bump)
- Modify: `crates/aq-server/src/playbooks/{review_domain.rs:33-49,134-136, config.rs:92-106, driver.rs:265-270, runtime.rs:103, service.rs, coding.rs:314-322}`
- Test: `crates/aq-server/tests/it_playbook/playbook_host_integration.rs`

**Interfaces:**
- Produces: `CatalogProfile::Host9`; `HOST9_VERSION = "2.2.0"`, `HOST9_FINGERPRINT`; `host9_registry()` via `include_str!`; config profile `host_v9`; `is_host2()` is `matches!(self, Host7 | Host8 | Host9)`; `Service::new_host9_integration(...)` with the same parameters as `new_host8_integration`. The coding pipeline `Builder` keeps `Host8` unless constructed for `Host9`; add `Builder::for_profile(CatalogProfile)`.

- [ ] **Step 1: Write the failing tests**: `host9_registry_fingerprint_matches_the_vendored_manifest`, `host_v9_profile_starts_and_registers_the_twelve_commands`, `host8_artifacts_install_under_host9`.
- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass**, plus the whole `it_playbook` binary.
- [ ] **Step 5: Commit** `feat(playbooks): adopt AQ host catalog 2.2.0 as CatalogProfile::Host9`

### Task 12: `routing_plan`, the `route` variant and the classification judgment

**Files:**
- Create: `crates/aq-server/src/playbooks/host_adapters/routing.rs`
- Modify: `crates/aq-server/src/playbooks/host_adapters.rs:1054` (view match), `:835` (variant match), `host_adapters/requests.rs:873-1060` (`answer_shape`), `crates/aq-server/src/playbooks/internal_judgments.rs`
- Test: `crates/aq-server/tests/it_playbook/playbook_host_routing.rs` (add to `it_playbook/main.rs`)

**Interfaces:**
- Consumes: `aq_routing::{plan, Plan, TaskFacts, Classified}`, `routing::compile::{active, options}`, `routing::write`, `WorkGraph::node`, `replacement::load` (`profiled_execution/replacement.rs:40`) and `Prior::failed()`/`exempt` for the failed-attempt count, `InternalWork`, `aq_internal_work::store::list` by affected item.
- Produces:
  - Table added in this task's migration edit to 0120: `routing_plans(reference TEXT PK, task_id UUID, task_version BIGINT, plan JSONB, created_at_ms BIGINT)`. A `reference` is `plan:<uuid>`.
  - View `routing_plan` (Host9 only; other profiles return `incomplete`): load the task node; if it already has `aq:route`, return `already_routed`. With no active policy, return `incomplete`. Build `TaskFacts`. `Classified` is `Missing` when the input has no `judgment`; otherwise look up the internal task whose affected item is that playbook operation: `Answered` parses to `Answered(Classification)`, a failed run or an unparsable answer is `Failed`. Run `plan`, store it under a new reference with the node version, and return the snapshot.
  - Variant `route`: load the stored plan by reference; `refused` if missing or not `planned`; if the node version differs from the stored `task_version`, return `stale`; else `routing::write`, returning `routed` or `stale`.
  - `judgment_request`: with `answer_shape: routing_classification`, the request to the provider carries the classification mode. `InternalJudgments` gains `fn classification(work, …)`: purpose `RoutingClassification`, `AnswerShape::Json { schema: classify::schema(allowed_kinds, allowed_classes) }`, instructions `policy.classifier_instructions`, input the task's title, description, kind and class hint and nothing else, `escalate: false`, `blocking: true`. The operation completes `assessed` with `choice: "answered"` and the rationale set to the classification's `reason`; the playbook never reads the object.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn routing_plan_without_a_judgment_needs_classification()
#[tokio::test] async fn routing_plan_with_an_answered_judgment_is_planned_and_route_writes_it()
#[tokio::test] async fn routing_plan_with_a_failed_judgment_still_plans_with_classification_failed()
#[tokio::test] async fn route_after_the_task_changed_is_stale_and_nothing_is_written()          // Review Focus 1
#[tokio::test] async fn already_routed_task_and_owner_routed_task_are_already_routed()
#[tokio::test] async fn classifier_is_given_only_title_description_kind_and_hint()              // assert the internal task's stored input
#[tokio::test] async fn routing_views_are_incomplete_on_host8()
```

- [ ] **Step 2: Run to verify they fail**: `timeout 900 cargo nextest run -p aq-server --test it_playbook -E 'test(/^playbook_host_routing::/)'`
- [ ] **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(playbooks): routing plan view, route variant and classification judgment`

### Task 13: The server originates `aq.work.route_needed`

**Files:**
- Modify: `crates/aq-server/src/playbooks/host_adapters/routing.rs`, `crates/aq-server/src/playbooks/driver.rs:613-675` (call the collector in `quantum` for Host9), `crates/aq-server/src/playbooks/service.rs`
- Test: `it_playbook/playbook_host_routing.rs`

**Interfaces:**
- Consumes: `queue_occurrence(&self, tx, id: Uuid, event: &EventEnvelope)` (`triggers.rs:200-219`) as the pattern for an event with no executing intent; `seal_event` validation against `self.profile.registry()`; the graph ready read used by placement (`scheduling_ready_current`); `routing::enabled`; table `route_needed_emissions`.
- Produces: `pub(crate) async fn collect_route_needed(&self, now_ms: i64) -> Result<u64>`. For each enabled project whose binding is on an automatic router, when routing is on: read up to 32 ready tasks; for each with no `aq:route`, emit `aq.work.route_needed` unless `route_needed_emissions` has a row for that task with the same `task_version` newer than 120 000 ms; upsert the emission row. The event id is derived from `(task_id, task_version, emitted_at_ms / 120_000)` so a retry in the same window is the same event.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test] async fn an_unrouted_ready_task_emits_one_event_and_not_again_within_two_minutes()
#[tokio::test] async fn it_is_emitted_again_after_two_minutes_and_at_once_when_the_task_version_changes()
#[tokio::test] async fn nothing_is_emitted_when_routing_is_off_or_the_binding_is_pinned_or_the_task_is_routed()
#[tokio::test] async fn the_event_starts_a_run_of_an_installed_artifact_that_triggers_on_it()     // first proof of a trigger on a host profile
```

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** `feat(playbooks): server-originated route_needed event`

---

# Part R4: the playbook, the failure path, acceptance

### Task 14: The routing playbook, installed by default

**Files:**
- Create: `crates/aq-server/src/playbooks/routing.rs`
- Modify: `crates/aq-server/src/playbooks/coding/pilots.rs` (delete `routing()` and its test use; the pilot is superseded), `playbooks/artifacts.rs:449` area, `playbooks/api.rs:252` area, `playbooks/runtime.rs` (install at start), `protocol/commands/v1/routes.json`
- Test: `it_playbook/playbook_host_routing.rs`, `it_playbook/real_adapter_pilots.rs` (remove the routing pilot cases)

**Interfaces:**
- Consumes: `Builder::for_profile(CatalogProfile::Host9)`, `invoke`, `wait`, `seal` (`coding.rs:355-420`); `Trigger { id, event, entry, input }`; `install_coding_default` as the template.
- Produces:

```rust
pub const ROOT: &str = "aq.routing.default";
pub fn artifact() -> Result<Artifact, CodingError>;
pub async fn install_default<C: Catalog, L: LockingLedger>(catalog: &C, ledger: &L, op_id: &str) -> Result<Value>;
```

- Steps of the artifact (input `{work: {id, revision}}`, taken from the trigger's event subject):
  1. `plan_a`: `context.read` view `routing_plan`. `planned` goes to 4 with `plan_a.reference`; `needs_classification` goes to 2; `already_routed` returns outcome `skipped`; `no_candidates` returns outcome `unroutable`.
  2. `classify`: `judgment.request` with `answer_shape: routing_classification`, deadline 120 000 ms. Then `wait` on `aq.judgment.completed` for that operation, 150 000 ms. Any outcome, including a timeout, goes to 3.
  3. `plan_b`: `context.read` view `routing_plan` with `judgment` set to the classify operation. `planned` goes to 4; `already_routed` returns `skipped`; `no_candidates` returns `unroutable`.
  4. `apply`: `work.change` variant `route` with the plan reference. `routed` returns outcome `routed`; `stale` returns `skipped` (the next `route_needed` event routes the new version).
- One trigger: `Trigger { id: "route-needed", event: aq.work.route_needed, entry: "main", input: <the event's subject> }`.
- `POST /api/playbooks/routing/default` (`RequireOwner`) calls `install_default`. At runtime start, when the profile is Host9, the server calls it too; installing the same digest twice is a no-op.

- [ ] **Step 1: Write the failing end-to-end tests** (Host9, fake graph, internal work on the local fake endpoint):

```rust
#[tokio::test] async fn an_unrouted_task_is_routed_end_to_end_by_the_default_playbook()          // route_needed -> run -> aq:route on the node
#[tokio::test] async fn a_docs_task_and_a_migration_task_get_different_classes()
#[tokio::test] async fn classifier_outage_still_routes_with_classification_failed()               // Review Focus 4
#[tokio::test] async fn a_task_edited_mid_run_is_skipped_and_routed_by_the_next_event()
#[tokio::test] async fn restart_between_the_judgment_and_the_second_plan_completes_the_run_once()
#[tokio::test] async fn install_default_twice_is_a_no_op()
```

- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass**, plus the whole `it_playbook` binary.
- [ ] **Step 5: Commit** `feat(routing): the default routing playbook`

### Task 15: Routing failures follow the internal work failure path

**Files:**
- Modify: `crates/aq-internal-work/src/store.rs` (add `record_failure`), `crates/aq-server/src/playbooks/host_adapters/routing.rs`, `crates/aq-server/src/routing.rs`
- aq-dashboard: `src/features/work/inspector/TaskRoute.tsx` (place `<InternalRun affectedKind="task" affectedId={taskId} />`)
- Test: `it_playbook/playbook_host_routing.rs`, `it_dashboard/dashboard_aggregation.rs`

**Interfaces:**
- Consumes: the escalation rules of the internal work plan's Task 13 (`escalate`, supervisor message, `AttentionKind::InternalWork`, resolve on success).
- Produces: `pub async fn record_failure(pool: &PgPool, purpose: Purpose, affected: &Affected, reason: Reason, detail: &str, now_ms: i64) -> Result<Uuid>`, which inserts a `failed` internal task with `escalate = true` and no candidates, unless an unresolved one already exists for the same affected item and `detail`.
- Three conditions call it with purpose `RoutingClassification` and `affected = {kind: "task", id, project_id}`:
  - The planner returns `no_candidates`: reason `NoCandidate`, detail `<code>: <detail>`.
  - A task has had no route for 600 000 ms while routing is on (checked in `collect_route_needed`): reason `DeadlineExceeded`, detail `unrouted for 10 minutes`.
  - A routed task is refused by placement because its route excludes every candidate (Task 4): reason `NoCandidate`, detail `route excludes every candidate`.
- When a route is later written for the task, the unresolved record for it is resolved.
- A single classifier failure calls nothing: the classification internal task has `escalate: false` and the task routes with defaults.

- [ ] **Step 1: Write the failing tests**: `unroutable_task_is_messaged_to_the_project_supervisor_once`, `task_unrouted_for_ten_minutes_is_escalated_once`, `a_later_route_resolves_it_and_removes_the_attention_item`, `one_classifier_failure_raises_nothing`, and in `it_dashboard` `owner_escalated_routing_failure_points_at_the_task`.
- [ ] **Step 2: Run to verify they fail.** **Step 3: Implement.** **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** in each repository: `feat(routing): routing failures use the internal work failure path`

### Task 16: The local stack and live acceptance

**Files:**
- Modify: `scripts/dev-stack.py:182-268` (profile `host_v9`, a new namespace generation, no deployment entry for the inspection-only artifact, `agent_access` and the grant the routing playbook needs), `docs/operations.md`, `docs/status.md` (§3 and §8)
- Create: `docs/implementation/0011-routing-policy.md`

- [ ] **Step 1: Update `dev-stack.py`** and verify with `python3 scripts/dev-stack.py --help` and the existing smoke script `scripts/product-smoke.py` in its fixture mode.
- [ ] **Step 2: Commit** `feat(dev-stack): run the routing playbook on host_v9`
- [ ] **Step 3: Live acceptance on `aq-ms1`. The owner approves the rollout first.** It changes the playbook namespace, applies migrations 0119 to 0121 (forward-only) and restarts the server. Record each result in `docs/implementation/0011-routing-policy.md`:
  1. Save the policy; "How this policy routes" appears; turn routing on.
  2. Create an Active docs task and an Active task that changes a database migration in the scratch project. Each shows "Waiting for routing", then a route. The classes differ, and the migration task's route allows Claude and Codex only.
  3. The docs task is offered on its class profile and completes.
  4. Stop the internal work host, create a third task: it routes within three minutes with the reason "classification failed" and risk assumed medium. Start the host again.
  5. Set an owner route on a fourth task before it is routed; the router leaves it alone.
  6. Assign no profile to a class the policy uses for a kind, create a task of that kind: it stays unrouted with a reason, the project supervisor is messaged, and after the supervisor escalates it appears in "Needs your attention" and opens the task. Restore the assignment; the task routes and the item disappears.
  7. Record the playbook tick time before and after the session.
- [ ] **Step 4: Update `docs/status.md`** §3 to say what is live, with the evidence path.
- [ ] **Step 5: Commit** `docs: routing policy live acceptance`

---

## What this plan leaves out

- A per-project policy (spec §2).
- Anything that writes `aq:origin`. The planner reads it; until something sets it, the rule about repairs never fires.
- Free hosted models. The owner removed them from the policy on 2026-11-12; the compiled policy has no lane for them.
- Playbooks other than routing asking for a stronger or weaker level on a judgment.
- Bounded storage for protected playbook namespaces and re-vendoring the engine, unless Task 9 shows they are needed.
