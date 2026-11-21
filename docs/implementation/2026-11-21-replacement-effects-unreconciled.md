# Replacement blocked by `replacement_effects_unreconciled` (2026-11-21)

Task `4588d8e5`. The case: attachment task `8c6ba979-993d-4611-b5ab-22228ec25e73`
(active, version 4) is held by its finished attempt
`294917a0-e8dc-57e4-bed8-585011efa1be` (session `cd8f619b-…`). Owner `task.retry`
operation `d36761ce-efbf-452e-8d54-c52767f27edf` was rejected with
`replacement_effects_unreconciled`. The supervisor's `attempt.show` answered
"attempt not found", and `playbook.inspection.runs` answered
`playbook_scope_refused`.

## Evidence

What the sandbox could read through `aq` (task scope only, no database access):

- `task.history`: revision 3 made the task `active` (2026-11-20 19:55:33).
  Revision 4 followed 19 s later. No later revision.
- `comment.list`: one comment, from the attempt's runner (20:07:47): "Final message
  of attempt 294917a0-…: Waiting for the build to finish." The runner submits
  the final message as the result claim, so a result report exists for the
  attempt.
- `attempt.show` is owner-only for this principal (`owner_only`).
  `operation.show d36761ce-…` returns `not_found` for this principal.

I could not read the attempt's retained rows (`profiled_authority_hot`,
`profiled_auto_settlements`, `agent_launch_bindings`) from here. After this
change, the owner reads them with one command (see "Owner recovery").

## Root cause

1. **`attempt.show` found only legacy fixtures.** `GET /api/execution/attempts/{id}`
   read `execution_attempts … AND kind='legacy_fixture'`. Every real task
   attempt is `profiled_v1`, so the route answered "attempt not found" for all of
   them, including this one. `task.work` and `session.show` read other tables,
   which is why they saw the attempt.
2. **B4 effect quiescence was tied to the read-only acceptance pilot.**
   `assess` (`aq-profiled-state/src/replacement.rs`) blocks with
   `replacement_effects_unreconciled` unless the predecessor's hot state has
   `effects = quiescent`. A launched attempt starts with `effects = unknown`
   (`producer/build.rs`). For a stopped attempt with results, only
   `settlement::record_replacement_facts` sets `quiescent`. It runs after a final
   rework (default policy or playbook `work.return`) and from `ReworkSweep`.
   It wrote `quiescent` only when `inspect_aq_effects_locked` returned
   `LedgerDisposition::Settled`. That verdict is `Settled` only for a profile with
   nothing but `read.*` capabilities that sent no mail. Any profile with
   `write.*` or `message.*` got `UnsupportedProfile` or `UnsupportedMessaging`,
   even when every operation in its ledger was final. aq-server developer
   attempts are told to report through the AQ message tool, so their profiles
   include messaging. Their `effects` therefore stayed `unknown` for good: the
   scheduler never replaced them and every owner retry was refused. That
   `Settled` verdict is an acceptance rule (owner settlement eligibility), not a
   statement about what could still be in flight.
3. `playbook.inspection.*` is owner-only by design: `authorize_inspection`
   requires the namespace owner user. A supervisor principal is refused. This
   change leaves that as it is (see "Not changed").

## Change

- `AqEffectsSnapshot::quiescent()` (`settlement/effects.rs`) is true when every
  `agent_graph_operations` row of the attempt is final at its dispatch owner
  (`confirmed`, `conflict` or `rejected`, with the same owner and receipt checks
  as before). Mail send and ack commit together with their mailbox operation, so
  mail is never in flight. A validated `ResolvedProfile` has no capability
  outside these two ledgers. `disposition()`, the acceptance verdict, is unchanged
  (`verdict`, unit test
  `messaging_and_write_ledgers_refuse_acceptance_but_are_quiescent_when_final`).
- `record_replacement_facts` writes `quiescent` from `quiescent()`. When the
  acceptance verdict is not `Settled`, the receipt also records
  `ledger_disposition`. The receipt for a read-only ledger is byte-identical to
  before. It still runs only for a stopped attempt that holds its task and whose
  current report was finally reworked. It never writes for an accepted, held or
  completing result, so review and evidence gates are not bypassed.
  `ReworkSweep` backfills attempts that were already reworked.
- A successor does not re-dispatch the predecessor's operations: each keeps its
  own `aq-agent-v1:{operation}` idempotency key. The predecessor's retention
  receipt and retained source are unchanged. The replacement owner (`release`,
  trigger on `profiled_replacement_releases`) still re-checks stop, retention
  and effects under its locks.
- `attempt.show` on a profiled attempt returns the server-owned B4 facts
  (`replacement_sql::inspect`): `holds_task`, `status`, `stop`, `retention`,
  `effects`, settlement evaluations, automatic settlements (disposition, reason,
  decided_by), `replacement.blockers` (`replacement::blockers`: which fact blocks,
  for example `effects_unknown`, `settlement_needs_reconciliation`,
  `task_completion_recorded`) and `task_assessment` (the B4 decision for the
  task). Legacy fixtures answer as before.

## Owner recovery (after this change is released)

1. Inspect the attempt:
   `aq run attempt.show --param attempt_id=294917a0-e8dc-57e4-bed8-585011efa1be --json`
   - `auto_settlements[].disposition = rework_required` and `effects.kind =
     quiescent` with empty `replacement.blockers`: go to step 2. If `effects` is
     still `unknown`, wait one `ReworkSweep` tick, or the next replay of the
     settlement.
   - `held`, or no automatic settlement: the result has not been returned. The
     review or playbook decision on the attempt's report is still open. Resolve it through
     review (return the result). Retrying does not skip review.
   - `effects_pending`, or `effects.kind = unknown` after a rework: an operation
     of the attempt is not final. Settle it at its owner first.
     `settlement_needs_reconciliation` or `task_completion_recorded` are not
     handled by this change.
2. Retry with a **new** operation ID. `d36761ce-…` is recorded as rejected, and
   repeating it replays that rejection:
   `aq run task.retry --subject task:8c6ba979-993d-4611-b5ab-22228ec25e73:4 --param project_id=<the task's project id> --param expected_version=4 --reason "effects quiescent after 4588d8e5 fix"`
   `aq run` gives the operation a fresh ID. Keep it so you can repeat the same
   call safely.

## Release gates

- Merge and release aq-server through the normal review gates. This change was
  not merged or deployed. No migration; the store format is unchanged.
- Before merge, run the DB test
  `it_profiled profiled_results::auto_settlement56::reworked_writer_attempt_with_final_ledger_becomes_replaceable`
  with `scripts/test-db`. It was not run in the authoring sandbox (no
  PostgreSQL).

## Not changed

- `playbook.inspection.*` stays owner-only. Letting supervisors read it is a
  scope decision for the owner.
- Agent effects outside `agent_graph_operations` and the mailbox (agent task
  creates, comments, waits) were not part of B4 quiescence before and are not
  now.
