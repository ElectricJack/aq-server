# 0013 Playbook authoring: implementation note

What was built for [spec 0013](../specs/0013-playbook-authoring.md) on branch
`feat/internal-work`, what the tests show, and what is not yet proven. The
plan is
[`superpowers/plans/2026-11-13-playbook-authoring.md`](../superpowers/plans/2026-11-13-playbook-authoring.md);
the operator's steps for the live check are in
[`operations/0013-live-authoring-runbook.md`](../operations/0013-live-authoring-runbook.md).

**Evidence level: Tested.** Nothing here has run on a live stack, and no real
model has authored a playbook.

## What was built

- **Engine 0.2.0, vendored.** `vendor/aq-playbooks-5b86e0f` (pin
  `vendor/aq-playbooks-source-0.2.0.json`): finished runs leave the State as
  history, rolling repeat protection, no namespace limits, the authoring
  library (`draft::seal`, `compare`, `retain_registry`, the cases simulator)
  and the agent's CLI (`check`, `seal`, `try`, `compare`, `facts`, `save`,
  `ask`, `unable`). Engine retention is configuration:
  `[playbooks.engine_retention]`.
- **Host9 = host catalog 2.2.0.** Profile `host_v9`
  (`vendor/aq-host-catalog-2.2.0`), sixteen commands: the Host7 twelve plus
  `work.read`, `work.route`, `work.unroutable` and `judgment.assess`.
  `installation_routing = true` (only with `host_v9`) lets those four serve a
  task of any project on the automatic router.
- **`aq.work.route_needed`.** Placement records a ready task that waits for a
  route (`route_needed_emissions`); the playbook owner seals one event per
  cause in its next drive quantum, and not again within two minutes for the
  same task version.
- **Authoring.** Crate `aq-playbook-authoring`: a source (a document, a record
  of the commands and events it may use, facts) is given to an internal-work
  agent run (purpose `playbook_compilation`) in a workspace with the guide, the
  language reference, the rendered catalog, the examples and, when one exists,
  the playbook in effect with its cases. `[playbook_authoring]` holds the
  deadline, turns, outcome size, case budget and the number of finished runs
  kept.
- **Save-time re-seal and case re-run.** Only the draft and the cases are
  taken from the agent. The server seals the draft again from its own record,
  catalog, facts and document, verifies the artifact, holds its contracts to
  the pinned catalog, its abilities to the record and the grant, and runs
  every case on its own artifact (`save::check_saved`). Quoted sentences of a
  question or an "unable" are read again from the server's document by line.
- **Taking effect.** Install, record the run as active, activate in the
  engine; an activation that fails is retried on every tick until the engine
  has the artifact, under the source lock so a retry never undoes a rollback.
  After a restart the active playbooks are installed and activated again.
- **Rollback.** One saved playbook back, under the source lock; it supersedes
  a run in flight.
- **Grants.** Each source has one grant that does not expire (R2); it is
  revoked when the source is deleted. Owner grants made through the API still
  expire.
- **Sources HTTP API.** `GET/POST /api/playbooks/sources`,
  `GET/DELETE /api/playbooks/sources/{id}`, `PUT …/{id}/markdown`,
  `POST …/{id}/runs`, `POST …/{id}/rollback`; owner only, and reachable by a
  supervisor through its admin nest. Shapes are in the chain C report.
- **Routing on the authoring path.** The built-in source `routing` (binding
  `aq.routing`) is the routing text of the Intelligence structure, with the
  facts `levels`, `offers`, `harnesses` and `providers` from Settings. A save
  in Settings > Intelligence authors it; the routing switch turns on only when
  the engine has acknowledged an active routing playbook. No code in the
  server decides a route: the playbook reads the task, asks its judgment and
  writes the route, and placement offers the task on the routed level.
- **Unroutable tasks are raised once per cause.** A `not_offered` refusal, a
  recorded `work.unroutable`, and a task still unrouted ten minutes after it
  first waited each become one failed internal-work item
  (`routing_classification`) per task version, which follows the existing
  escalation to a supervisor and then the owner's attention list. A route
  written by anyone resolves it.
- **Rolling records.** Finished authoring runs keep the last
  `keep_finished_runs` (50) plus the active and the previous one; the server's
  playbook bridge tables, finding episodes and the heartbeat log are deleted on
  a rolling basis (migrations 0126 to 0128); nothing finished blocks a new run.
- Migrations 0121 to 0131. 0121 drops `routing_policies` and turns routing
  off; saving the document authors the playbook that turns it on again.

## Evidence for the spec's acceptance items

Tests are in `crates/aq-server/tests/it_playbook` unless a crate is named.
"Prepared outcome" means the agent's side is the engine's worked example
handed to the save path; no model ran.

| # | Acceptance | Evidence | Level |
| --- | --- | --- | --- |
| 1 | Today's routing document is authored with no edit, on a subscription login | None. | Not proven |
| 2 | Tasks are routed as the document says, with no routing code deciding | `playbook_routing::a_saved_routing_document_routes_a_new_task_and_places_it_on_the_routed_level`: a saved document, one authoring run, activation on `aq.routing`, the switch, `route_needed`, read / judge / route, the task offered on the routed level. Real placement, engine, save path and ports; prepared outcome; fake Graph peer and judgment provider. One task of one kind, not every kind the document names. | Tested in part |
| 3 | Changing a sentence changes the steps and the cases | Only the mechanism: the view's `comparison` and cases in `playbook_authoring::a_registered_source_is_authored_and_its_view_shows_the_cases`. That a changed sentence changes the draft needs a model. | Not proven |
| 4 | A second, unrelated document runs through the same service | `playbook_authoring::a_registered_source_is_authored_and_its_view_shows_the_cases` (the `notify-when-ready` example registered through the API, same save path, same guide and tools; prepared outcome) and, on the real engine and grant tables, `playbook_findings_triggers::an_authored_playbook_runs_under_a_source_grant_that_never_expires`. | Tested in part |
| 5 | A sentence the catalog cannot serve is refused with that sentence, and the previous playbook keeps running | `playbook_authoring::a_question_or_unable_changes_nothing_and_keeps_the_sentence` | Tested |
| 6 | A save whose cases do not run as stated is refused | `aq-playbook-authoring` unit test `save::tests::a_save_whose_case_does_not_run_as_stated_is_refused`; `playbook_authoring::a_save_the_server_cannot_reproduce_is_refused_in_the_owners_words` | Tested |
| 7 | 500 routing runs keep step time flat | The ignored test `real_adapter_pilots::routing_scale::five_hundred_routing_runs_in_one_protected_namespace`, run by hand in a release build. It measures the Host8 routing pilot, not an authored playbook. | See below |

Scale test (acceptance 7): see the coordinator's numbers.
Measured 2026-11-14 on commit 37bd30a7 (release build, the Host8 pilot playbook, a shared developer machine under other load): 500 of 500 runs `routed` in 618 s. Slowest tick per run: 184 ms at run 10, 200 ms at run 250, 204 ms at run 500 (1.11x of run 10); over all runs median 187 ms, p95 224 ms, max 348 ms. Server bridge tables at run 500, with `keep_finished` set low by the test: `playbook_outbox` 20, `playbook_effects` 20, `playbook_host_operations` 20, `playbook_admissions` 50 - flat, with effects deleted against the engine's real `run_history` (13,000 rows). This does not yet measure a playbook written through the authoring path.

Earlier runs of the same test on this engine, for comparison: at the final
vendor (chain B, shared host with load average 7 to 10) all 500 runs ended
`routed`; slowest tick 178 ms at run 1, 380 ms at 50, 247 ms at 100, 218 ms at
250 and 275 ms at 500, median 215 ms. The engine before 0.2.0 went from 150 ms
to 2,829 ms at run 206 and failed at run 207.

Other tests this note relies on:

| Behaviour | Test |
| --- | --- |
| Install, activate, previous kept for rollback | `playbook_authoring::a_saved_playbook_is_installed_and_activated_and_the_previous_kept_for_rollback` |
| Rollback | `playbook_authoring::rollback_reactivates_the_previous_artifact`, `…::a_rollback_holds_its_target_until_it_is_active`, `…::a_late_activation_retry_never_undoes_a_rollback` |
| Activation retry | `playbook_authoring::an_activation_that_fails_is_tried_again_until_the_engine_has_the_artifact` |
| Switch needs a deployed playbook | `playbook_authoring::the_switch_needs_a_routing_playbook_the_engine_has_acknowledged` |
| A save missed while the engine was closed | `playbook_authoring::a_missed_save_is_authored_on_reconciliation_and_an_ended_run_is_not_repeated` |
| Facts changed during a run | `playbook_authoring::a_save_whose_facts_changed_is_refused_and_authoring_starts_again` |
| Abilities within the grant | `aq-playbook-authoring` `save::tests::a_save_needing_an_ability_the_grant_does_not_give_is_refused` |
| Rolling authoring runs | `playbook_authoring::finished_runs_are_a_rolling_set_that_keeps_the_active_and_the_previous` |
| `route_needed` starts a run that routes | `playbook_host9::adapters::the_event_starts_a_run_of_an_installed_artifact_that_triggers_on_it` |
| Raised once per cause | `playbook_host9::adapters::a_task_routing_cannot_route_is_raised_once_per_cause_and_again_for_a_new_version`, `…::a_task_unrouted_for_ten_minutes_is_raised_once_and_again_for_a_new_version` |
| Judgment purpose of the routing source | `playbook_host9::adapters::a_routing_playbook_judgment_runs_with_the_routing_classification_purpose` |

Suite on the final tree before this task (37bd30a7): 2481 passed, 41 skipped
(whole workspace, nextest). After this task's change to `aq-routing`:
`cargo check --workspace --all-targets --locked` is clean and
`playbook_routing::` + `playbook_authoring::` pass (36 tests); the whole suite
was not run again.

## Inventory check

`scripts/check-authoring-inventory.sh ENGINE_DIR` checks that every piece of
the old compile path is removed or has a caller on a tool or save path. It
applies the engine rules to the engine checkout and to the vendored copy, has
no allowlist, and exits 1 on any finding.

Result on 2026-11-14 against engine 0.2.0 (`feat/internal-work`, 26b3082):
**1 open finding, exit 1.**

- **Open: the Host8 routing pilot**,
  `crates/aq-playbook-domain/src/coding/pilots.rs` `pub fn routing()`
  (`aq.pilot.routing`). No server path uses it. Its two users are tests: the
  routing section of `real_adapter_pilots::real_adapter_pilots_complete` (D11
  evidence, which records the pilot's digest) and the acceptance 7 scale test.
  Removing it means moving the scale test to an authored routing playbook on
  Host9 and cutting the D11 routing section, then running both; that is not a
  small change and was not done. The script keeps failing until it is.
- **Fixed in this task:** `struct Classification`, `Risk`, `Flag` and
  `classify::schema` are removed from `aq-routing` (the old classifier's
  answer shape; nothing read them). `Route.classification` stays as an
  optional JSON value so stored routes and the route history keep their
  shape.
- Clean: no `aq_routing::{policy, plan, compile, fixtures}`, no
  `COMPILER_INSTRUCTIONS`, `routing_policies` only in migrations 0120 and 0121
  and the test of the drop, no `unsupported_features` or `Severity::Question`,
  no `Command::{Surface, Interfaces, Simulate}`, no `fake_host` or `pilots`
  module in the engine CLI's `src`, no routing pilot in the engine.

Where the script differs from the plan's list, because code moved after the
plan was written:

| Plan | Checked now |
| --- | --- |
| `routing_policies` outside `migrations/012[01]*` | also allowed in `tests/it_schema/routing_policies_migration.rs` (the test of the drop) and its `mod` line |
| `pub fn routing()` in either `pilots.rs` | every `pilots.rs` under the server's `crates/` and the engine's `crates/` (the server's is now in `aq-playbook-domain/src/coding/`, the engine's in the CLI's `tests/support/`) |
| `artifacts::validate` in `playbook_authoring.rs` | the equivalent check, contracts equal to the pinned catalog, inside `check_saved` in `crates/aq-playbook-authoring/src/save.rs`, and `check_saved` called from `complete.rs`. `artifacts::validate` itself (now in `aq-playbook-domain`) is still called by the artifact API |
| `retain_registry` in `coding.rs` | `crates/aq-playbook-domain/src/coding.rs` (moved out of `aq-server`) |
| `retain_registry` in `request.rs`, `draft::seal` and `run_cases` in `save.rs` | `crates/aq-playbook-authoring/src/` (as planned) |
| `route_needed_emissions` in `routing.rs` | `crates/aq-server/src/routing.rs`, and also the collector `playbooks/triggers.rs` |
| `parse_source_header` reached from `validate` | inside `validate` in the compiler's `lib.rs`, and `draft.rs` calls `validate` |

## Not yet proven

- **No real model has authored a playbook.** Every test hands the save path a
  prepared outcome. Acceptance 1, and the model's half of 2, 3 and 4, wait for
  the live check in the runbook.
- **Real-login checks in the runbook**: Claude enforces `Bash(aq-playbooks:*)`
  against compound commands; the Codex sandbox starts in the hardened
  container; memory stays within 1 GiB.
- **Plan tests not written.** Four of D2:
  `a_settings_change_that_changes_the_facts_authors_again`, the question
  naming the two ambiguous sentences, `a_classifier_outage_still_routes…` and
  `acceptance_1_…`; and the 12-case routing fixture, so acceptance 2 is shown
  for one kind only.
- **Acceptance 7 on an authored fixture.** The scale test still runs the
  Host8 pilot (the open inventory finding).
- **Engine benchmarks not re-run**: the burst and upgrade benchmarks.
- **Engine step time at 10,000 runs is 1.18× the start**, of which about
  1.12× is unexplained.
- A measured first open and drain of a copy of the live namespace by engine
  0.2.0 (see the warning at the top of [`status.md`](../status.md)).
- `ServerRouting`, the production routing port, has no test of its own beyond
  the end-to-end test; the reconcile timer has none.

## Known gaps and follow-ups

Limits that are constants and must become Settings (owner rule: no hard-coded
limits):

- 10 minutes before an unrouted task is raised (`host9::UNROUTED_RAISE_MS`).
- 15 minutes a run that read a task holds back a new ask
  (`host9::ROUTE_RUN_BOUND_MS`); also the 30 s and 5 min freshness windows of
  `route_needed` beside it.
- Agent files 4 MiB (`aq_internal_work::AGENT_FILES_BYTES`) and the agent
  outcome limit (`AGENT_MAX_OUTPUT_BYTES`, at most 4,194,304 bytes).
- The Codex sandbox probe's 10 s.
- The 6 MiB HTTP body limit of the sources API.

Behaviour:

- An artifact larger than 1,048,576 bytes cannot be installed, whatever
  `[playbooks] request_bytes` says (default 262,144).
- The routing playbook is authored again only on an Intelligence settings
  save, when the engine opens, and by the periodic reconcile (every 30 polls).
  A change of offers by catalog activation or project binding does not start
  it; `POST /api/playbooks/sources/routing/runs` does.
- `playbook_grants`, `playbook_work_keys` and `playbook_integration_receipts`
  still grow without rolling deletion.
- A bad item is skipped only by the graph collector; the other collectors
  still fail their quantum on one.
- The reason the playbook engine is unavailable is not on the dashboard's
  attention list (the playbook routes answer 503 with it).
- `work.read` answers `origin` only as `playbook` or `unknown`.
- `notes.json` of catalog 2.2.0 still differs from the server in: the
  `judgment.assess` notes on no fields and on oversized requests (the bridge
  rejects those before the adapter), `budget_exceeded` (cannot occur under the
  contract's maxima), `origin`, and "at once" for `route_needed` (the next
  worker poll plus one drive quantum, only while a node polls).
- The server's database role must be able to read `aq_playbooks.run_history`:
  repeat asks and the rolling deletion of effects depend on it.
- There is no move of a namespace between profiles. `host_v9` needs a new
  `namespace_id` and name with `generation = 1`; the old namespace's rows are
  kept and its unfinished runs are no longer driven (runbook step 3).
- A crash between the graph write of a route and its effect row makes the
  repeat `stale`: the route is written once, the run ends `stale`.
- A supervisor's or owner's retry of a failed authoring task is not picked
  up; author again.
- The engine has no deactivation: deleting a source revokes its grant and
  leaves the binding reserved for that id.
- A routing run that ends `failed`, and "placement finds no candidate for the
  route", are not causes of their own; the first is caught by the ten-minute
  raise.
- `work.read` refuses tasks of projects not on the automatic router, and the
  host originates `aq.work.changed` only for a playbook's own writes. Both
  must change before a second real playbook is useful.
- C2 review items not done: the CI step for the two root-only uid-isolation
  tests, redaction of pre-run Codex token values in Claude runs, the owner
  change order in `hand_over`.
- Dashboard: five test failures in `SwarmSettings` and `AddMachine` predate
  this work (missing mock handlers).
- The live Settings copy of the routing document does not yet have the
  owner's two clarified sentences; update it at deploy (runbook).
