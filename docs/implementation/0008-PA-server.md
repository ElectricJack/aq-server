# Spec 0008 phase A: live-only history caps (aq-server)

Plan: `docs/superpowers/plans/2026-11-08-sealed-history-p-a-live-caps.md`,
aq-server tasks 1-9, branch `swarm/p1-live-caps`. The dashboard re-pin (D1) is in
the aq-dashboard repo.

## What was built

- **Task 1.** The pure fair-share ordering moved from aq-server into
  `aq-profiled-state::fairness`, which paid for the lines this phase adds to the
  aq-server lib.
- **Task 2.** Migration 0103: `sealed_attempt_live`, `sealed_history_usage`,
  `sealed_staging_live_bytes`, `sealed_history_limits` and `sealed_history_holds`,
  plus the indexes they need. These are the only definition of a live row.
- **Task 3.** Migration 0104: the five quota triggers (admission artifacts,
  source exports, launch registrations, phase terminal capacity, presence
  operations) count live rows through `sealed_history_usage`. Presence also gets
  an hourly report rate cap.
- **Task 4.** The `aq-history-caps` crate: Rust mirror of the usage queries (with
  the all-rows fallback before 0103), the hold type, and its message copy.
- **Task 5.** The Rust mirror checks (decisions, registrations, source exports,
  staging, preparation, v2 proposals) call `aq_history_caps`.
- **Task 6.** Migration 0105: a database fence on the offer rate.
- **Task 7.** The poll gate: a node or server hold ends the poll with no offers
  and no writes; a project hold skips that project.
- **Task 8.** `HoldCode::HistoryRateLimited` and `HistoryCapReached`, the swarm
  assembly of history holds on nodes and in `waiting.reasons`, the regenerated
  `dashboard-read` schema and README.

## Limits (from `sealed_history_limits()`)

| Kind | Scope | Max rows | Max bytes | Headroom rows | Headroom bytes |
|---|---|---|---|---|---|
| admission_artifacts | global | 4096 | 268435456 | 64 | 4194304 |
| admission_artifacts | project | 512 | 67108864 | 64 | 4194304 |
| intelligence_decisions | global | 1024 | 67108864 | 2 | 16777216 |
| launch_registrations | global | 4096 | 262144 | 1 | 64 |
| launch_registrations | node | 256 | 16384 | 1 | 64 |
| native_preparation | global | 4096 | 67108864 | 1 | 262144 |
| native_preparation | node | 256 | 4194304 | 1 | 262144 |
| offers | global | 960 | | 1 | 0 |
| offers | node | 30 | | 1 | 0 |
| phase_terminal | global | 256 | 268435456 | 1 | 1048576 |
| phase_terminal | node | 16 | 16777216 | 1 | 1048576 |
| presence_operations | global | 65536 | 268435456 | 1 | 0 |
| presence_operations | node | 4096 | 16777216 | 1 | 0 |
| presence_rate | global | 11520 | | 1 | 0 |
| presence_rate | node | 720 | | 1 | 0 |
| project_offers | project | 240 | | 1 | 0 |
| source_exports | global | 4096 | 268435456 | 1 | 4202496 |
| source_exports | project | 256 | 67108864 | 1 | 4202496 |
| v2_proposals | global | 4096 | | 1 | 0 |
| v2_proposals | node | 256 | | 1 | 0 |

## Tests

- `it_schema sealed_history_live::*` (11): the live definition, each usage kind,
  the limits table, the holds, the 0104 guards and the 0105 fence.
- `it_schema schema33_phase_terminal::settled_terminal_capacity_does_not_block_a_new_grant`
  and the updated `actual_grant_cannot_commit_without_reserved_terminal_capacity`.
- `it_profiled profiled_artifacts::orphaned_and_settled_artifacts_do_not_count_toward_the_project_cap`.
- `it_native native_offers::history_caps::*`: the held poll is a 200 with no
  offers and no history written; offers older than an hour do not count.
- `it_dashboard dashboard_swarm::swarm_shows_the_offer_rate_hold_on_the_node`.
- aq-swarm `history_holds_join_the_node_and_the_waiting_reasons`; aq-history-caps unit tests.

## Deviations from the plan

- Tests that open a database with `TestDb::create()` call `db.store()` first,
  because `pool()` alone gives an unmigrated database.
- `settled_terminal_capacity_does_not_block_a_new_grant` runs on the current
  store, not the frozen state-32 fixture. That fixture lacks tables the grant
  path reads today, so the grant would fail for reasons unrelated to capacity.
- `native_initial_grant::published::material_receipts::retained_installation_json_bytes_refuse_new_grant_under_shared_budget`
  seeds its padding leases as `pending` instead of `expired`. Settled leases no
  longer count by design; the test still checks that live retained bytes refuse a grant.
- The two new `HoldCode` variants carry `//` comments, not `///` doc comments,
  so the generated schema stays a single `enum`.
- `AGING_MS` and `STARVATION_MS` are not re-exported from `producer::fairness`
  (nothing in aq-server uses them).

## Deferred (spec 0008 section 5.2)

Registration history, presence heads, settlement evaluations, the review budget,
intelligence receipts, legacy closures, legacy renew receipts and delegated
operations are unchanged. They belong to phase B.

## Fix round 1

- **Live-set driven queries.** `sealed_live_attempt_ids` (a view over three partial
  indexes: held tasks, hot status not settled, legacy-fixture snapshot not
  settled) is read once per call into an array, and every kind joins from it
  through the history table's attempt key (`= ANY(live)`). A pending or recent
  window uses a btree on the table's time column. Nothing scans a history table.
  A released profiled attempt with no hot row is settled (it holds nothing and no
  owner advances it), which narrows `sealed_attempt_live` for that case.
- **One call per kind.** `sealed_history_scoped` returns a kind grouped by node
  or project; `sealed_history_holds(uuid[], uuid[], bigint)` computes each kind
  once for all listed nodes and projects. `swarm_holds` and the poll gate
  (`poll_gate`) make one call each. `swarm_holds` takes the enabled projects.
- **Fair share.** Held projects leave the candidate list before the per-poll
  project limit, so they are never charged a turn.
- **Pending window for exports.** `profiled_source_exports.created_at_ms` (set
  at reservation) keeps an unlinked export live for ten minutes, in the source
  export cap and in staging.
- **Fence race.** The offer-rate fence refusing an insert (SQLSTATE 54000) ends
  the publication with no offer, an empty 200 page.
- **Presence counters.** The 0029 lifetime counters now saturate at their CHECK
  bounds, so they no longer stop an always-on worker after 4096 reports.
- The settlement39 catalog ignores indexes named `sealed_*`.
- The fair-share store functions moved into `aq-profiled-state::fairness`
  (lib line budget).

## Verification

The full `cargo nextest run --workspace --locked --no-fail-fast` run had three
failures in tests this branch does not touch:
`playbook_activation::inspection_refuses_other_owner_viewer_and_absent_runtime`,
`playbook_activation::catalog::actual_empty_startup_preserves_installed_definitions_and_activations` and
`delegated_permit_runtime::server_restart_mid_attempt_resumes_or_preserves`. Each
passed on three reruns alone, so they are recorded as load flakes.
