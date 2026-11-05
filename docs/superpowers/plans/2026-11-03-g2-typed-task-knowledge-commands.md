# G2: Typed Task and Knowledge Commands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add eleven typed dashboard actions (six task, five knowledge) to the
`/api/v1/actions` union. Each returns an honest disposition and carries
`current_versions` on a conflict.

**Architecture:**
- **Graph-backed kinds** go through one generic adapter, `GraphAction<P>`, in
  the new child module `agent_access/human/graph_actions.rs`. That module can
  use the private project `access`, the Graph `runtime` and the `node`
  validation.
  - Each payload type maps itself to the operations of a single
    `MutationRequest`.
  - The request is posted with the existing `settlement_post` transport under
    the idempotency key `aq-dashboard-action:{operation_id}`.
- **`task.cancel`** delegates to B3's
  `profiled_execution::cancellation::cancel`.
- **`task.retry`** delegates to B4's `replacement::{assess_task, release}`.
  Both live in `agent_access/human/task_execution.rs`.
- **The operation contract** gains an optional
  `current_versions: {entity: version}` map. It may be filled only on
  `conflict`.

**Tech Stack:**
- Rust: axum, sqlx/Postgres, schemars, serde_json.
- The aq-graph v1 HTTP mutation API.
- Python `check.py` with jsonschema, for fixtures.

**Spec:**
- Task description: `aq task show steady-cascade-45.3 --field description`.
- Graph contract: `protocol/agent-access/v1/upstream/aq-graph-openapi.json`
  (MutationRequest, CreateNode, UpdateNode, NodePatch, VerifyNode, RestoreNode,
  AddEdge, RemoveEdge, Source).
- B3: `docs/implementation/B3.md`.
- B4: `docs/implementation/B4.md`.

## Global Constraints

- Each kind has `version 1` and a deny-unknown-fields payload, so its schema
  is `type: object` with `additionalProperties: false`.
- Every payload carries `operation_id` (equal to `request.operation_id`) and
  `project_id`.
- `subject` is `{type: "task"|"knowledge", id, revision}`, and
  `expected_versions` is `{"task"|"knowledge": revision}`. The revision is the
  Graph node `version` as a decimal string matching `^[1-9][0-9]{0,18}$`.
- Exception: `knowledge.author`. Its subject is
  `{type: "project", id, revision: expected_project_revision}`, the same as
  `task.create`.
- Authority:
  - The caller must be the current owner (`existing::owner`).
  - The project must be bound to Graph (`access`).
  - A Graph mutation identity must be configured
    (`task_create_available(state)`). If it is not, the request fails with
    503 `unavailable`.
- Validation reads the node fresh from Graph: right workspace, right kind,
  `deleted == false`.
- Disposition mapping:
  - Graph 2xx, with a valid `MutationResult` whose results match the
    operations: `confirmed`.
  - Graph 409, or a stale version caught before dispatch: `conflict`, with
    `current_versions` re-read from Graph.
  - Graph 400/403/404/422: `rejected`.
  - Any other status, or a transport failure: `unknown`.
- `confirmed` on `task.cancel` means the cancellation was admitted. It does
  not mean the writers have stopped.
- `task.retry` never resets the retry, spend or sandbox budgets. It only
  releases a settled, failed predecessor that B4 has assessed.
- Priority:
  - Stored in the metadata key `aq:priority`.
  - An integer in `-1_000_000..=1_000_000`. The scheduler reads it with
    `as_i64()` and sorts descending (`producer/workflow.rs:238-250`).
  - The other metadata keys are preserved, because a Graph metadata patch
    replaces the whole object.
- `knowledge.author` always creates `verification: unverified`, with
  `content_type` defaulting to `note`. It cannot assert `verified`.
- `knowledge.verify` and `knowledge.restore` are administrator operations.
  Graph enforces this, and a refusal maps to `rejected`. `knowledge.verify`
  needs a non-blank `request.reason` and at least one evidence `Source`.
- Out of scope:
  - Task creation.
  - B3/B4 internals.
  - The dashboard UI (G3).
  - Playbook kinds.
  - `set_summary`, deletion and bulk edits.
- Testing: no slow tests, no real Graph. Use a fake axum Graph on
  `127.0.0.1:0`.

## Review Focus

1. **A stale subject version at execute time.**
   - Expected: `conflict`, with `current_versions` naming the live Graph
     version, and no Graph mutation posted.
   - Pinned by `knowledge_correct_with_stale_version_conflicts` (Task 3).
2. **Reprioritize on a task that already has other metadata.**
   - Expected: those keys survive, and only `aq:priority` changes.
   - Pinned by `task_reprioritize_preserves_other_metadata` (Task 3).
3. **Retry while the predecessor is still running or unreconciled.**
   - Expected: `rejected`, and the claim is not released.
   - Pinned by `task_retry_creates_replacement_attempt` (its unsettled leg,
     Task 4).
4. **Replay of the same operation_id with a changed payload.**
   - Expected: 409 from the journal.
   - Already covered by `dashboard_api::actions` admission, so it needs no
     new test.
5. **A Graph 403 on an admin-only verify.**
   - Expected: `rejected`, not `unknown`.
   - Pinned by `knowledge_verify_forbidden_is_rejected` (Task 3).

---

### Task 1: `current_versions` on the operation contract

**Files:**
- Modify: `crates/aq-server/src/dashboard_api/actions/contract.rs`
  (`Operation`, `Outcome`)
- Modify: `crates/aq-server/src/dashboard_api/actions/mod.rs` (`finish`)
- Modify: `crates/aq-server/src/dashboard_api/actions/journal.rs` (`admit`)
- Modify: `crates/aq-server/src/dashboard_api/actions/existing.rs`
  (`disposition`)
- Modify: `crates/aq-server/tests/dashboard_actions.rs` (`Operation`
  literal)
- Test: `crates/aq-server/src/dashboard_api/actions/tests.rs`

**Interfaces:**
- Produces:
  - `Operation.current_versions: BTreeMap<String, String>`, with
    `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]` and a
    schemars maximum of 32 properties.
  - `Outcome.current_versions: BTreeMap<String, String>`.
  - `Outcome::conflict(current: BTreeMap<String, String>) -> Outcome`.

- [ ] **Step 1:** Write the failing unit test
  `conflict_outcome_carries_current_versions`.
  - Build an outcome with `Outcome::conflict({"knowledge": "3"})`.
  - Assert that `finish` keeps the map.
  - Assert that a confirmed outcome carrying a non-empty map is finished as
    `unknown`.
- [ ] **Step 2:** Run
  `aq test -- -p aq-server --lib conflict_outcome_carries_current_versions`.
  Expected: it fails to compile.
- [ ] **Step 3:** Add the field and the constructor.
  - `finish` accepts a non-empty map only when the disposition is
    `Conflict`, and includes the map in its unchanged-equality check.
  - `admit` starts with an empty map.
  - Stored rows that lack the field still deserialize, via the serde default.
- [ ] **Step 4:** Rerun the test. Expected: PASS.

### Task 2: Typed payloads, registration, schemas and capabilities

**Files:**
- Create: `crates/aq-server/src/agent_access/human/graph_actions.rs`
- Create: `crates/aq-server/src/agent_access/human/task_execution.rs`
- Modify: `crates/aq-server/src/agent_access/human.rs`
  (`mod graph_actions; mod task_execution; pub(crate) use …::{register, schemas}`)
- Modify: `crates/aq-server/src/dashboard_api/actions/existing.rs`
  (`registry`, `schemas`)
- Modify: `crates/aq-server/src/dashboard_api/service.rs` (`bootstrap`
  capabilities)
- Modify: `crates/aq-server/src/dashboard_assets/manifest.rs`
  (`CAPABILITIES`)
- Modify: `protocol/dashboard-actions/v1/schema.json` (regenerated)

**Interfaces:**
- Produces:
  - `pub(crate) fn register(r: &mut Registry)`.
  - `pub(crate) fn schemas(r: &mut Registry)`.
  - Payload structs: `TaskEdit`, `TaskReprioritize`, `DependencyAdd`,
    `DependencyRemove`, `KnowledgeAuthor`, `KnowledgeCorrect`,
    `KnowledgeVerify`, `KnowledgeRetire`, `KnowledgeRestore`, `TaskCancel`,
    `TaskRetry`.
  - Capabilities: `tasks.edit`, `tasks.cancel`, `tasks.retry`,
    `tasks.reprioritize`, `tasks.dependencies.add`,
    `tasks.dependencies.remove`, `knowledge.author`, `knowledge.correct`,
    `knowledge.verify`, `knowledge.retire`, `knowledge.restore`.
    - Each is `graph && task_create_available(state)`.
    - `tasks.cancel` and `tasks.retry` also require native execution.

Payload fields. Every payload also has `operation_id` and `project_id`, and
every one except `knowledge.author` has `expected_version`.

| Kind | Fields |
|---|---|
| `task.edit` | `title?` (1..512), `body?` (≤32768 B), `acceptance_criteria?` (≤8192 B), `labels?` (≤16 unique, each 1..128 B); at least one field |
| `task.cancel` | none; `request.reason` is required |
| `task.retry` | none |
| `task.reprioritize` | `priority: i64` in ±1_000_000 |
| `task.dependency.add` | `blocker_id: Uuid`; becomes `add_edge {source: blocker, target: task, edge_type: "blocks"}` with no revisions |
| `task.dependency.remove` | `edge_id: Uuid`, `edge_version` (decimal string); becomes `remove_edge` |
| `knowledge.author` | `expected_project_revision`, `title`, `body`, `labels?`, `content_type?` (default `note`); becomes `create_node {id: operation_id, kind: knowledge, verification: unverified}` |
| `knowledge.correct` | `title?`, `body?`, `labels?`, `content_type?`; at least one field |
| `knowledge.verify` | `verification: verified \| disputed`, `evidence: [Source]` (1..64); `request.reason` is required |
| `knowledge.retire` | `retirement_reason` (non-blank, ≤4096), `successor_id?`; Graph enforces the `supersedes` edge rule |
| `knowledge.restore` | `revision` (≥1), `include_lifecycle?` |

- [ ] **Step 1:** Write the failing test
  `dashboard_action_schema_lists_task_and_knowledge_kinds` in `tests.rs`.
  - Assert that `registry` contains all eleven kinds.
  - Assert that each payload schema has `additionalProperties: false`.
- [ ] **Step 2:** Run it. Expected: FAIL.
- [ ] **Step 3:** Implement the payloads and registration. Leave `execute`
  for Task 3, but keep it compilable.
- [ ] **Step 4:** Regenerate the schema with
  `cargo run --locked -p aq-server --example dashboard_actions_contract`,
  then rerun the test. Expected: PASS.

### Task 3: The Graph adapter, with the named conflict test

**Files:**
- Modify: `crates/aq-server/src/agent_access/human/graph_actions.rs`
- Test: `crates/aq-server/tests/dashboard_task_knowledge.rs`, with a fake
  Graph serving `GET /v1/workspaces/{w}/nodes/{id}` and
  `POST /v1/workspaces/{w}/mutations`, and recording the posted bodies.

**Interfaces:**
- Consumes: `Outcome::conflict` from Task 1.
- Produces: the `GraphPayload` trait.
  - `const KIND`, `const ENTITY`.
  - `fn operations(&self, current: &Value) -> Result<Vec<Value>, ApiError>`.
  - `fn reason_required() -> bool`.

Algorithm for `execute`:
1. Run `access`.
2. Read the node, or the project revision for `knowledge.author`.
3. If the expected version is not the current version, return
   `Outcome::conflict({ENTITY: current})` without posting.
4. Build the `MutationRequest` bytes and check them with
   `upstream_valid("MutationRequest")`.
5. Call `settlement_post`.
6. Map the status as described in Global Constraints. On 409, re-read the
   node to fill `current_versions`.
7. On `confirmed`:
   - `receipt_refs = [graph_change_set(change_set_id)]`.
   - `affected_entities` is one entry per result:
     `{task|knowledge|graph_edge, id, version}`.

`authorize_lookup` checks owner, project access and the subject type.
`reconcile` returns `None`, because Graph commands have no durable
reconcile journal (follow-up).

- [ ] **Step 1:** Write `knowledge_correct_with_stale_version_conflicts`.
  - The fake Graph has the knowledge node at version 3.
  - Submit with expected version 2.
  - Assert:
    - HTTP 200;
    - `disposition == "conflict"`;
    - `current_versions == {"knowledge": "3"}`;
    - `recovery == "refresh_and_review"`;
    - no POST reached the fake Graph.
- [ ] **Step 2:** Write `knowledge_correct_confirms_with_change_set`.
  - The node is at version 3; expected version is 3.
  - Assert:
    - `confirmed`;
    - the posted operation is
      `{"op":"update_node","id":…,"expected_version":3,"patch":{"title":"New"}}`;
    - the receipt is `graph_change_set`.
- [ ] **Step 3:** Write `task_reprioritize_preserves_other_metadata`.
  - The task's metadata is `{"example:weight":1.5}`.
  - Assert that the posted patch metadata is
    `{"example:weight":1.5,"aq:priority":7}`.
- [ ] **Step 4:** Write `knowledge_verify_forbidden_is_rejected`.
  - The fake Graph answers 403.
  - Assert `rejected`.
- [ ] **Step 5:** Write `knowledge_author_creates_unverified_note`.
  - Assert that the posted operation has `kind: "knowledge"`,
    `content_type: "note"` and `verification: "unverified"`.
- [ ] **Step 6:** Run
  `aq test -- -p aq-server --test dashboard_task_knowledge`.
  Expected: these tests FAIL.
- [ ] **Step 7:** Implement the adapter and every `operations` mapping.
- [ ] **Step 8:** Rerun. Expected: PASS.

### Task 4: Cancel and retry, with the named retry test

**Files:**
- Modify: `crates/aq-server/src/agent_access/human/task_execution.rs`
- Test: `crates/aq-server/tests/dashboard_task_knowledge.rs`

**Interfaces:**
- Consumes:
  - `cancellation::cancel(state, attempt, operation, reason) -> Receipt`.
  - `replacement::assess_task(state, task, now_ms) -> Assessment`.
  - `replacement::release(state, task, &Replacement)`.
- Produces: the `TaskCancel` and `TaskRetry` `ActionKind` implementations.

Behaviour of `task.cancel`:
- Find the holding attempt with
  `SELECT attempt_id FROM execution_attempts WHERE task_id=$1 AND kind='profiled_v1' AND holds_task`.
- No holding attempt gives `rejected`.
- Otherwise call `cancel(state, attempt, operation_id, reason)`. That gives
  `confirmed`, with `receipt_refs = [native_cancellation(operation_id)]` and
  `affected = [execution_attempt(attempt)]`.
- B3 cancellation is idempotent per `operation_id`.

Behaviour of `task.retry`:
- The Graph `task_state` must be `active`.
- `Replace(r)` → `release` → `confirmed`, with
  `receipt_refs = [replacement_release(predecessor)]` and
  `affected = [execution_attempt(predecessor)]`.
- `Fresh` or `Blocked(_)` gives `rejected`.

- [ ] **Step 1:** Write `task_retry_creates_replacement_attempt`.
  - Set up `common::profiled` with a fake Graph whose task matches the
    lineage task.
  - Before settling the predecessor (it is still running):
    - submit `task.retry`;
    - assert `rejected`, and that `holds_task` is still true.
  - Settle it the way `tests/replacement_source.rs` does: finished, stopped,
    retained, quiescent.
  - Submit again with a new operation_id. Assert:
    - `confirmed`;
    - a `profiled_replacement_releases` row exists for the predecessor;
    - `holds_task == false`;
    - `assess_task` now returns `Replace` with `needs_release == false`, the
      same predecessor, and an unchanged `attempts_started`. The budget is not
      reset.
- [ ] **Step 2:** Run it. Expected: FAIL.
- [ ] **Step 3:** Implement both kinds.
- [ ] **Step 4:** Rerun. Expected: PASS.

### Task 5: Fixtures, DoD and evidence

**Files:**
- Create: `protocol/dashboard-actions/v1/fixtures/request-<kind>.json`, one
  per kind, and `operation-conflict-current-versions.json`.
- Modify: `protocol/dashboard-actions/v1/check.py`, which validates the new
  request fixtures against the per-kind schema.
- Modify: `protocol/dashboard-actions/v1/README.md`.
- Create: `docs/implementation/G2.md`, covering:
  - tests and commands;
  - the `aq:priority` agreement for F2;
  - the limitations: no Graph reconcile journal, and the pre-dispatch window
    on dependency edits.

- [ ] **Step 1:** Write the fixtures. Run
  `python3 protocol/dashboard-actions/v1/check.py`. Expected: it passes.
- [ ] **Step 2:** Run the DoD checks:
  - `cargo fmt -p aq-server --check`;
  - `cargo clippy --locked --all-targets -- -D warnings`;
  - `cargo run --locked -p aq-server --example dashboard_actions_contract -- --check`;
  - the targeted tests above.
- [ ] **Step 3:** Write `G2.md`. Squash the work into one commit, merge
  `origin/main`, then `aq git push`.
