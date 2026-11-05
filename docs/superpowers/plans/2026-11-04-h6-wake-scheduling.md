# H6 Hibernation Wake Scheduling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a hibernated H4 wait resolves, schedule exactly one continuation attempt (fresh attempt, session and generation) on any eligible live node. That includes a different node when the original node is offline.

**Architecture:** Migration 0083 adds three tables:

- `agent_wait_suspensions`: the server side of suspension, holding the directive, checkpoint and parking report.
- `agent_wait_wakes`: the scheduler projection, one row per H4 resume intent.
- `agent_wait_wake_admissions`: one durable winner per dispatch generation.

The new module `aq_server::agent_access::wakes` projects ready intents, orders them by B4 fairness, serializes each one against cancellation, completion and supersession, picks a live node, and builds and validates a `ContinuationAttachment`.

**Tech Stack:** Rust, sqlx/Postgres, `aq_wait_engine::wire` (lifecycle protocol 0.1.0).

**Spec:** task `clear-crest-61.6` description (H6). H4 evidence: `docs/implementation/H4.md`.

## Global Constraints

- Migration `0083_agent_wait_wakes.sql` ends `UPDATE meta SET value='83' WHERE key='state_format_version';`. Re-check the number against origin/main before push.
- `resume_pending` is never a running state. A wake is running only once an admission winner exists.
- Admission never requires the predecessor's node or sandbox. No warm-start placement.
- Budgets carry forward and are never reset:
  - retries: `replacement::MAX_ATTEMPTS` minus the lineage's started attempts and failed wake launches;
  - elapsed: `MAX_LINEAGE_ELAPSED_MS` minus time since the wait's lineage started;
  - cost: `NotConfigured`.
- Central (F1a) predecessors get a Central successor with `node_id IS NULL`. A worker node is never chosen for one.
- Bounded recovery: `MAX_LAUNCH_FAILURES = 3`, then `escalated`.
- Don't edit `docs/architecture/*` or `docs/specs/*`.

## Review Focus

- Retrying `schedule` after a winner exists returns that winner and inserts no second admission. Pinned by the acceptance test, which calls `schedule` twice.
- A cancelled task never resumes: the wake becomes `cancelled` and the H4 intent is `retired`. Pinned by `wake_cancelled_task_does_not_resume`.
- A missing Checkpoint or Source receipt holds the wake as `resume_pending` with reason `retention_missing`. It is not admitted with an invalid attachment, and repeated holds escalate. Covered by `validate()` before insert. The test for this is a follow-up.
- If every node is offline, the wake stays `resume_pending` with reason `no_eligible_node`. It never loops into a running state.
- A launch failure after admission bumps `dispatch_generation`. After 3 failures the wake is `escalated`.

---

### Task 1: Migration + wake projection + admission (acceptance)

**Files:**
- Create: `crates/aq-server/migrations/0083_agent_wait_wakes.sql`
- Create: `crates/aq-server/src/agent_access/wakes.rs`
- Modify: `crates/aq-server/src/agent_access.rs` (add `pub mod wakes;`)
- Create: `crates/aq-server/tests/profiled_results/wakes.rs`
- Modify: `crates/aq-server/tests/profiled_results.rs` (register the module)
- Modify: `crates/aq-server/tests/profiled_results/waits.rs` (make the `WaitEnv` helpers `pub(super)`)

**Interfaces:**
- Produces:
  - `pub async fn wakes::suspend(state:&AppState, node:Uuid, attempt:Uuid, wait_id:Uuid) -> Result<wire::SuspensionIntent>`
  - `pub async fn wakes::accept_parking(state, node, attempt, &wire::ParkingReport) -> Result<String /*disposition*/>`
  - `pub async fn wakes::schedule(state, limit:i64) -> Result<Scheduled>`
  - `pub async fn wakes::winner(state, wait_id) -> Result<Option<Admission>>`
  - `pub async fn wakes::record_launch_failure(state, admission_id, reason:&str) -> Result<String>`
  - `pub async fn wakes::manual_resume(state, wait_id) -> Result<bool>`
- `Admission { admission_id, node_id: Option<Uuid>, dispatch_generation: u64, attachment: wire::ContinuationAttachment, differences: Value }`.

- [ ] **Step 1: Write the failing test** `wake_schedules_continuation_on_other_node_when_original_offline`:
  1. Register a HumanDecision wait.
  2. Run `suspend` and then `accept_parking` (RetainedSuspended, with Source and Checkpoint receipts bound to `canonical_digest(binding)`). Assert the disposition is `suspended`.
  3. Resolve the decision and run `waits::reconcile_due`.
  4. Enroll a second node through `store.create_enrollment`, `redeem_enrollment` and `execution_register`.
  5. Set the original node's `last_seen_at` to 10 minutes stale and node 2 to now.
  6. Call `schedule` twice.
  7. Assert:
     - exactly 1 admission row;
     - the winner's `node_id == node2`;
     - the successor attempt and session are fresh, the attempt generation is predecessor+1, and the lineage is the same;
     - `attachment.validate()` succeeds;
     - remaining retries are below `MAX_ATTEMPTS`;
     - the wake state is `admitted`.
- [ ] **Step 2: Run it.** `cargo test --locked -p aq-server --test profiled_results wake_schedules` should FAIL because `wakes` does not exist.
- [ ] **Step 3: Implement the migration and `wakes.rs`** to the interfaces above.
- [ ] **Step 4: Run it again.** The same command should PASS. Also run `state_format_is_the_latest_migration`.
- [ ] **Step 5: Commit.**

### Task 2: Cancellation serialization

- [ ] **Step 1: Write the test** `wake_cancelled_task_does_not_resume`. Insert a `profiled_cancellations` row for the predecessor, then call `schedule`. Assert the wake is `cancelled`, no admission exists, and the intent is `retired`.
- [ ] **Step 2: Make it pass.** Inside the wake row lock, lock `profiled_authority_hot` FOR UPDATE, then check cancellation and confirmed completion.
- [ ] **Step 3: Commit.**

### Task 3: Evidence

- [ ] `docs/implementation/H6.md`, in the H4.md format. Run fmt and clippy, then squash.
