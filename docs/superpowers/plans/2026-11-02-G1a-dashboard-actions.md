# G1a Dashboard Actions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Mount a finite, durable human action boundary and principal-private operation lookup, with common errors and shared authenticated cursors.

**Architecture:** A build-time typed action registry validates strict payloads and authorizes each submission before the SQL journal admits an immutable principal/scope/request binding. Only the admitting request dispatches; interrupted or ambiguous effects remain recoverable by lookup and never redispatch on retry. Router-local cursor keys authenticate a shared bounded token format; existing consumers retain their authority checks.

**Tech Stack:** Rust, Axum, SQLx/PostgreSQL, Serde/Schemars, SHA-256, ChaCha20Poly1305.

**Spec:** Task steady-cascade-45.1 (G1a), program `2026-11-02-v2-full-application-plan.md` Global Constraints, dashboard API spec sections 2 and 7.

## Global Constraints

- Rust uses the committed lockfile with `--locked`; no sibling dependencies.
- No second writable task lifecycle; this journal owns admission/dispositions only.
- No production action kinds; `dashboard.actions` remains false.
- Retain receipts at least 90 days and every unresolved operation; retired IDs never replay.
- No secrets in errors, logs or evidence; current human authentication and CSRF remain required.
- No edits to architecture/spec documents; record proposed deltas in evidence.
- Required commands: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`; focused tests and related area suites under AQ resource control (prime limits full-suite runs to CI).
- Evidence in `docs/implementation/G1a.md`; guarded publication and final merge from origin/main (supervisor ruling replaces nonexistent origin/dev).
- Named tests must exist and execute; zero matching tests never passes.

## Review Focus

1. Concurrent identical submissions and interrupted dispatch must never execute twice.
2. Revoked authority or a different principal must not obtain retained private operations or effects.
3. Admission storage failure, dispatch timeout and lost completion write must not invent a definite rejection.
4. Unknown fields, oversized bodies and outer middleware refusals must use bounded v1 errors.
5. Expired/tampered cursors, changed filters/sort/authorization and retired IDs must fail closed.

### Task 1: Durable action boundary and published contract

**Files:** Create `crates/aq-server/src/dashboard_api/actions/{mod,contract,registry,journal,tests}.rs`, `crates/aq-server/migrations/0042_dashboard_operations.sql`, `crates/aq-server/tests/dashboard_actions.rs`, `crates/aq-server/examples/dashboard_actions_contract.rs`, `protocol/dashboard-actions/v1/{schema.json,README.md}`. Modify dashboard routes, store migration inventory, lib state versions and Dockerfile state default.

**Interfaces:** `ActionRequest` carries operation UUID, dotted kind, EntityRef subject, expected revision map, optional reason and typed payload. `ActionKind` exposes associated Payload (DeserializeOwned + JsonSchema), `authorize(state,user,request,payload)->Scope`, `execute(state,user,request,payload)->Outcome`; registry registers trusted Rust implementations only. Journal admits operation ID/principal/scope/digest before execution and looks up by principal. `Operation` carries five dispositions, digest, timestamps, subject/receipt/affected refs and typed recovery.

- [x] Write `action_union_rejects_unknown_kind_with_envelope`: actual production router returns 422 with safe envelope; no journal row.
- [x] Run it under an AQ-admitted Cargo wrapper; expect missing route assertion failure.
- [x] Write `unknown_effect_operation_is_recoverable_by_lookup` plus concurrent replay, changed bytes/scope/principal, revoked role, unknown fields, terminal retention/tombstones and interrupted dispatch tests using a test-only adapter and actual PostgreSQL.
- [x] Implement typed registry, strict validation, canonical JSON digest (recursive key ordering, compact UTF-8), admission with unique operation ID and one dispatcher, finite outcomes, lookup, 90-day retention and immutable tombstones. No automatic redispatch.
- [x] Publish generated schema and README, keep production kind union empty and capability disabled; document registration obligations.
- [x] Run named tests and action suite; expect each named test passes with nonzero test count. Commit.

### Task 2: Shared v1 errors and cursor codec

**Files:** Create `crates/aq-server/src/dashboard_api/{errors,cursor}.rs`. Modify `dashboard_api.rs`, `dashboard_api/runtime.rs`, `agent_access/human/cursor.rs`, bootstrap capability contract/service, lib outer human middleware and dashboard read tests/schema.

**Interfaces:** `ApiError` serializes the shared bounded error envelope. Outer `normalize_errors` maps framework/CSRF/admission errors only under `/api/v1/`. `CursorCodec::encode(binding,snapshot,position,issued,expires)` and `decode(token,binding,now)` authenticate principal, partition, scope, filters, sort, limit and snapshot; `CursorError` distinguishes authorization change and restart.

- [x] Add error tests for all required statuses, malformed body/path, CSRF, size and neutral lookup. Add cursor tests for every bound field, expiry, clock rollback, tampering and server restart.
- [x] Run tests and observe missing behavior before implementation.
- [x] Implement common bounded errors and one codec; adapt both current consumers. Map v1 cursor restart to 410; preserve legacy graph route status behavior.
- [x] Add operation retention metadata to disabled dashboard.actions capability. Regenerate dashboard read schema and verify contract drift.
- [x] Run action, cursor, dashboard read and existing human graph area suites, format and clippy; expect pass. Commit.

### Task 3: Review, evidence and publication

**Files:** `docs/implementation/G1a.md` and this plan's progress ledger.

- [x] Inspect acceptance coverage and record exact commands, environment, HEAD, limits and schema changes.
- [x] Fresh whole-branch review per executing-plans; fix material findings with focused regressions. G1a has no cross-family adversarial gate.
- [x] Squash unpublished leaf implementation to one feature commit preserving attribution and tree.
- [x] Merge supervisor-authorized origin/main without conflicts and run targeted checks; final publication SHA is recorded on the AQ task.
- [ ] Settle AQ subtasks and close with changes, verification and named-test evidence, following next-claim disposition.
