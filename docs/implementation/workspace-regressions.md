# Persistent workspace regressions (fair-zenith-27)

All 19 requested regressions passed both serially and within the full workspace
run. The wider workspace command remains red; its unrelated failures are
recorded below. Final fmt and all-target Clippy with warnings denied also passed.

The 19 cases below reproduced with identical panics on the A3b branch and clean
main. The paired before evidence is
[A3b comparison](A3b-checks/persistent-failure-comparison.json), with run provenance
in [baseline comparison](A3b-checks/baseline-comparison.md). This change starts from
fetched `origin/main` at `a860d3e03a6c114eaad5f32e5726f46f46fddcaf`, then synced
again through `c802fed5` (workspace validation merge `35ccd683`), then fetched
`59e6b566` (final named-validation merge `28a06502`); no dev merge. Implementation commit
`184894a4` contains the repairs and reproduction harness; `13f0fa64` bounds
build storage after the first full workspace attempt approached the disk limit.
Timing cases belong to `smart-delta-18` and are outside this repair.

## Causes and repair

* Schema 32 fixtures use real registered migrations and current Rust owner code.
  Poll newly queried cancellation tables introduced in 43, before these tests
  reached their grant/terminal assertions. Explicit historical Store construction
  now records its profile. Only test-support historical instances omit the later
  cancellation delivery step; current startup and corruption guards stay strict.
* The hot loader test's synthetic verified-release row predates qualified
  settlement ownership introduced in 39. Its original storage/compatibility
  assertions run on an actual prefix 27, with all SQL guards of that prefix.
  Current settlement owner tests still exercise qualified release and refusal.
* Deliberate missing-table tests now remove dependent foreign keys with CASCADE.
  They still require startup refusal and verify that no repair occurred. The
  mandatory-release-trigger corruption uses the current qualified guard which
  replaced the old unqualified trigger in 39.
* Migration 43 tests upgrade through current Store (format 44 for the workspace
  run, 45 after the final main sync). Their
  old-reader refusals now require the actual current format and the exact old
  supported format, rather than assuming current remains 43 forever.
* The original accepted-phase expiry test asserts the pre-40 held/refused
  behavior. It now constructs actual prefix 39 and invokes that historical SQL
  expiry owner, rather than failing on an absent newer function. Phase-aware
  absence release was added in 40. A current-schema frozen-preparation
  counterpart shares all input-scoping assertions and verifies valid absence
  proof, release, zero grants, and byte-identical replay; current refresh release
  and grant-issued refusal remain covered by `phase_never_granted`. Both expiry
  cases use the existing local functional clock after genuine offer publication.

No test is deleted, ignored, or given a weaker authority assertion. Production
constructors never infer a historical profile from missing SQL objects. The
historical dispatch is compiled only with `test-support` and follows constructor
provenance through cloned Stores; normal Store startup still validates current
objects, even with that feature enabled.

The newer main base also left `dashboard_aggregation::quota_unknown_is_not_zero`
uncompilable: its Rust credential literals lacked `allowed_backends`, and the
outer configuration lacked `kind`. The fixture now spells out the same defaults
as omitted serialized configuration (API key, unrestricted backends). This is a
compile prerequisite for full workspace validation; its quota/privacy assertions
are unchanged. Main independently landed the same fix during validation; the
final merge keeps main's explicit `CredentialKind::ApiKey`, leaving no dashboard
diff relative to that main. The original compiler output is retained in `checks-1`.

## Historical timing boundary

After removing the historical Poll 500, serial runs exposed admission expiry
in these historical consumers. Under supervisor authorization (AQ message
`msg-dc6b1a5e913f46c6ba58b18b4b5e9933`), only the fixture modules in
`phase_terminal_closure.rs` and `schema33_phase_terminal.rs` were experimentally
routed through existing `common/phase_frozen_dispatch.rs`. The controlled run
(`exact-3`) exposed that phase Prepare ready-read
stamps still use wall time while context stamps use `state.clock`; that shared
Graph clock injection belongs to `smart-delta-18.1` and is required for this
controlled boundary to qualify actual issuance. Both imports were reverted.
The final change retains the original real-clock fixtures, with no snapshot
restamping, production deadline changes, or weakened authority assertions.
`common/native_offer.rs` and the wider timing fixtures are untouched.
`smart-delta-18` and `smart-delta-18.1` retain ownership of that wider class.
The subsequent real-clock run on task-specific PostgreSQL (`exact-4`) passed
17/19. Its two early failures were `selection expired during admission locks`
in the historical accepted-preparation case and `accepted preparation v2 is not
currently available` in the suppressed terminal-closure case. The former now
uses its existing local functional setup; the latter retains its original
fixture pending the separate Graph clock work.
Supervisor messages `msg-63b221ffdb574689a858bc3db86bf5c2` and
`msg-98088b7e27ed43e989aabd2de1a6c94e` explicitly direct
completion without waiting for the separate `smart-delta-18.1` work,
with any residual phase-32 issuance timing cases named as exceptions in the
close summary. These exceptions do not make a failed command pass; all command
statuses and per-case outcomes remain in the evidence.

The eleven historical timing fixtures are the four `phase_terminal_closure`
cases and these seven `schema33_phase_terminal` cases: `actual32_history_survives33_and_reserves_cleanup_without_terminal_effects`,
`actual_grant_cannot_commit_without_reserved_terminal_capacity`,
`already_open_writer_cannot_insert_legacy_started_for_actual_phase_grant`,
`collision_rolls_back33_without_changing_actual32_history`,
`old_snapshot_classifies_actual030_origin_even_without_visible032_grant`,
`ordered::actual32_open_owner_history_survives_registered33_upgrade`, and
`relational_only_over_budget_backfill_rolls_back_entire_migration`. Their full
names and final observed results appear in the table below. All eleven passed
both final runs; their retained dependence on wall time remains an approved
`smart-delta-18.1` follow-up. Actual real-clock
failures concern expiring admission/issuance; the ready/context clock mismatch
explains why switching these fixtures to the controlled boundary is blocked,
not the cause of every real-clock failure.

## Final workspace result

The complete workspace command on `35ccd683` exited **101** after 3,415 seconds:
**1,385 passed, 74 failed, 30 ignored**. All **19 requested cases passed**,
as did the new current-schema absence-proof counterpart and
`profiled_results::results_feedback::feedback_task_links_result_revision` with
the pinned actual Graph binary. The full workspace command is not green.
See [workspace summary](workspace-regressions-checks/workspace-summary.json)
and [complete output](workspace-regressions-checks/workspace-2/00-workspace.txt).

Of the other failures, 23 explicitly assert PostgreSQL port 55432 while this
run uses task-specific port 20533: 16 in `playbook_activation`, five in
`playbook_bridge`, and two protected-engine cases in `profiled_results`.
Those fixtures also forward their Unix socket to the hard-coded shared endpoint.
Their unchanged assertions and exact names are retained in
[endpoint failures](workspace-regressions-checks/playbook-endpoint-failures.json).
The remaining 51 report grant/phase freshness or availability failures and
cancellation polling assertions; their exact names are retained in
[outside-scope failures](workspace-regressions-checks/outside-scope-failures.json).
This repair does not claim to establish the cause of every outside-scope failure.
For those 74 failures, **baseline not rerun** against clean main `c802fed5`;
they are not claimed identical to main or newly introduced by this branch.
Supervisor message `msg-30c89193c529412d8c6a6080a63b6fc3` accepts the requested
19 passing cases as task acceptance, classifies the other failures as 23
environment fixtures plus 51 load-sensitive timing cases owned by
`smart-delta-18.1`, and explicitly directs no individual or full-suite reruns.

The final fetched-main sync added schema 45 central delivery and dashboard
packaging changes. Named serial qualification and fmt/Clippy use `28a06502`;
the full-workspace result above remains attributed to `35ccd683`. The owner
velocity instruction added to `.aq/PRIME.md` during that sync requires stopping
at task acceptance and targeted verification. Accordingly this task does not
repeat the full workspace or expand into the 74 unrelated failures.

## Reproduction and evidence

`scripts/test-workspace-regressions.py` runs Cargo through an `aq test` resource
slot. It selects all 19 cases from the immutable before manifest, checks that
each named case actually appears and passes, and saves commands, return codes,
durations, source/diff identity, and complete output. `exact` runs each case
serially; `area` also runs current cancellation/expiry neighbors serially;
`workspace` runs the whole workspace with two test threads; `checks` runs fmt
and Clippy. Each run requires a fresh evidence directory. Exploratory shared-PostgreSQL
runs are retained as `exact-1`, `exact-2`, and `area-1`; `exact-3` records the
reverted controlled-import experiment. Subsequent runs use the
task-specific disposable PostgreSQL described in
[environment.json](workspace-regressions-checks/environment.json).

`workspace-1` was interrupted with SIGINT before tests started: shared free space
fell below 9 GiB with only 59/108 server test binaries built. Its log and return
code are retained. Cargo cleaned only this worktree's build artifacts. Subsequent
runs disable debug symbols (`CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`) and incremental caching (`CARGO_INCREMENTAL=0`),
with unchanged default optimization, features, test selection and assertions.
The harness records these controls; two Cargo jobs remain the limit.

Use the disposable database exported by `scripts/test-db`, `CARGO_BUILD_JOBS=2`,
and the [pinned Graph fixture](A3b-checks/graph-test-binary.json). Graph SHA-256:
`da79578f79181b8f9c4e2ecb393a04d441a6eb5fcd952c976bef436199c93f21`.
Set `AQ_TEST_GRAPH_BINARY` to that extracted binary, `AQ_TEST_GRAPH_SHA256` to
this hash, and `AQ_WORKSPACE_REGRESSION_EVIDENCE` to a fresh evidence directory.
Then run:

```sh
AQ_WORKSPACE_REGRESSION_SCOPE=exact aq test scripts/test-workspace-regressions.py --timeout=3600
AQ_WORKSPACE_REGRESSION_SCOPE=area aq test scripts/test-workspace-regressions.py --timeout=3600
AQ_WORKSPACE_REGRESSION_SCOPE=workspace aq test scripts/test-workspace-regressions.py --timeout=3600
AQ_WORKSPACE_REGRESSION_SCOPE=checks aq test scripts/test-workspace-regressions.py --timeout=3600
```

Final [fmt and Clippy](workspace-regressions-checks/checks-3/results.json) both
passed on `28a06502`: `cargo fmt --check` and
`cargo clippy --locked --all-targets -- -D warnings`.

## Before/after

The final [serial run](workspace-regressions-checks/exact-5/results.json) on
`28a06502` passed 19/19 with every exact case actually observed (including
the historical expiry refusal). The full [workspace run](workspace-regressions-checks/workspace-2/results.json)
on `35ccd683` passed the same 19/19. Earlier failures, the reverted experiment,
and the interrupted build remain indexed in [run-index.json](workspace-regressions-checks/run-index.json);
early dirty snapshots retain their diff and harness hashes. No prior run is
relabelled as passing.

| Case | Before | Cause | After serial | After workspace |
| --- | --- | --- | --- | --- |
| `native_accepted_preparation::original_inputs_remain_scoped_after_proposal_expiry_then_expire_held` | Failed, identical on branch/main | Pre-40 phase expiry refusal | Pass | Pass |
| `phase_execution_transport::genuine_old_schema_cannot_issue_production_grant_without_reserved_cleanup_capacity` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `phase_terminal_closure::failed_closure_insert_rolls_back_permission_revocation_and_retains_retry_identity` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `phase_terminal_closure::pre_started_host_stop_can_omit_pane_and_root_status_without_inventing_started` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `phase_terminal_closure::suppressed_phase_closure_after_expiry_and_retirement_is_historical_and_held` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `phase_terminal_closure::wrong_origin_checkpoint_or_current_credential_cannot_change_phase_authority` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `profiled_hot::actual_common_loader_reads_hot_renewal_revocation_and_compatibility_fallback` | Failed, identical on branch/main | Pre-settlement hot loader fixture | Pass | Pass |
| `schema30_preparation::current30_missing_one_or_both_tables_refuses_reopen_without_repair` | Failed, identical on branch/main | Corruption setup blocked by later foreign keys | Pass | Pass |
| `schema32_phase_execution::current32_missing_execution_history_refuses_reopen_without_repair` | Failed, identical on branch/main | Corruption setup blocked by later foreign keys | Pass | Pass |
| `schema33_phase_terminal::actual32_history_survives33_and_reserves_cleanup_without_terminal_effects` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `schema33_phase_terminal::actual_grant_cannot_commit_without_reserved_terminal_capacity` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `schema33_phase_terminal::already_open_writer_cannot_insert_legacy_started_for_actual_phase_grant` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `schema33_phase_terminal::collision_rolls_back33_without_changing_actual32_history` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `schema33_phase_terminal::old_snapshot_classifies_actual030_origin_even_without_visible032_grant` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `schema33_phase_terminal::ordered::actual32_open_owner_history_survives_registered33_upgrade` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `schema33_phase_terminal::relational_only_over_budget_backfill_rolls_back_entire_migration` | Failed, identical on branch/main | Schema 32 Poll queried schema 43 cancellation storage | Pass | Pass |
| `schema33_phase_terminal::ordered::current33_missing_mandatory_objects_refuses_reopen_without_repair` | Failed, identical on branch/main | Release guard replaced by migration 39 | Pass | Pass |
| `schema43_cancellation::cancellation43_upgrade_preserves_dashboard42_operation_and_old42_refuses` | Failed, identical on branch/main | Hard-coded current format 43 | Pass | Pass |
| `schema43_cancellation::cancellation43_upgrade_preserves_prior_records_and_old41_refuses` | Failed, identical on branch/main | Hard-coded current format 43 | Pass | Pass |
