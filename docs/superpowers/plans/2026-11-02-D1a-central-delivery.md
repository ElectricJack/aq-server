# D1a Central Delivery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build frozen candidates from retained source records and conditionally publish through central aq-git with durable, replay-safe evidence.

**Architecture:** A central-only Rust service takes receipt IDs, resolves their stored OIDs and retention handles, and freezes a typed protocol request in PostgreSQL before socket I/O. Immutable byte receipts preserve aq-git replies. Publication reserves a repository/target destination before dispatch; uncertainty retains the reservation and prevents another push, including after restart. Reads use separate durable observation identities.

**Tech Stack:** Rust, Tokio Unix sockets, serde, SQLx/PostgreSQL, pinned aq-git 0.2.0.

**Spec:** Task crisp-meadow-36.1 complete description; Global Constraints in `2026-11-02-v2-full-application-plan.md`; retained aq-git `docs/api.md` and `docs/aq-integration.md` candidate/publication paragraphs.

## Global Constraints

- `--locked`; no sibling dependencies or changes to aq-git pins.
- Protocol v1; 1 MiB requests, 8 MiB responses, at most 64 ordered sources, `git-ort-v1`.
- No worker routes, arbitrary operation forwarding, URLs or credentials in service inputs.
- Central aq-git binding authoritatively enforces allowed targets and binding generation.
- Persist intent before socket calls; immutable exact response bytes; changed inputs are `idempotency_conflict`.
- No automatic re-push after publication uncertainty; policy/reconciliation belongs to D1b.
- Evidence `docs/implementation/D1a.md`; cross-family review via supervisor before passing close; PR integration.

## Review Focus

1. Crash after committing publish intent but before saving receipt: destination remains held; same operation never redispatched.
2. Concurrent callers and payload changes: one publish socket call and deterministic idempotency conflict.
3. Malformed/oversize/disconnected replies: no invented success; publication remains blocked.
4. Unretained or cross-repository source IDs: rejected before socket; caller cannot supply source OIDs/handles.
5. Observed presence is not causal success; conflicted candidates remain unpublishable and preserve bounded path evidence.

### Task 1: Durable central delivery and typed protocol

**Files:** Create `crates/aq-server/src/delivery.rs`, `crates/aq-server/src/delivery/protocol.rs`, `crates/aq-server/tests/source_retention/delivery.rs`, `crates/aq-server/migrations/0045_central_delivery.sql`; modify `crates/aq-server/src/lib.rs`, `crates/aq-server/src/store.rs`, and Docker state format metadata.

**Interfaces:** `DeliveryService::new(PgPool, &SourceRetentionConfig)`; `create_candidate(CandidateRequest) -> Result<Receipt<CandidateResult>>`; `publish(PublishRequest) -> Result<Receipt<PublishResult>>`; `candidate_show(ObservationRequest) -> Result<Receipt<CandidateResult>>`; `operation_show(ObservationRequest) -> Result<Receipt<OperationResult>>`. Requests carry operation/repository UUID and binding generation; candidate inputs carry ordered source receipt UUIDs, base OID and commit metadata. Each Receipt contains exact response bytes and a typed success/error result. Transport failure returns an explicit error; publish uncertainty remains durable.

- [x] Write tests `candidate_from_retained_source_publishes_once`, `delivery_replay_conflict_and_unknown_publish_are_fenced`, `delivery_protocol_bounds_and_conflict_results`, `delivery_rejects_unretained_sources`. Use disposable PostgreSQL and socket peers to assert committed intents before calls and immutable receipts after calls. Prove restart and concurrent replay do not issue another publish.
- [x] Run focused tests through resource-controlled `aq test` wrapper around locked Cargo; verify missing service fails compilation.
- [x] Implement migration 0045 (intent, receipt, destination hold; immutability triggers), register state version 45.
- [x] Implement typed whitelist and bounded correlated transport in protocol.rs. Validate typed OIDs, full branch refs and response invariants, preserving all raw response bytes.
- [x] Implement source resolution and canonical immutable inputs; serialize per-operation dispatch using a database row lock after committing intent. Commit destination reservation with intent; never redispatch a publish whose reservation already exists without a receipt. Store response and release only conclusively resolved destinations atomically.
- [x] Run focused tests and migration tests; verify all named tests actually execute. Commit implementation.

### Task 2: Actual aq-git qualification and submission

**Files:** Extend `tests/source_retention/delivery.rs` with local bare-remote fixture using the retained aq-git executable and its qualified Git runtime; write `docs/implementation/D1a.md`.

**Interfaces:** Same public central service as Task 1; actual aq-git owns binding allowlist, candidate retention and conditional remote publication.

- [x] Extend acceptance test to real candidate creation/publication using retained source acknowledgement, local bare remote and `allow_file_transport:true`; assert remote revision, exact replay, changed-payload refusal and already-present noncausation.
- [x] Run `cargo test --locked --workspace candidate_from_retained_source_publishes_once` and related delivery tests via foreground resource control; `cargo fmt --check`, locked workspace/all-target Clippy as configured (the all-workspace formatting discovery limitation is recorded in evidence). Do not run unrelated full-suite checks contrary to worker validation scope.
- [x] Record commands, environment, results, SHA and limitations. Reconcile every acceptance item.
- [x] Squash unpublished task commits preserving tree/attribution, merge configured integration source `origin/main` (supervisor waiver `msg-c81e8d8d99de4a4b9d30f8a27565e549` confirms no `origin/dev`), and run affected tests.
- [ ] Publish the final evidence commit with guarded push and open the required PR; record the exact SHA outside its own commit.
- [ ] Obtain supervisor-dispatched cross-family adversarial review tied to pushed SHA; address findings and record dispositions. Close explicitly only when requirements are met.
