# Existing dashboard actions implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish three existing human writes through dashboard-actions-1, with shared legacy operation identity, native-journal reconciliation and bootstrap recovery discovery.

**Architecture:** Trusted adapters delegate to the existing human Graph operation owner, exact review owner and global mailbox owner. The action journal retains admission metadata, never a second copy of domain intent. Legacy HTTP handlers use the same admission path while preserving their native response envelopes. Lookup reconciles evidence without dispatching effects.

**Tech Stack:** Rust, Axum, SQLx/PostgreSQL, Serde/Schemars, Python/jsonschema.

**Spec:** Task steady-cascade-45.2 (G1b), `2026-11-02-v2-full-application-tasks.yaml` G1b; program plan Global Constraints; dashboard API sections 5, 7, 8.

## Global Constraints

- Rust uses the committed lockfile with `--locked`; no sibling dependencies.
- aq-graph owns task state; AQ owns grants, attempts and dispositions.
- No secrets in manifests, logs, fixtures or descriptions.
- Valid, invalid, replay and compatibility fixtures are server-owned; dashboard pinning belongs to G3/G8.
- The read envelope remains `dashboard-read-1`.
- Keep task IDs equal to operation IDs; unknown effects never authorize a new ID or automatic dispatch.
- Bounds: title 512 characters, body 32768 bytes, acceptance 8192 bytes, 16 labels; supervisor body 16384 UTF-8 bytes.
- Use AQ-admitted pytest subprocess adapters for Cargo (two build jobs/two test threads), as G1a did. Run focused and affected-area suites; CI owns the full suite.
- Check fmt, locked Clippy and locked targeted tests; evidence in `docs/implementation/G1b.md`; publish only the assigned branch.
- Do not edit architecture/spec documents. G1b has no adversarial gate.

## Review Focus

1. Legacy/union retries and concurrent submissions retain one operation and effect.
2. Timeout after domain commit is repaired by lookup, without dispatch.
3. Revoked owner or other principal cannot see operation refs or pending IDs.
4. Decision and subject pins cannot resolve a different revision; supervisor confirmation means enqueue only.
5. More unresolved rows than the bootstrap bound reports partial coverage, never silently complete.

### Task 1: Shared adapters and recovery

**Files:** `crates/aq-server/src/dashboard_api/actions/{mod,registry,contract,journal}.rs`; new `actions/existing.rs` and focused adapter modules as needed; `agent_access/human{,/commands}.rs`; `agent_access/messaging/global{,/writes}.rs`; `reviews.rs`; `lib.rs`; `tests/dashboard_actions.rs` and existing native-owner tests.

**Interfaces:** Consume `ActionKind::{authorize,authorize_lookup,execute}`, `Registry::register`, native create/enqueue/resolve services and retained receipts. Produce a production registry with exactly `task.create`, `decision.resolve`, `supervisor.message.send`; add default read-only `ActionKind::reconcile(state,user,operation) -> Result<Option<Outcome>,ApiError>`; legacy builders create the same `ActionRequest` as union callers.

- [x] Add failing mounted-route tests for supervisor enqueue/replay/changed payload/legacy interoperability, validation and principal isolation; run through AQ and observe missing-kind failures.
- [x] Extract bounded native service entrypoints without changing effects. Implement strict adapters, common legacy admission and read-only reconciliation. Task requests preserve `human-task-create-1`; decision payload binds the exact subject artifact as well as decision revision; message payload preserves message ID and empty unsupported context/attachments.
- [x] Add focused task/review adapter tests using existing domain fixtures, including pending recovery and exact version checks. Verify existing action-journal and native owner suites.
- [x] Commit adapters and tests.

### Task 2: Bootstrap and published contracts

**Files:** `dashboard_api/{service,contract}.rs`, router mounting metadata, `tests/dashboard_actions.rs`, action/read schema examples and generated schemas, `protocol/dashboard-actions/v1/{README.md,check.py,fixtures/*}`, read bootstrap fixture if regeneration is required.

**Interfaces:** Consume the mounted production registry and native/union pending journals; produce `Envelope<Bootstrap>` with at most 100 principal-private pending IDs and explicit partial coverage on truncation; `ContractRef` gets a distinct version enum admitting `dashboard-actions-1`, leaving `ApiVersion` read-only.

- [x] Write `bootstrap_lists_pending_operations` proving pending/unknown inclusion, terminal/foreign exclusion, bounded ordering, truthful owner capability and both contract digests. Observe red.
- [x] Implement bounded query and contract metadata; recheck current owner authorization before delivery.
- [x] Publish one valid request per kind, all five dispositions, ten status errors, identical/changed replay, expired cursor, invalid requests and read-version compatibility. Validator checks schemas and semantic fixture relationships.
- [x] Generate schemas, run drift checks and fixture validators; run named workspace test with explicit target selectors and affected dashboard/native-owner suites. Commit.

### Task 3: Evidence and publication

**Files:** `docs/implementation/G1b.md`, this plan and per-plan scratch ledger.

- [x] Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and locked affected-area tests through resource controls; record exact commands/environment/results and limits.
- [x] Review the complete branch using a fresh reviewer as required by executing-plans, resolve material findings with regression tests, and record rulings.
- [x] Reconcile every G1b requirement, write evidence, settle AQ subtasks, squash unpublished implementation WIP to one feature commit, retaining the integration merge and post-merge correction as separate commits.
- [x] Merge the authorized integration source, run targeted tests for touched files, and prepare guarded publication and closure with changes, verification and exact test evidence. Supervisor message `msg-db1c96b0687d4faab64f16fce1499c6d` authorized `origin/main` because `origin/dev` is absent. AQ records the delivered HEAD and final publication/closure outcome.
