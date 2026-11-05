# F1b Supervisor Dispatch, Reply and Scoped Credential Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** an owner message in the global supervisor conversation reaches the live F1a supervisor session as one bounded turn. The supervisor's reply is then recorded in the same conversation, with a server-derived author and an exact dispatch association. The supervisor acts through one scoped credential bound to its generation.

**Architecture:**
- aq-server owns three things:
  - the dispatch intent row, which maps one migration-37 reservation to the immutable input bytes of one turn;
  - a generation-bound supervisor credential;
  - the reply writer, which uses the existing AQ-11 mailbox owners with a separate reply mailbox key so the central37 global-mail guards stay untouched.
- aq-supervisor-host (F1a) launches the turn with those input bytes as its startup payload. It proves handoff by re-reading the staged `context-0.bin`, then reports `Handoff`, `ReconcileHandoff` or `ReportUnknown` through the central `SessionOwner`. Its verifier checks the journaled fact.
- History merges each reply after the last message of its dispatch batch. It derives `host_delivery` for owner messages from the generation's `DispatchState`.

**Tech Stack:** Rust, axum, sqlx/Postgres, aq-central-authority (`SessionOwner`, `CentralPlan`, `HandoffEvidence`, `ReplyAssociation`), aq-session-host 0.1.7 (vendored).

**Spec:** task steady-harbor-87.2 (F1b). Contract: `docs/superpowers/plans/2026-11-02-v2-full-application-plan.md` and `...-tasks.yaml`. Foundation: `docs/implementation/API-07-supervisor-conversation-foundation.md`.

## Global Constraints

- Acceptance: `cargo test --locked --workspace owner_message_reaches_supervisor_and_reply_is_recorded` passes. A filter that matches zero tests does not count.
- A plan is one turn over one immutable, ordered batch of 1–16 message refs, with no bodies or credentials in the plan.
- Record the dispatch intent and the host-input intent before any host handoff. Host Running is not proof of delivery.
- An uncertain handoff stays `delivery_unknown` until the exact input is reconciled. Unknown or handed-off mail is never re-sent automatically to a successor generation (already enforced by `history::fence` and `Run::successor`; do not weaken it).
- The reply author is server-derived. Logical admission, host delivery and agent receipt stay separate fields.
- The credential:
  - is a single central credential with the scopes `graph.read`, `graph.write`, `enqueue`, `messaging` and `decisions`;
  - is bound to the current generation and expires or is revoked with it;
  - is stored only as a sha256 digest server-side, and in plaintext only in the supervisor host's private session directory;
  - never reaches a worker;
  - creates no fabricated worker, task or attempt identity.
- `supervisor.conversation` becomes true only when dispatch and reply are live.
- Do not edit `docs/architecture/*` or `docs/specs/*`. Use targeted named tests only and add no slow tests. Hardening goes to follow-ups.
- Out of scope: F1a lifecycle, F2 enqueue commands, F3 sandbox messages, G8 panel, F7 chat, F5 questions.

## Review Focus

1. **Reply against a dispatch from a stopped or prior generation.** Expect it refused: the credential check re-reads the current generation at use time. Pinned in Task 3's test, which stops and then posts.
2. **Reply replayed with the same operation_id and a different body.** Expect 409 conflict, and no second mailbox row. Pinned in Task 3.
3. **`context-0.bin` missing or mismatched after Running.** Expect `ReportUnknown`, so history shows `delivery_unknown` and the mail is never re-sent. Pinned in Task 4's unknown-path assertion.
4. **Startup bytes whose digest differs from the dispatch row.** Expect `authorize_dispatch` to refuse, so prepare fails before launch. Pinned in Task 2.
5. **A credential presented on the worker machine router without the messaging scope, or after revocation.** Expect 401/403, with no attempt principal derived. Pinned in Task 3.

---

### Task 1: Migration 48 and the dispatch intent

**Files:**
- Create: `crates/aq-server/migrations/0048_supervisor_dispatch.sql`
- Modify: `crates/aq-server/src/store.rs` (`MIGRATIONS`: add `(48, "0048_supervisor_dispatch.sql")`)
- Create: `crates/aq-server/src/agent_access/messaging/global/dispatch.rs` (declared `pub mod dispatch;` in `global.rs`)
- Test: inline `#[cfg(test)]` in `dispatch.rs`, using the `reservations/tests.rs` fixture pattern

**Interfaces:**
- Produces the following tables:
  - `central_supervisor_dispatches`:
    - columns: `dispatch_id PK`, `host_operation_id UNIQUE`, `actor_id`, `reservation_id`, `generation BIGINT`, `session_id UUID`, `batch_digest TEXT`, `input_digest TEXT`, `input_bytes BYTEA ≤ 1048576`, `created_at_ms`;
    - constraints: `UNIQUE(actor_id,reservation_id)`, `UNIQUE(actor_id,generation)`, `FK(actor_id,reservation_id) → central_supervisor_reservations`;
    - an immutable trigger.
  - `central_supervisor_replies(reply_message_id PK, reply_operation_id, dispatch_id FK, body_digest, association JSONB)`, immutable.
  - `central_supervisor_credentials(token_digest PK, actor_id, generation, session_id, scopes TEXT[], issued_at_ms, expires_at_ms, revoked_at_ms NULL)`, with a partial unique index on `(actor_id, generation) WHERE revoked_at_ms IS NULL`.
- Produces the `dispatch.rs` API:
  - `pub struct Turn { pub batch: MailBatch, pub dispatch_id: Uuid, pub host_operation_id: Uuid, pub input: Vec<u8> }`
  - `pub async fn prepare_turn(pool: &PgPool, actor: Uuid, reservation: Uuid, generation: u64, session_id: Uuid, at_ms: u64) -> Result<Turn, AppError>`. It is idempotent per (actor, reservation): it returns the existing row.
  - `pub fn render_input(conversation: Uuid, members: &[(MailRef, String)]) -> Vec<u8>`. It renders a deterministic UTF-8 turn: a header line, then per message `--- message <id> cursor <n> ---\n<body>\n`.

- [ ] **Step 1:** Write `prepare_turn_records_intent_with_exact_batch_before_host`. Configure the actor, enqueue two owner messages, reserve, and call `prepare_turn`. Assert:
  - `turn.batch` equals the reservation view's batch;
  - `turn.input` contains both bodies;
  - the row's `input_digest` equals `Digest::of(&turn.input)`;
  - a second call returns the same ids and bytes;
  - `state_format_is_the_latest_migration` still passes.
- [ ] **Step 2:** Run `aq test` with the filter `prepare_turn_records_intent`. Expect FAIL (module missing).
- [ ] **Step 3:** Implement the migration, the `MIGRATIONS` entry and `prepare_turn`. The reservation must be `awaiting_prepared_plan`. Read the members and bodies via `central_supervisor_mail_members JOIN mailbox_messages`, ordered by ordinal.
- [ ] **Step 4:** Run the test plus `state_format_is_the_latest_migration` and the central37 storage-guard tests (`central37`). Expect PASS.
- [ ] **Step 5:** Commit.

### Task 2: Plan authorization joins the dispatch row

**Files:** modify `dispatch.rs`.

**Interfaces:**
- Produces `pub async fn authorize_dispatch(tx: &mut Transaction<'_, Postgres>, plan: &CentralPlan, startup_digest: &Digest) -> Result<(), AppError>`. It requires the dispatch row to match all of the following:
  - `plan.dispatch_id`, `plan.host_operation_id` and `plan.subject.actor_id()`;
  - `plan.generation`;
  - `plan.session` (its session_id);
  - `plan.batch.digest()`, which must equal both `batch_digest` and the reservation's batch;
  - `startup_digest == input_digest`.

- [ ] **Step 1:** Write `authorize_dispatch_requires_exact_recorded_turn`. Build a plan from the Turn using the example-plan template with the subject actor and conversation set. Assert `Ok`. Then mutate each of these one at a time and assert `Err`: startup digest, dispatch_id, generation, batch message order.
- [ ] **Step 2–4:** Run it and see it fail, implement, then run it and see it pass.
- [ ] **Step 5:** Commit.

### Task 3: Scoped credential and reply writer

**Files:**
- Create: `crates/aq-server/src/agent_access/messaging/global/reply.rs`
- Modify: `global.rs` to add `pub fn supervisor_routes(state) -> Router<AppState>`, mounting POST `/v1/supervisor/conversation/replies`
- Modify: `src/lib.rs`, merging `supervisor_routes` into the machine router next to `agent_access::machine_routes`

**Interfaces:**
- `pub const SCOPES: [&str; 5] = ["graph.read","graph.write","enqueue","messaging","decisions"]`
- `pub struct SupervisorPrincipal { pub actor_id: Uuid, pub generation: u64, pub session_id: Uuid, pub scopes: Vec<String> }`. No attempt, task or worker fields.
- `pub async fn issue_credential(pool: &PgPool, actor: Uuid, ttl_ms: u64, at_ms: u64) -> Result<String, AppError>`:
  - It requires `SessionOwner::inspect(actor)` to be at the head generation, in phase StartCommitted or RunningReported.
  - It revokes prior unrevoked rows for the actor.
  - It stores `sha256(token)` and returns `"aqsv1_" + 32 random bytes hex` once.
- `pub async fn authenticate(pool: &PgPool, bearer: &str, scope: &str, at_ms: u64) -> Result<SupervisorPrincipal, AppError>`. At use time it requires all of: the digest is present, not revoked, not expired, the scope is held, the generation equals the head generation, and the phase is active. Otherwise 401.
- `pub async fn live(pool: &PgPool) -> bool`: true when any actor's head generation is active, which implies dispatch and reply are live.
- Reply request `{operation_id, message_id, dispatch_id, body}`. The body limit is the same as for owner messages: 16384 bytes, non-blank, no NUL.
  - The author is `{"kind":"central_supervisor","actor_id","generation","session_id"}`. It is derived from the credential only.
  - The recipient is `{"kind":"global_supervisor_reply","conversation_id"}`, with thread = conversation.
  - The dispatch row must have `actor_id == principal.actor_id` and `generation == principal.generation`. Its association is `run.association()`, and `association.dispatch_id` must equal the requested dispatch_id.
  - Write in one transaction:
    - the retention budget update;
    - `append`;
    - insert `central_supervisor_replies` with `ReplyAssociation{dispatch, reply_message_id, reply_operation_id, body_digest}`;
    - `receipt` with contract `supervisor-conversation-reply-1`.
  - The response is `{contract, reply_message_id, operation_id, dispatch_id, cursor, author, logical_admission:"queued", host_delivery:"not_attempted", agent_receipt:null}`.
  - Replaying the same request returns an identical receipt. A different body returns 409.

- [ ] **Step 1:** Write `supervisor_reply_is_authored_by_credential_generation` (in-crate, synthetic run via a test `EvidenceVerifier` that returns Ok). Assert:
  - the reply cursor is recorded;
  - the stored author equals the server-derived author;
  - a replay is identical and a different body gets 409;
  - a missing or other scope gets 401/403;
  - the credentials table holds only the digest (the plaintext is not in any column);
  - after `report(Stopped)` or with the generation not active, `authenticate` gets 401.
- [ ] **Step 2–4:** Run it and see it fail, implement, then run it and see it pass.
- [ ] **Step 5:** Commit.

### Task 4: Host handoff evidence and verifier (aq-supervisor-host)

**Files:**
- Modify: `crates/aq-supervisor-host/src/lib.rs`, adding `pub async fn handoff(&self, actor: Uuid) -> Result<Snapshot>` and `pub async fn install_credential(&self, actor: Uuid, token: &str) -> Result<PathBuf>`
- Modify: `crates/aq-supervisor-host/src/verifier.rs` to handle `Handoff`, `ReconcileHandoff` and `ReportUnknown`
- Test: `crates/aq-supervisor-host/tests/supervisor_session.rs`

**Interfaces:**
- `handoff`:
  - It holds the actor fence and requires a RunningReported run with an association.
  - It reads `<root>/<launch_id>/context-0.bin` (bounded to 1 MiB, regular file, no symlink).
  - If the digest equals `host.startup.payload.digest`, it journals `"handoff"` `{launch_id, association, disposition:"handed_off", input: digest}`. It then reports `Handoff`, or `ReconcileHandoff` when the dispatch is `Unknown`, with `receipt` = the fact ref.
  - Otherwise it reports `ReportUnknown{association}`.
- Verifier:
  - `Handoff` and `ReconcileHandoff` look up the run's launch_id from `central_host_launches`. They then call `original(tx, launch_id, "handoff", &evidence.receipt)`, decode it, and require the body's association to equal `evidence.association`, which must equal `run.association()`. The disposition must match.
  - `ReportUnknown` requires `association == run.association()`.
- `install_credential` writes the token 0600, `O_EXCL`, to `<root>/<launch_id>/supervisor.credential`. That is the supervisor host's private session directory, never a worker workspace.

- [ ] **Step 1:** Extend the fixture test. After `start`, `handoff` gives `DispatchState::Resolved{HandedOff}`. Replaying it gives Replayed. A case with a tampered `context-0.bin` gives `Unknown`.
- [ ] **Step 2–4:** Run it and see it fail, implement, then run it and see it pass.
- [ ] **Step 5:** Commit.

### Task 5: History, schema, capability and acceptance

**Files:**
- Modify: `global/reads.rs` to merge reply items and derive `host_delivery`
- Modify: `protocol/human-supervisor-conversation/v1/schema.json`, additively. The history item becomes `anyOf` the human item or a supervisor item. The human `host_delivery` becomes an enum: `not_attempted`, `pending`, `delivery_unknown`, `delivered`, `not_delivered`.
- Modify: `dashboard_api/service.rs:~260`, setting `supervisor.conversation` to `mounted.supervisor_conversation && a.owner && supervisor_conversation::dispatch::live(pool)`
- Test: `crates/aq-supervisor-host/tests/supervisor_session.rs`, adding `owner_message_reaches_supervisor_and_reply_is_recorded`
- Modify: `crates/aq-server/src/agent_access.rs`, re-exporting `ReservationOwner` publicly (via `supervisor_conversation::reservations`, already `pub mod`)

**Interfaces:**
- The history item for a reply is `{message_id, thread_id, conversation_id, cursor, author:{kind:"supervisor",actor_id,generation,session_id}, body, body_digest, created_at_ms, dispatch:{dispatch_id,host_operation_id,batch_digest}, logical_admission:"queued", host_delivery:"not_attempted", agent_receipt:null}`.
- `cursor` is the anchor (the batch's max cursor). The reply is placed after its anchor message in both directions. Paging counts only owner rows.
- Owner `host_delivery` comes from the generation of the dispatch containing the message:
  - none or NotAttempted → `not_attempted`
  - IntentCommitted → `pending`
  - Unknown → `delivery_unknown`
  - Resolved HandedOff → `delivered`
  - Resolved NotHandedOff → `not_delivered`

- [ ] **Step 1:** Write the acceptance test (real tmux, about 2 s). It follows these steps in order:
  1. The owner enqueues through the mounted `supervisor_conversation::routes`.
  2. Configure the actor and reserve.
  3. Call `prepare_turn`.
  4. Build the CentralPlan and HostPlan from the Turn, with the startup payload set to `turn.input`.
  5. Call `Supervisor::start`, with a `PlanAuthority` that calls `authorize_dispatch`.
  6. Assert that the `delivered` file contains the owner body.
  7. Call `handoff`, then `issue_credential`, then `install_credential`.
  8. POST the reply on `supervisor_routes` using the token read from the installed file.
  9. Fetch history and assert the owner message is `delivered`, and the next item is the supervisor reply with the matching `dispatch.dispatch_id`. Both items validate against the schema.
  10. Assert `live()` is true and the bootstrap capability is true.
  11. Stop the session.
  12. Assert the credential gets 401, `live()` is false, and the DB contains no plaintext token.
- [ ] **Step 2–4:** Run it and see it fail, implement, then run it and see it pass. Also run `supervisor_conversation` (the existing history and capability tests still pass, and the capability is false with no live session) and `aqv2` `cli` schema tests.
- [ ] **Step 5:** Commit.

### Task 6: Evidence, follow-ups, delivery

- [ ] Write `docs/implementation/F1b.md`: the commands, environment, commit, one evidence line per acceptance item, and limits.
- [ ] File follow-ups with `aq task create --integration-mode direct`:
  - the reservation moving to plan-bound or consumed after a turn, since an awaiting reservation blocks the next reserve;
  - production daemon wiring of prepare_turn, start, handoff and credential into `aq-supervisor-host` `main.rs`;
  - history reply cursoring, because `after` polling misses a reply anchored at an already-seen cursor;
  - enforcing the graph, enqueue and decisions scopes in F2/F3.
- [ ] Add a `CROSS-REPO FOLLOW-UP (aq-dashboard):` comment covering the schema additions (supervisor item and host_delivery enum).
- [ ] Squash, `git merge origin/main`, run the targeted tests, `aq git push`, then close.
