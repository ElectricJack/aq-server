# H4 Durable Waits Registry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An agent can register a durable, verified wait on a message, task, dependency or human decision. It can inspect the wait and consume its outcome exactly once, across replays and restarts.

**Architecture:** A SQL registry in aq-server (migration 0076) stores one `aq_wait_engine::Snapshot` per wait. Every change runs under `SELECT … FOR UPDATE`, with compare-and-swap on the engine revision. Source adapters turn owner rows into `CommittedObservation`s:
- mailbox rows for Message;
- graph nodes for Task and Dependency;
- `human_decision_requests` for HumanDecision.

An outcome and its resume intent commit in the same transaction. A periodic reconciler drives due waits.

**Tech Stack:** Rust, axum, sqlx/Postgres, `aq-lifecycle-api` wire types, `aq-wait-engine` reducer.

**Spec:** task clear-crest-61.4 (H4) in `docs/superpowers/plans/2026-11-02-v2-full-application-tasks.yaml`.

## Global Constraints

These apply to every task in this plan.

- Contracts: a wire change publishes schemas and fixtures from the contract owner.
- Explicit refusals: unsupported capabilities and source kinds refuse explicitly; nothing silently degrades.
- Named tests are the evidence:
  - `wait_on_decision_wakes_once_after_resolution` must exist and pass under `cargo test --locked --workspace`;
  - `wait_registration_replay_is_idempotent` must exist and pass under `cargo test --locked --workspace`.
- Definition of done: `cargo fmt --check`, plus clippy `-D warnings` on the changed crates, plus the named tests. Evidence goes in `docs/implementation/H4.md`.
- Bounds:
  - at most 32 members per wait;
  - at most one active wait per attempt;
  - maximum age 30 days;
  - self-waits and known cycles are refused.
- Do not edit `docs/architecture/*` or `docs/specs/*`.

## Review Focus

1. **Replay with changed bytes.** The same `operation_id` with a different body must return `changed_payload`, never a second wait.
2. **Replay after time passes.** A replay must return the original `original_created_at_ms` and must not reset the deadline. This is asserted in `wait_registration_replay_is_idempotent`.
3. **Duplicate reconcile after a decision resolves.** Exactly one outcome row and one resume intent may exist. This is asserted in `wait_on_decision_wakes_once_after_resolution`.
4. **Ack with a stale continuation generation.** It must return `stale_generation` and must not consume the intent.
5. **A second wait from the same attempt while one is active.** It must be refused, and the partial unique index enforces this.

---

### Task 1: Registry schema and engine wiring

**Files:**
- Create: `crates/aq-server/migrations/0076_agent_waits.sql`. Its tables:
  - `agent_waits`, with `UNIQUE(caller_key, operation_id)` and a partial unique index on `attempt_id WHERE phase<>'terminal'`;
  - `agent_wait_outcomes`;
  - `agent_wait_resume_intents`;
  - `agent_wait_acks`.
  It ends with `UPDATE meta SET value='76' …`.
- Modify: `crates/aq-server/Cargo.toml` to add `aq-wait-engine`.
- Modify: `crates/aq-wait-engine/src/reduce.rs` to add read accessors:
  - `registration()`;
  - `boundaries() -> Option<Vec<SourceBoundary>>`, the original boundaries, present only once every member is installed;
  - `hold_reason()`;
  - `cycle()`;
  - `ids()`;
  - `inflight()`;
  - `member_states()`.

### Task 2: Registry core and source adapters

**Files:**
- Create: `crates/aq-server/src/agent_access/waits.rs`. This is the registry core:
  - `register(state, tx, caller, request) -> wire::Response`;
  - `drive(state, tx, wait_id, now) -> Result<bool>`;
  - `reconcile_due(state, limit) -> Result<usize>`;
  - `inspect`;
  - `ack`;
  - `owner_list`;
  - `owner_show`.
- Create: `crates/aq-server/src/agent_access/waits/sources.rs`. It holds the Message, Task/Dependency and HumanDecision adapters. Each returns an `aq_wait_engine::SourceReply`.
- Create: `crates/aq-server/src/agent_access/waits/routes.rs`. Routes:
  - agent routes `POST /v1/agent/waits/register`, `/inspect` and `/ack`, under capabilities `aq.lifecycle-api.wait`, `.inspect` and `.continuation-ack`;
  - owner routes `GET /api/waits` and `GET /api/waits/{wait_id}`.

**Design rules:**
- `Caller{scope, binding: wire::Binding, principal: Digest, caller_key}` is derived from the authenticated principal. The `lineage_id` comes from `task_lineage_id`, and a mismatched registration gives `scope_denied`.
- The fence's `task_cancelled` is set when `execution_attempts.state->>'cancellation_id'` is non-null.
- A source kind outside Task, Dependency, Message and HumanDecision gives `unsupported_capability`.
- The `KnownGraph` is built from the task edges of active waits in the same project. Cycle and Capacity errors map to `known_cycle`.
- Register inserts the row, then runs Install and Reobserve inside the same locked transaction (a bounded loop of 4 rounds), then commits.
- The response is computed from the stored `registered` body, and is the same on every replay.
- A snapshot loaded with in-flight work is Reclaimed first, because the row lock proves no other worker owns it.
- Message: rows in the caller's task mailbox for `thread_id` with `cursor > exclusive_cursor`, filtered by the sender constraint. Task-addressed mail is never consumed, so a successor still sees it.
- HumanDecision: `pending` maps to Pending. `answered` maps to Satisfied, with the finality receipt as evidence. `expired` and `cancelled` map to SourceFailed. The decision's `subject_digest`, `request_digest` and scope must match the condition.

### Task 3: Acceptance tests (written first, then made green)

**Files:**
- Create: `crates/aq-server/tests/profiled_results/waits.rs`, included from `tests/profiled_results.rs`.

- [ ] `wait_registration_replay_is_idempotent`:
  1. Register a HumanDecision wait.
  2. Advance the clock by 5 s and replay the same request. The response must equal the first and `original_created_at_ms` must not change.
  3. Replay with a changed body. The response must be `Refused{changed_payload}`.
  4. A second wait from the same attempt must be refused.
  5. Assert there is exactly one `agent_waits` row.
- [ ] `wait_on_decision_wakes_once_after_resolution`:
  1. Admit a pending decision and register the wait. The phase must be pending.
  2. Resolve the decision through `reviews::Service`, then advance the clock.
  3. Run `reconcile_due` twice. Assert one Satisfied outcome, one resume intent in state `ready`, and the same outcome from inspect.
  4. Ack twice. Both acks must return the same receipt and the intent must be `consumed`.
  5. An ack with a stale generation must return `stale_generation`.
- Run: `cargo test --locked --workspace wait_on_decision_wakes_once_after_resolution` and the `replay` test. Expected: one test runs in each, and it passes.

### Task 4: Contract publication and evidence

- Create `protocol/agent-waits/v1/README.md` and `routes.json` (the route → command → capability table), plus `fixtures.json` (sealed register, inspect and ack requests). The schemas are `crates/aq-lifecycle-api/artifacts/{request,response}.schema.json`.
- Unit test `wait_route_fixtures_decode` checks that each fixture passes `decode_request` and that the route capability matches.
- Write `docs/implementation/H4.md`. It records:
  - commands and results;
  - that `source_unavailable` is reported as member and hold freshness, because the engine refuses it as a terminal kind;
  - out-of-scope items.
