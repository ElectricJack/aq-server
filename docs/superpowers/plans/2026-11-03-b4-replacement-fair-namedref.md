# B4 — Replacement attempts, fair multi-project scheduling, moving-ref sources (TDD plan)

Task: `noble-summit-37.4`. Scope: the four named acceptance tests plus the wiring
they prove. Out of scope: C4 placement, G2 owner retry command, H2 cost feeding,
B3 cancellation, D3 automatic settlement.

## Acceptance tests (must exist and pass with `cargo test --locked --workspace <name>`)

| Test | Location | Proves |
|---|---|---|
| `failed_attempt_is_replaced_within_retry_budget` | `profiled_execution::replacement::tests` | A failed predecessor that has stop proof, retained source and quiescent effects yields `Replace` with fresh `ReplacementEvidence` for exactly that predecessor. The scheduler then picks the task (not starved, not blocked). |
| `retry_budget_exhaustion_blocks_task` | `profiled_execution::replacement::tests` | `attempts_started >= max_attempts` yields `Blocked("lineage_retry_budget_exhausted")`. The scheduler skips the task without starving later tasks. Uncertain stop or unreconciled effects also block. |
| `two_projects_share_capacity_fairly` | `profiled_execution::producer::fairness::tests` | With two always-ready projects and a priority gap, serving N polls while recording each service never starves the lower-priority project. Shares stay bounded and waiting gains priority. |
| `named_ref_resolves_to_exact_commit_at_offer` | `profiled_execution::producer::workflow::tests` | A `NamedRef` policy is admitted. The source operation key freezes the commit resolved at offer time, so a moved ref yields a new operation and the frozen base never follows the ref. |

## A. Replacement (`profiled_execution/replacement.rs`)

1. RED: a pure `assess(&[Prior], &Limits, now) -> Assessment` test for each reason.
2. GREEN, with `Assessment::{Fresh, Replace(Replacement), Blocked(&'static str)}`:
   - Any prior with stop `Unknown`/`Running` → `replacement_prior_execution_uncertain`.
   - Effects not `Quiescent`, a completion that is prepared/possible/conflict/blocked, or a stale/needs_reconciliation evaluation → `replacement_effects_unreconciled`.
   - Retention not `Retained` → `replacement_source_retention_unknown`.
   - Accepted evaluation → `task_already_settled_accepted`.
   - Predecessor not proven failed (no cancelled settlement, no rejected evaluation, not Finished/Declined, deadline not passed) → `replacement_predecessor_not_failed`.
   - `attempts_started >= max_attempts` → `lineage_retry_budget_exhausted`.
   - Elapsed since first offer `>= max_lineage_elapsed_ms` → `lineage_time_budget_exhausted`.
   - Conservative sandbox charge Σ(deadline − offer) `>= max_sandbox_ms` → `task_sandbox_minutes_budget_exhausted`.
3. DB loader `load(pool, task)` reads `execution_attempts`, `profiled_authority_hot`, `profiled_launches`, settlements, evaluations and completions for the task's non-never-granted attempts.
4. `release_locked`: under the node lock then `FOR UPDATE` of the attempt and hot rows, re-load and re-assess. Insert an immutable `profiled_replacement_releases` row, then set `holds_task=false`.
5. Migration 0050 (delivered as 0052 after main took 50/51):
   - Adds the release table with an insert trigger that re-checks hot stop/retention/effects and the absence of an accepted evaluation.
   - Extends `profiled_historical_snapshot_guard` and `settlement_claim_guard` to accept a replacement release.
   - Adds `profiled_project_schedule`.
   - Bumps the state format to 50.
6. The resolver takes an optional `ReplacementBudget` (predecessor, elapsed, evidence) on `ResolveRequest`. It fills `last_attempt_id`, `elapsed_ms` and `replacement`, and refuses when `attempts_started` does not match the counts.

## B. Fairness (`producer/fairness.rs`)

1. RED: test the pure `order(projects, served, now)`.
2. GREEN:
   - Effective priority = configured priority + seconds waited since last service (each project starts waited from `now − STARVATION_MS` when it has never been served).
   - Any project waiting longer than `STARVATION_MS` goes first, oldest first.
   - Ties break on oldest service, then project id.
3. DB: `served(pool)` reads `profiled_project_schedule`; `record(pool, project, now)` upserts it.
4. `offers()` tries up to 4 projects in fair order. It records service for an offer and also for a project found to have no eligible work.

## C. NamedRef freeze

1. RED: test the `source_operation(...)` key. Immutable keeps the historical 5-tuple; NamedRef adds the resolved commit; two commits give two operations.
2. GREEN:
   - Remove the Immutable-only refusals.
   - Observe the source once before deriving the operation and pass that observation to `reservation`.
   - The second observation still refuses if the ref moved (conservative: no follow). A later poll freezes the new commit under a new operation.

## Limits and follow-ups (recorded in `docs/implementation/B4.md`)

- Budget limits are server constants until per-project config exists; the selection policy's own `max_attempts` and `max_lineage_elapsed_ms` still fence.
- Successor-from-predecessor retained source is not selected by any policy yet; a follow-up task covers it.
- Exhaustion is a scheduling block, not a Graph-visible task state; owner retry is G2.
- Task-level priority within a project is a follow-up.
