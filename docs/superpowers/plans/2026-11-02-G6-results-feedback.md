# Results feedback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give retained Results revision-bound comments, feedback tasks, waivers, feeds and explicit media availability.

**Architecture:** AQ owns supplemental feedback records; retained native reports remain the result authority and Graph remains task authority. G1 action adapters journal mutations, with transactional local effect receipts and a fixed-key task-create intent for cross-owner effects. Capture policy lives in append-only task-policy revisions, copied into immutable report snapshots.

**Tech Stack:** Rust, Axum, SQLx/PostgreSQL, schemars, existing G1 cursors and operation journal.

**Spec:** AQ task steady-cascade-45.7; dashboard 0003 §6; program plan Global Constraints.

## Global Constraints

- Rust uses the committed lockfile with `--locked`; no sibling dependencies.
- Publish wire schemas and valid, invalid, replay and compatibility fixtures.
- Graph owns task lifecycle; following, evidence presence and linking never imply acceptance.
- Explicit refusals: disabled media returns a typed 503.
- Do not edit architecture/spec documents, production Compose, UI or completion-policy wiring.
- AQ-admitted pytest adapters run Cargo with two build jobs and two test threads; focused/area tests, fmt and Clippy. CI owns unrelated full-suite execution.
- Evidence: `docs/implementation/G6.md`; publish assigned branch through guarded AQ push.

### Task 1: Explicit media refusal

**Files:** `crates/aq-server/src/result_media/{api,config}.rs`, `src/lib.rs`, `src/dashboard_api/service.rs`, `tests/profiled_results/results_feedback.rs`.

**Interfaces:** `result_media::api::unavailable_routes(configured: bool) -> Router<AppState>`; existing configured media routes remain authoritative.

- [x] Add `media_upload_disabled_without_store_is_explicit`: POST the actual protected admission path with no runtime, assert 503 and `media_store_unconfigured`; exercise disabled configured uploads and bootstrap reasons.
- [x] Run `cargo test --locked --workspace --test profiled_results media_upload_disabled_without_store_is_explicit`; observe 404 before implementing.
- [x] Mount refusal routes for absence and disabled writes; require current Owner and preserve outer CSRF/maintenance gates. Add comment/feedback bootstrap capabilities after owners mount.
- [x] Run named test and dashboard mount/read area tests; commit.

### Task 2: Revision feedback persistence and capture evaluation

**Files:** new `src/results_feedback/{mod,contract,store,actions,api}.rs`; new migration `0044_results_feedback.sql`; `src/{lib,store}.rs`; `src/profiled_execution/results.rs`; `tests/profiled_results{.rs,/results_feedback.rs}`.

**Interfaces:** `results_feedback::routes(&AppState) -> Router<AppState>`; action kinds `result.follow`, `result.comment`, `comment.revise`, `thread.resolve`, `feedback.task_create`, `feedback.link`, `result.evidence_waive`; typed payloads exported from `contract`.

- [x] Add mounted tests for comment creation/replay/changed identity, revision correction history, ordered replies, wrong revision/artifact rejection, explicit resolution with version and follow-up revision, Following and Needs review, capture snapshot/waiver policy binding and viewer refusals.
- [x] Run focused tests and observe missing routes/actions.
- [x] Add migration tables for policies/snapshots, threads/comments/history, follows, feedback intents/links, waivers and local effect receipts. SQL guards retain immutable policy/comment revisions and exact report pins.
- [x] Implement closed typed anchors with normalized image bounds and retained artifact digest/revision checks; body byte bounds 1–16384. Enforce expected versions within transactions, fence current principal, retain action outcomes atomically.
- [x] Project stable result identity from supersedes ancestry, immutable revision content and separate assessment version. Pin task capture policy when reports are inserted; backfill existing reports with explicit empty policy. Evaluate only retained finalized exact-revision artifacts or explicit authorized waivers; return acceptance `unreviewed` independently.
- [x] Add bounded feeds (default 20), exact detail and comments reads using authenticated G1 cursors bound to filters, principal and page size. Following combines explicit result follows and opted-in thread participants; Needs review checks unresolved threads/reviews/evidence.
- [x] Run feedback area tests and commit.

### Task 3: Fixed-key feedback task recovery

**Files:** `results_feedback/{actions,store}.rs`; narrow reusable entrypoints in `agent_access/human/commands/action.rs`; action registry/schema registration.

**Interfaces:** reuse `TaskCreate: ActionKind` under `operation_id` without admitting a second action-journal identity. Retain feedback draft before dispatch and reconcile native task status under that original identity.

- [x] Add `feedback_task_links_result_revision` using real report recording and the existing disposable Graph fixture; assert one created task, exact comment/result pins, replay, lost-link recovery and unresolved origin thread.
- [x] Observe failure; implement durable feedback intent before G1b execute, then transactional link after confirmed native task receipt. Reconciliation reads retained native status, adds the missing link under the same ID, never allocates a second Graph key.
- [x] Validate existing-task and parent refs through configured project Graph reads. Reject cross-project origins and unknown comment revisions before dispatch; exact retained historical revisions remain valid origins.
- [x] Run both named workspace tests with target selector, task-create and action area suites; commit.

### Task 4: Contracts, evidence and publication

**Files:** new `protocol/results-feedback/v1/{schema.json,fixtures,check.py,README.md}` and `crates/aq-server/examples/results_feedback_contract.rs`; generated G1 action schema; `docs/implementation/G6.md`.

- [x] Generate schema from Rust payloads/entities. Add valid, invalid, replay and compatibility fixtures and validate them with the existing contract validator.
- [x] Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, exact named tests and affected suites via AQ resource admission. Record actual results and test count.
- [x] Document production persistent private media root `/var/lib/aq/result-media`, stable installation storage UUID, configuration and volume handoff to E1a. Record PNG-only/machine-ingestion limits and any proposed spec follow-up.
- [x] Review requirements against implementation; reconcile the task checklist; squash own unpublished implementation commits preserving source base/tree/attribution.
- [ ] Merge the authorized integration source, resolve conflicts and run targeted tests.
- [ ] Guarded push; record final SHA and close with changes, verification and exact checks. No review is required by G6's task marker.

Implementation and test steps are complete. Intermediate implementation commits
are consolidated for the first publication as required by the worker profile.
The check step records execution, including the existing failures and follow-ups
in `docs/implementation/G6.md`; it does not claim all affected suites passed.
Final integration and close remain pending because `origin/dev` is absent and
AQ refused the decision messages. The attempted checkpoint push was refused with `out of scope: git_push`;
local committed work and the active claim are preserved.
