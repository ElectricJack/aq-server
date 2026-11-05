# D3 Automatic Result Settlement and Graph-Event Outbox Consumer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** when a worker result is recorded, a policy actor evaluates it automatically. Under the default completion policy, the graph task is completed only when server-owned evidence supports it. Confirmed graph events in `agent_graph_outbox` reach the embedded playbook host exactly once.

**Architecture:**
- **Settlement.** A new module, `profiled_execution/settlement/automatic.rs`, runs after both commit paths of `results::record` (a new report, and the duplicate re-POST that re-drives an unfinished settlement).
  - It applies only to outcomes whose review is `PolicyActor` and whose `validation_policy` resolves, through `profiled_execution.definition_bodies`, to the body `{"schema_version":1,"kind":"aq.default-completion.v1","delivery":"required"|"not_required"}`. Any other outcome returns `NotApplicable` and writes nothing, so the human-owner pilot path is unchanged.
  - The pure ordered gate is in `settlement/default_policy.rs`:
    1. accepted: current head, outcome Success, stopped, not cancelled, not revoked;
    2. required checks: each one needs a server-owned `profiled_check_evidence` row bound to the exact retained source (the import operation and checkpoint OID);
    3. source retained: the central receipt, via `source_retention::load_confirmed_locked`;
    4. delivery, when the policy requires it: `profiled_delivery_evidence` must say `published`;
    5. a version-checked graph `update_node`, sent with the frozen `expected_version` and a stable operation ID.
  - The result is recorded in `profiled_auto_settlements`, keyed by report.
- **Refusal.** A definite refusal (a failed check, unpublished delivery, a result that is not accepted, missing source) is recorded as `rework_required` with its reason and evidence. Missing check evidence, indeterminate delivery and a graph version conflict are recorded as `held`. In both cases the task is not completed and `holds_task` stays true, so dependents are not released.
- **Completion.** Only a validated graph receipt sets the row to `confirmed` and releases `holds_task`. `settlement_claim_guard` accepts that row as release proof.
- **Seams.** The check evidence table is the D6 seam and the delivery evidence table is the D1a/D1b seam. Both are server-owned tables with no agent or worker route, and D3 never writes them.
- **Outbox consumer.**
  - A new playbook catalog profile, `GraphEvents3`, is `HumanReview2` plus the event contract `aq.graph.operation.confirmed` v1, whose trusted producer is `aq.graph.operations@1`.
  - The driver quantum calls `collect_graph_events(≤2)`. It seals undelivered `agent_graph_outbox` rows for the namespace's workspace and project into `playbook_outbox` as pending, with event_id equal to the graph event_id, epoch 0 and the stable producer.
  - `acknowledge_event` sets `agent_graph_outbox.delivered=true` in the same transaction that records durable engine acceptance.
  - Crash replay re-sends the pending row. The engine deduplicates on `digest(producer, epoch, event_id)`.

**Tech Stack:** Rust, axum, sqlx/Postgres, aq-playbook-host (vendored aq-playbooks 0.1.3).

**Spec:** task crisp-meadow-36.4 (D3); `docs/specs/0002-execution-and-agent-access.md` §7, §9.

## Global Constraints

- Acceptance (each named filter must match at least one test, and every matched test must pass):
  - `cargo test --locked --workspace false_pass_result_does_not_complete_task`
  - `cargo test --locked --workspace accepted_result_completes_task_with_expected_version`
  - `cargo test --locked --workspace outbox_replay_delivers_each_event_once`
- An agent's claim (the report conclusion or outcome) is never evidence. D3 runs no checks.
- A version conflict never overwrites: it re-evaluates and holds.
- aq-graph owns task state. D3 adds no writable task lifecycle; its AQ rows are evidence records.
- Do not edit `docs/architecture/*` or `docs/specs/*`. Use targeted tests only and no slow tests.
- Out of scope: D6, D1a/D1b, B4, D9, D8, F4.

## Review Focus

1. A success claim with a failing check, or with required delivery unpublished, must not complete the task and must be marked `rework_required` with evidence. Pinned by t1.
2. Check evidence for a different source or checkpoint must not count. Pinned in the t1 module.
3. Completion sends `expected_version` equal to the frozen task version and a stable idempotency key. A replayed POST must not mint a new operation. Pinned by t2.
4. `delivered` must be set only after engine acceptance, and a replay must not produce a second engine acceptance. Pinned by t3.

## Tasks

### Task 1: Migration 0052 (state format 52)

**Files:**
- Create: `crates/aq-server/migrations/0052_auto_settlement.sql`
- Modify: `crates/aq-server/src/lib.rs` (`STATE_FORMAT_VERSION`, `READABLE_STATE_FORMATS`), `crates/aq-server/src/store.rs` (MIGRATIONS), `Dockerfile` (`AQ_STATE_FORMAT`)

- [ ] Add the tables `profiled_check_evidence`, `profiled_delivery_evidence` and `profiled_auto_settlements`. Add an owner trigger on the auto table that requires `aq.settlement_owner='aq.settlement-owner.auto.1'`.
- [ ] `CREATE OR REPLACE settlement_claim_guard`, adding the confirmed auto row as an alternative release proof.
- [ ] Widen the `playbook_outbox.producer` check with `aq.graph.operations@1`, and the `playbook_namespaces.catalog_profile` check with 3.
- [ ] Bump the format to 52.

### Task 2: Tests first (they must fail)

- [ ] `tests/profiled_results/auto_settlement52.rs`: setup with the default policy plus the fake graph; t1 `false_pass_result_does_not_complete_task` (a failing check, delivery required but unpublished, and evidence for the wrong source); t2 `accepted_result_completes_task_with_expected_version`.
- [ ] `tests/playbook_bridge.rs`: t3 `outbox_replay_delivers_each_event_once`.

### Task 3: Default policy and automatic settlement

- [ ] `settlement/default_policy.rs`: the pure gate (`Gate` → `Verdict::{Complete, Rework, Hold}`).
- [ ] `settlement/automatic.rs`: `settle_after_record(state, report_id)`, covering evaluation, recording, dispatch, confirmation and release.
- [ ] Call it from `results::record` after commit. Log failures; they never fail the worker's report.

### Task 4: Catalog profile 3 and the outbox consumer

- [ ] `review_domain.rs`: `GraphEvents3`, `graph_operation_event()`, `has_reviews()`.
- [ ] `service.rs`: `new_profile3`, `initialize` stamps `profile.number()`, `collect_graph_events`, and the ack marks `delivered`.
- [ ] `driver.rs`: quantum calls `collect_graph_events(2)`.
- [ ] `config.rs`/`runtime.rs`: accept the `graph_events_v3` profile.

### Task 5: DoD and evidence

- [ ] Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and the three named tests plus the touched suites (`profiled_results`, `playbook_bridge`).
- [ ] Write `docs/implementation/D3.md`.
