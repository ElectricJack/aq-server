# F5 Agent Questions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An agent asks the owner a question and waits. The owner's answer, which targets the exact question revision, wakes the waiting attempt exactly once. If the attempt has already ended, the answer goes to its continuation as task-addressed mail.

**Architecture:** A new `question_decisions` owner table holds typed `question` decisions. The H4 `HumanDecision` source adapter falls back to this table when no review decision matches the ID. The agent route asks; inside one transaction it creates the decision and registers the H4 wait. A G1b action kind `question.answer` resolves the decision. When the asking attempt has ended, the same transaction queues task-addressed F3 mail.

**Tech Stack:** Rust, axum, sqlx/Postgres, `aq_wait_engine::wire`, schemars.

**Spec:** AQ task steady-harbor-87.6 (F5) description; findings comment on that task.

## Global Constraints

- Route name: `POST /v1/agent/questions` (ask), `POST /v1/agent/questions/show` (read answer). Per-attempt credential, capability `question.ask`.
- Discord reactions/replies are never answers; acknowledging a notification is not answering.
- F4 notification carries only the decision id, never question text (`observe_question`).
- Old session-addressed mail is never retargeted; ended-attempt delivery is `Recipient::Task`.
- Migration must be contiguous: `0092_question_decisions.sql`, `UPDATE meta SET value='92'`.
- No slow tests; targeted tests only.

## Review Focus

- Replay of ask with the same operation_id and same body returns the identical decision; changed body → `id_conflict` (409). Pinned in Task 2 test.
- Answer with a stale `expected_revision` is a conflict and changes nothing. Pinned in Task 3 test.
- Answer replay (same owner, same operation, same text) returns the same receipt; a second different answer conflicts. Pinned in Task 3 test.
- Duplicate wait reconciliation records one outcome and one ready intent. Pinned in Task 3 test.
- A live attempt gets no mail (wake via wait only); an ended attempt gets exactly one task-addressed message. Pinned in Tasks 3 and 4.

---

### Task 1: Owner table and wait-source fallback

**Files:**
- Create: `crates/aq-server/migrations/0092_question_decisions.sql`
- Modify: `crates/aq-server/src/agent_access/waits/sources.rs` (`decision()`)

**Interfaces:**
- Produces: table `question_decisions(decision_id PK, attempt_id FK execution_attempts, operation_id, node_id, project_id, workspace_id, task_id, session JSONB, attempt_generation, question TEXT ≤4096 bytes, question_digest TEXT, subject_revision TEXT, decision_revision BIGINT, state pending|answered, expires_at_ms, wait_id, created_at_ms, answer ≤16384, answered_by, answer_operation_id, receipt_digest, answered_at_ms, delivery JSONB)`, `UNIQUE(attempt_id,operation_id)`, unique `(answered_by,answer_operation_id)`.
- Source: when no review row, read `question_decisions`; match `task_id==subject_id`, `subject_revision`, `question_digest`, scope; same state mapping as reviews; result artifact `question-decision-receipt-{id}`.

- [ ] Write migration; `cargo check -p aq-server`.
- [ ] Extend `decision()`; covered by Task 3's test.

### Task 2: Ask service and agent route

**Files:**
- Create: `crates/aq-server/src/agent_access/questions.rs`
- Modify: `crates/aq-server/src/agent_access.rs` (mod + route merge), `crates/aq-server/src/agent_access/waits.rs` (expose `register_locked` as `pub(crate)`)
- Test: `crates/aq-server/tests/profiled_results/questions.rs` (registered in `profiled_results.rs`)

**Interfaces:**
- `pub async fn ask(state:&AppState, node:Uuid, attempt:Uuid, operation_id:Uuid, question:&str) -> Result<Value, QuestionError>`; response `{api_version:1, decision_id, decision_revision:"1", kind:"question", state, subject_id(task), subject_revision, question_digest, wait_id, expires_at_ms}`.
- `pub async fn show(state, node, attempt, decision_id) -> Result<Value, QuestionError>`; adds `answer` when answered; scoped to caller's task.
- `pub struct QuestionError(pub &'static str)`; codes `invalid_request`, `id_conflict`, `not_found`, `wait_conflict`, plus `load_caller` codes.
- subject_revision = `attempt:{attempt_id}:{generation}`; question_digest = `wire::Digest::of(question)`; expiry = now + `aq_wait_engine::MAX_AGE_MS`.

- [ ] Test (part of `question_answer_resumes_waiting_attempt`): ask → wait row phase `pending`; replay → identical JSON; changed body same op → `id_conflict`.
- [ ] Implement; F4 `observe_question(operation_id, scope, decision, true)` after commit (best-effort).

### Task 3: Answer service, action kind, wake

**Files:**
- Modify: `questions.rs` (answer + `QuestionAnswer` ActionKind `question.answer`), `dashboard_api/actions/existing.rs` (register + schema), `messaging.rs` (`question_send_in_tx`), `protocol/dashboard-actions/v1/schema.json` (regenerate)

**Interfaces:**
- `pub async fn answer(state, owner:Uuid, decision_id, expected_revision:u64, operation_id, answer:&str) -> Result<Value, QuestionError>` → `{decision_id, decision_revision, receipt_digest, delivery:{kind:"wait"}|{kind:"task_mail",message_id}}`.
- Action payload `{operation_id, answer}`; subject `{type:"decision", id, revision}`.

- [ ] Test `question_answer_resumes_waiting_attempt`: stale revision → `stale_revision`; answer → ok; replay same → equal; reconcile twice after +61s/+125s → one `satisfied` outcome, intents `["ready"]`; `show` returns answer; no `mailbox_messages` with thread = decision.
- [ ] Implement; regenerate schema; run `cargo test --locked -p aq-server --test profiled_results question_` and the action schema test.

### Task 4: Ended attempt → continuation mail

- [ ] Test `question_after_attempt_ended_routes_to_continuation`: ask; revoke launch binding (attempt ended); answer → delivery `task_mail`; exactly one message, recipient kind `task`, thread = decision_id, body contains answer, sender component `question_answer`; no session-addressed mail.
- [ ] Implement ended check (`NOT holds_task` OR cancellation_id OR binding revoked) inside answer tx.

### Task 5: Contract, evidence, close

- [ ] `protocol/agent-access/v1/questions.json` route contract + README note.
- [ ] `docs/implementation/F5.md`; fmt/clippy; squash, merge origin/main, renumber migration, push, close.
