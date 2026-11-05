# D6 Judgment and Check Adapters Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Execute registered sandbox checks and bounded provider judgments through durable playbook operations, with actual owner evidence.

**Architecture:** Extend the D5 grant/intent/outbox transactions with separate check and judgment owners. Persist each exact operation before dispatch and reconcile its original executor operation after uncertainty. Consume qualified execution and credential APIs; do not implement another sandbox backend, controller, credential vault or task lifecycle.

**Tech Stack:** Rust, Tokio, SQLx/PostgreSQL, protected_storage, aq-playbook-host and a pinned aq-playbooks catalog.

**Spec:** Task `crisp-meadow-36.7`; D6 in `2026-11-02-v2-full-application-tasks.yaml`; `docs/implementation/AQ-14-decision-check-owner-design.md`, “Registered executed checks” and “Storage026 and bounded retained history.”

## Global Constraints

- Rust uses the committed lockfile with `--locked`; no sibling path dependencies or floating release inputs.
- A wire change publishes schemas and fixtures from the contract owner, and both endpoints consume a pinned version.
- AQ owns grants, attempts and dispositions; aq-graph owns task state.
- No secrets in prompts, logs, assignments, descriptions or provenance.
- Unsupported required capabilities refuse explicitly; nothing silently degrades.
- No second writer after uncertainty; stopped-writer proof precedes release/replacement.
- Requests/receipts <=64 KiB, required checks <=32, evidence references <=64, executions <=8 per exact subject/check, review/check operations <=256 per original task lineage, retained decisions/checks <=10,000, protected owner payloads <=64 MiB.
- Run the two named acceptance tests and focused related tests. The prime's 2026-11-02 velocity rule narrows the program's broad test requirement; no full-suite qualification run.
- Preserve existing catalog/grant generations. Do not modify canonical architecture/spec documents in D6.
- Integration owns delivery to main; publish only the assigned branch.

## Review Focus

- Lost response: reconcile the original stable operation; assert a single executor start.
- Missing input/unknown process stop: assert Unknown, never Passed.
- Subject/validator mismatch: refuse before execution; stale completion cannot create current evidence.
- Token/cost/deadline exhaustion: typed refusal, retained provenance and no tool loop.
- Quota exhaustion/replay: exact retained replay succeeds before quota checks; concurrent admission cannot exceed bounds.

## Task 1: Resolve prerequisite interfaces (implementation gate)

**Files inspected:**
- `vendor/aq-host-catalog-1.0.0/registry.json`
- `crates/aq-server/src/playbooks/{driver,host_adapters,review_domain,service}.rs`
- `crates/aq-server/src/profiled_execution/producer/{mod,build,compose}.rs`
- `crates/aq-server/src/{provider_accounts.rs,intelligence/config.rs}`

**Interfaces available:** `Service::host_dispatch(&RuntimeOwner, Uuid, i64, &Intent) -> Result<HostResult>`, `Service::host_retained(Uuid, i64, &Intent) -> Result<Option<HostResult>>`, Host5 registry and namespace/grant identity, native agent execution, sanitized credential inventory.

**Interfaces not identified:** a qualified registered-check execution profile/admission/result owner and a bounded direct judgment provider owner with permitted credential selection. The current native producer constructs an agent profile and pins `required_checks: vec![]`. The existing Responses transport is scoped agent forwarding, not a judgment operation owner.

- [x] Read the task and named source files; record findings in the task.
- [x] Inspect only the reference sections needed to resolve execution and catalog ambiguities.
- [x] Send the owner the concrete interface/contract decision request (`msg-9e9ea6e6b4d34a228c1965f23f170783`).
- [ ] Receive the contract-owner release/compatibility decision and identify the actual configured check/provider executor APIs. Record their exact signatures here before proceeding. Do not substitute fake executors or an in-process integrity check for sandbox acceptance.

The frozen D4 1.0.0 catalog currently has synchronous `judgment.request(choices, context, question, subject)` and `check.request(execution_context, subject, suite)` contracts. Judgment has neither an accepted-handle outcome nor a completion contract. `aq.operation.updated` is restricted to accepted/changes_requested/failed dispositions; it does not carry an assessed choice. Whether to release a new contract or define a compatible registration-based interpretation is an owner decision, not a local edit of the pinned artifact.

## Task 2: Registered sandbox check owner (after Task 1)

**Files:** Create `crates/aq-server/src/playbooks/checks.rs`, `crates/aq-server/tests/playbook_checks.rs`, and the next free `crates/aq-server/migrations/NNNN_review_checks.sql`; modify `playbooks/{mod,driver,service,review_domain}.rs` and the migration catalog as required.

**Consumes:** the exact qualified executor API and catalog release recorded in Task 1; immutable result/outcome/source owners; D5 authority/identity/outbox helpers.

**Produces (local proposed API):** `Service::request_check(&RuntimeOwner, Uuid, i64, &Intent) -> Result<HostResult>` and `Service::reconcile_check(Uuid, i64, &Intent) -> Result<Option<HostResult>>`; immutable `review_check_receipts` read by later D3/D7 work.

- [ ] Write `check_request_runs_in_sandbox_and_reports_typed_outcome`. Assert an actual execution-profile sandbox attempt, an intent committed before start, exact pinned check ID/validator/source/input binding, retained artifacts and stop/exit evidence, typed Passed and Failed results, and `aq.check.completed` publication.
- [ ] Run `cargo test --locked --workspace check_request_runs_in_sandbox_and_reports_typed_outcome`; observe failure against the missing adapter, not a zero-test match.
- [ ] Add focused tests for the five review-focus cases, all six dispositions (Passed, Failed, Cancelled, TimedOut, Unknown, Stale), foreign grants and same-ID/different-payload conflicts.
- [ ] Implement strict registered selection from the pinned outcome; derive original lineage from retained owner records. Reject arbitrary commands, routes and verdicts.
- [ ] Add durable intent/receipt storage, serialized quota admission, immutable evidence, retained replay before capacity, and transactionally sealed completion events. Check actual combined retained decision/check counts.
- [ ] Dispatch the qualified executor using the durable operation ID. Reconcile original attempts on uncertainty; never allocate another operation as an implicit retry.
- [ ] Map Passed→passed, Failed→failed, Cancelled/TimedOut/Unknown→infrastructure_failure, Stale→refused while preserving the distinct receipt disposition; confirm this mapping against the contract-owner decision in Task 1.
- [ ] Run the named test and `cargo test --locked -p aq-server --test playbook_checks`; require every test to pass.
- [ ] Commit implementation and test evidence.

## Task 3: Bounded direct provider judgment owner (after Task 1)

**Files:** Create `crates/aq-server/src/playbooks/judgments.rs` and `crates/aq-server/tests/playbook_judgments.rs`; extend the owner migration and `playbooks/{mod,driver,service,review_domain}.rs` using the catalog release from Task 1.

**Consumes:** the permitted credential/provider API and bounded request configuration recorded in Task 1; D5 identity/grant/outbox helpers. No agent session or arbitrary tool loop.

**Produces (local proposed API):** `Service::request_judgment(&RuntimeOwner, Uuid, i64, &Intent) -> Result<HostResult>` and `Service::reconcile_judgment(Uuid, i64, &Intent) -> Result<Option<HostResult>>`; durable operation handles and protected completion provenance.

- [ ] Write `judgment_request_respects_budget_and_records_provenance`: exercise the qualified provider port with a deterministic provider fixture; assert explicit context/question/schema, host-selected permitted credential, accepted durable handle and completion event. Assert provider/model/profile reference, prompt/context digest, token/cost usage and output digest; no secret bytes in stored/public provenance.
- [ ] Run `cargo test --locked --workspace judgment_request_respects_budget_and_records_provenance`; observe failure against the missing adapter.
- [ ] Add focused token, cost and deadline refusal cases, invalid schema output, secret redaction, lost response/single dispatch, and cross-operation evidence separation. Judgment cannot create human decisions or check receipts.
- [ ] Persist the intent before provider dispatch, enforce reservations and deadline, validate the output schema, retain usage/digests and atomically complete the operation/outbox event.
- [ ] Implement retained operation lookup/reconciliation under the original request identity, including after grant expiry where read authority permits it.
- [ ] Run the named test and `cargo test --locked -p aq-server --test playbook_judgments`; require every test to pass.
- [ ] Commit implementation and test evidence.

## Task 4: Integration and evidence

**Files:** `docs/implementation/D6.md` and this plan's checklist.

- [ ] Run targeted compatibility tests: `cargo test --locked -p aq-server --test playbook_host_adapters --test playbook_contract --test playbook_review_contract` and `cargo test --locked -p aq-playbook-host`.
- [ ] Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `git diff --check` within the configured resource limits. Record failures exactly.
- [ ] Reconcile every acceptance requirement with evidence in `docs/implementation/D6.md`; record environment, commands, final SHA and limits. D6 is not marked `review: adversarial`.
- [ ] Fetch and merge `origin/main` into this task branch. Choose a unique contiguous migration immediately before publication and rerun affected targeted tests if the merge changes relevant source.
- [ ] Prepare the required single feature commit, publish with guarded `aq git push`, record its full OID, settle task subtasks, and close only on actual acceptance evidence.

## Current disposition

Task 1 is resolved: the operator pinned AQ host catalog 2.0.0 (aq-playbooks `8343a1cd`, fingerprint `sha256:e2ce7dc8…b143db`). It adds asynchronous `judgment.request.v2`/`check.request.v2`, `operation.get` reconciliation, and the `aq.judgment.completed`/`aq.check.receipted` events.

Tasks 2–4 were executed with these changes from the proposed local API:
- Both adapters live in one module (`host_adapters/requests.rs`) on catalog profile Host7, over a single `playbook_host_operations` table (migration 0071; renumbered after D8 took 0070 and profile 6).
- Tests live in `tests/playbook_host_requests.rs`, rather than separate `playbooks/{checks,judgments}.rs` owners and test files.
- The qualified executor and provider are injected ports (`CheckExecutor`, `JudgmentProvider`). Binding them in production is a follow-up, not part of D6.
- `Stale` maps to `refused` with disposition `stale`. `Cancelled`, `TimedOut` and `Unknown` map to `infrastructure_failure`, keeping the distinct disposition.

Evidence and remaining follow-ups: [D6.md](../../implementation/D6.md).
