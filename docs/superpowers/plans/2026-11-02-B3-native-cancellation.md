# B3 Native Cancellation Implementation Plan

> **For agentic workers:** Execute with superpowers:executing-plans in the assigned AQ worktree. Steps use checkbox syntax. The task explicitly authorizes implementation in this session.

**Goal:** Durably cancel native attempts, deliver cancellation promptly, and preserve ordered result history without inventing physical stop.

**Architecture:** A single `profiled_execution::cancellation::cancel` domain function owns cancellation under node → attempt/hot locks shared with grants, results and settlement. Immutable operation receipts enforce replay. The existing NeverGranted owner gains an explicit cancellation mode, preserving its original authority/effect checks. Poll delivers retained directives even at zero free slots. Renewal emits the existing Cancelled failure. Stopped owners record cancellation settlement only after actual proof.

**Tech Stack:** Rust, Axum, sqlx/PostgreSQL, existing aq-admission contracts.

**Spec:** B3 in `2026-11-02-v2-full-application-tasks.yaml`; program Global Constraints in `2026-11-02-v2-full-application-plan.md`.

## Global Constraints

- Rust uses the committed lockfile with `--locked`; no sibling dependencies or wire changes.
- aq-graph owns task state; AQ owns grants, attempts and dispositions.
- Workers initiate every connection; no secrets in retained request/reason evidence.
- Stopped-writer proof precedes physical resource release/replacement; cancellation alone is not stop proof.
- Unsupported capabilities refuse explicitly. Do not edit architecture/spec documents.
- Evidence: `docs/implementation/B3.md`, exact commands, environment, commit and limits.
- Run fmt, clippy and named/related Rust tests. Respect AQ foreground resource controls; full CI remains integration-owned per worker instructions.
- Push assigned branch with guarded AQ publication; merge the component default before final submission (origin/dev is absent; origin/main is the fetched default).

## Review Focus

1. Cancellation while every slot is occupied must still reach the matching boot.
2. Changed replay payload or attempt must conflict, including across nodes.
3. Cancellation and result acceptance must serialize; an accepted evaluation must survive later cancel.
4. Ungranted preparing attempts must retain typed preparation verification and absence-of-effects checks.
5. Stop acknowledgement and cancellation settlement must preserve source/effect claims and late result history.

## File Structure

- Create `crates/aq-server/src/profiled_execution/cancellation.rs`: domain request receipts, directives, stopped settlement.
- Create `crates/aq-server/migrations/0043_native_cancellation.sql`: immutable cancel/replay/settlement records and cancellation-enabled NeverGranted owner.
- Modify `profiled_execution/no_grant.rs`: expose locked closure reuse, decode cancelled proof.
- Modify `profiled_execution/producer/workflow.rs`: cancellation delivery before capacity/scheduling.
- Modify `execution/api.rs`: native dispatch through domain function; preserve legacy response.
- Modify renewal/control error handling: typed Cancelled response after identity/binding checks.
- Modify native stopped owners: settle cancellation using their verified stop receipt.
- Register module/migration and state-format version in `profiled_execution.rs`, `store.rs`, `lib.rs`, `Dockerfile`.
- Create cancellation tests; extend `tests/profiled_results/cancellation.rs` to reuse the existing retained-result fixture.

### Task 1: Cancellation, replay and NeverGranted release

**Interfaces:** Consume AppState and native authority rows. Produce `cancel(state: &AppState, attempt_id: Uuid, operation_id: Uuid, reason: &str) -> Result<Receipt, AppError>` and locked no-grant closure reuse. Receipt distinguishes requested, never_granted and already_accepted.

- [x] Write `native_cancel_before_grant_releases_claim`: offered/preparing cases, nonexpired closure, no physical stop, claim released, same-payload replay, changed-payload conflict, preserved history.
- [x] Run through foreground AQ resource gate; expect failure because native cancellation is missing.
- [x] Add migration and domain transaction, preserving NeverGranted validation and immutable accepted evaluations.
- [x] Run focused test; expect named test passes with no skipped/zero matches. Commit implementation.

### Task 2: Delivery and proof-gated cancelled settlement

**Interfaces:** Produce `pending_locked` directives for matching worker identity and `settle_stopped_locked` using the actual stopped-owner receipt. Consume existing Cancellation and FailureCode::Cancelled wire types.

- [x] Write `native_cancel_after_start_stops_and_settles_cancelled` using fake Started/stopped worker evidence; prove no settlement before stop, directive binding/revision, zero-capacity poll delivery, renewal refusal, retained task/source claims after stop.
- [x] Run and observe missing behavior.
- [x] Wire poll, owner HTTP dispatch, renewal failure and stopped owners; preserve node/binding authorization before disclosing cancellation.
- [x] Run named test and owner auth/replay coverage; expect pass. Commit.

### Task 3: Late results and ordered acceptance

**Interfaces:** Reuse result record/load and settlement eligibility owners. Cancellation and evaluation use the same SQL node/attempt locks; cancellation cannot erase an accepted evaluation.

- [x] Write `late_result_after_cancel_is_recorded_not_applied`: cancellation before retained stopped result, result remains readable, eligibility forbids success; old released attempt cannot settle current task. Cover accepted-first cancellation outcome and concurrent lock ordering.
- [x] Reuse the initial cancellation RED; confirm cancel-first and accepted-first behavior through existing result/applicability owners.
- [x] Implement only missing ordering fences; leave automatic result settlement to D3.
- [x] Run named tests plus related no-grant, native poll, renewal, stopped and result tests. Record passes and broader timing/setup failures in the evidence. Commit.

### Task 4: Verification and publication

- [x] Run `cargo fmt --all --check`, `cargo clippy --locked --all-targets -- -D warnings` and exact three named workspace commands within available resource controls; record unavailable checks truthfully.
- [x] Record evidence in `docs/implementation/B3.md`; perform a fresh whole-branch review under executing-plans. B3 is not marked cross-family adversarial.
- [x] Resolve review findings with focused RED/GREEN tests. Squash own unpublished feature commits to one, preserving tree and attribution.
- [x] Merge fetched origin/main (component default; origin/dev is absent), rerun targeted schema/acceptance checks, guarded push, record full SHA, complete AQ subtasks and close with evidence in the AQ receipt.
