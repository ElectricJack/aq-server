# Playbook authoring B1 (interim): the server on the live-state engine

Plan: `docs/superpowers/plans/2026-11-13-playbook-authoring.md`, Task B1.
This is an interim version of that task. It moves aq-server from
`vendor/aq-playbooks-0173673` to the engine as it is now, so the server tasks
that need the new engine can start. A second, smaller re-vendor follows when
the engine is final (crates at 0.2.0).

## What is vendored

| | |
| --- | --- |
| Source | aq-playbooks, branch `feat/playbook-authoring`, commit `e7df37944a8214b26d475920ac3db5bce4fd3c67` |
| Tree | `vendor/aq-playbooks-e7df379/` (104 files) |
| Archive | `vendor/aq-playbooks-source-0.1.3-e7df379.tar`, `git archive --format=tar e7df379 Cargo.toml Cargo.lock crates scripts docs/implementation/current-catalog-inspection-design.md skills/aq-playbook-author contracts/aq-host-2.2.0` (the B1 archive list) |
| Pin | `vendor/aq-playbooks-source-0.1.3-e7df379.json`, checked by `scripts/check-playbook-source.py` |
| Crate versions | runner, postgres, CLI 0.1.3; model, compiler 0.1.0 (unchanged numbers; the engine bumps them to 0.2.0 at the end) |

The old tree `vendor/aq-playbooks-0173673` and its pin and archive
(`aq-playbooks-source-0.1.3.{json,tar}`) are removed. The pin carries the
commit in its name because the crate version did not change. `Cargo.lock` did
not change: path crates keep their names and versions, and the engine added
no dependency.

## What changed for the server

1. **Finished runs leave the engine State** in the commit that finishes them
   and are written as history (a rolling log, `history_keep`, default 10,000
   runs). The server already read runs only through `Request::Inspect`
   (`playbooks/inspection.rs`, `driver.rs` `inspect_run`), which serves live
   State plus history, so the dashboard run list and timeline need no change.
   `Request::Runs` (the engine index) now lists live runs only; the server does
   not use it. Tests that read finished runs from the State now read them
   through inspection (listed below).
   One server path did change: `Driver::cancel_run`. The engine's cancel of a
   terminal run is a no-op, but a finished run is no longer in the State, so
   the engine answers "not found" and the dashboard's `playbook.run.cancel`
   returned `unknown` (202) whenever the run had already finished (seen as a
   flaky `dashboard_playbook_actions_start_cancel_and_refuse_stale_activation`).
   On an engine refusal the driver now reads the run through inspection and
   confirms the cancel when the run is finished; an unknown run is still
   `unknown`.
2. **Request times and `Error::Stale`.** `aq-playbook-host` `Request::AdmitOnce`
   and `Request::Event` carry `requested_at_ms: Option<i64>` (serde default
   `None`) and call `admit_once_at` / `admit_event_at`. The driver passes the
   row's `created_at_ms`: the `playbook_admissions` row for a keyed admission,
   the `playbook_outbox` row for an event. Both are stamped once, from the
   service clock that also supplies `now_ms`, when the row is created.
   `aq-playbook-host::Error::Stale(text)` is new; `runner_error` maps the
   engine's `Error::Stale` to it.
   - A stale admission takes the existing hold path: the row becomes
     `unknown`, the quantum goes on, and it is never admitted as new.
   - A stale event is the outcome of that one outbox row: `Service::hold_event`
     sets it to `unknown` and the quantum goes on with the next event. Before
     this, any engine error on an event failed the whole quantum. The outbox
     page now selects `pending` rows and accepted rows whose fan-out is
     incomplete, so a held row is not delivered again. `unknown` was already an
     allowed outbox status (migration 0023) and was never written before.
3. **No namespace limits.** The quota mechanism is gone from the engine. The
   host's special case for the quota refusal text in `runner_error` is
   removed (dead). `EngineConfig`'s `max_*` values are accepted and ignored by
   the engine; the server still passes them (`playbooks/runtime.rs`) and the
   host still validates their range (`Limits::validate`).
4. **Model and compiler.** `Intent` has `dispatch_sequence` (tests build it
   with `None`). `Proposal.questions` and `unsupported_features` are removed
   (two test literals).
5. **Engine migrations 6 and 7** (`aq_playbooks.run_history`, its indexes and
   `run_history_counts`) are applied by `PostgresStorage::initialize` on the
   owner thread's first connect, as the engine role (`aq_playbook`, which owns
   schema `aq_playbooks`; `scripts/dev-playbook-db.sh`). Migration 7 needs
   exactly what migration 6 needs of that role, so nothing changes in role
   setup. No aq-server migration was added.
6. **History-forwarding storage wrappers.** `Engine::new` refuses a backend
   that does not persist history. The test wrappers in
   `crates/aq-playbook-host/tests/{owner.rs,catalog.rs}` forward
   `persists_history`, `history_page` and `delete_history`.

## Retention values

No new number is hard-coded in server code. The server does not call
`set_retention_policy`, so a new namespace gets the engine defaults and an
existing namespace keeps its stored policy:

| Value | Engine default | Spec 0003 asks the server for | Note |
| --- | --- | --- | --- |
| `event_replay_ms` | 86,400,000 (24 h) | 600,000 (10 min) | AQ event contracts declare 24 h `retention_ms`; the engine uses the larger, so the 10-minute policy changes nothing for them |
| `admission_replay_ms` | 604,800,000 (7 days) | 600,000 (10 min) | |
| `replay_keep` | 256 | | bounds the remembered set in practice |
| `history_keep` | 10,000 | | finished runs kept for inspection |
| `audit_ms` | engine default | | |

These should become Settings (owner rule: every limit and rolling size is a
Setting), applied by the owner thread after `Engine::new`
(`aq-playbook-host/src/lib.rs`, after `engine.config = ...`). The engine
limits in `playbooks/runtime.rs` (`max_runs` 128, `max_events` 2048,
`max_queued_destinations` 256, `max_receipts` 4096, `max_state_bytes` 8 MiB)
are now ignored and can be dropped when the host stops validating them.

## Not done here

- **Admin operations.** `Engine::rebase_retention_clock`,
  `rebase_retention_clock_dry_run` and `lower_stale_floor` are not exposed.
  They would be wired as new owner-thread requests in `aq-playbook-host`
  (`Request` and `execute`), a driver method in `playbooks/driver.rs`, and an
  operator-only route next to the other playbook controls
  (`playbooks/api.rs`), following "When aq-server exposes these operations"
  in the engine's `docs/implementation/0003-live-state-only.md` (actor from
  the authenticated identity, the server clock as `now_ms`, `to_ms` checked
  against PostgreSQL `now()`, dry run first, explicit confirmation).
- **The server's own namespace counters.** `playbook_namespaces`
  `admission_count`, `effect_count`, `cancellation_count`, `outbox_count` and
  `retained_bytes` are lifetime counters with fixed caps (migration 0023,
  lines 15-19), raised by `Service::budget` (`playbooks/service.rs`) and never
  lowered. They are unchanged here.

## For the second re-vendor (engine 0.2.0)

- New tree `vendor/aq-playbooks-<rev>/`, pin and archive
  `vendor/aq-playbooks-source-0.2.0.{json,tar}` with the same archive list,
  then remove this interim tree, pin and archive.
- `crates/aq-playbook-host/Cargo.toml` (`=0.2.0` for runner and postgres;
  model as published), `crates/aq-server/Cargo.toml` (compiler `=0.2.0`), the
  root `Cargo.toml` `exclude`, the `aq_playbooks_simulation` crate if the
  server depends on it, and `Cargo.lock` (the version numbers change).
- The pin path in `crates/aq-server/tests/it_playbook/real_adapter_pilots.rs`
  (`include_str!` of the pin).
- The README version section of `aq-playbook-host`.
- Whatever the engine's remaining tasks change in the public API: the legacy
  drain (old finished runs leave a namespace written by the old engine),
  `collect_garbage`, and the new compiler and simulation items.
- The retention Settings above, if that task is scheduled with it.

## Upgrading a live namespace

- The first open runs engine migrations 6 and 7 (new tables and indexes,
  empty for a namespace that has no history yet) and one bookkeeping commit
  (index rebuild, fills). The engine's reviewer verified that a namespace
  written by the vendored 0173673 engine opens and runs under this engine.
- That namespace still holds the runs that finished before the upgrade: this
  engine has no drain yet. They stay in the State, are served by inspection
  as before, and never block new work. New runs leave the State when they
  finish.
- Outbox rows and admissions that are older than their window when first
  delivered are held as stale (`unknown`), not delivered. With the default
  windows that is 24 hours for events and 7 days for admissions.
- Retire the old owner before starting the new one, as for every engine
  change (one owner per namespace).

## Tests

New:

- `playbook_bridge::one_stale_event_is_held_and_does_not_block_the_events_behind_it`
  (two outbox events in one page, the first with a request time older than
  every window: it is held `unknown`, the second is accepted and fanned out,
  and the quantum succeeds; without the driver's `Stale` arm the quantum
  fails `Unknown`);
- `playbook_bridge::a_stale_admission_is_held_and_the_next_one_is_admitted`;
- `playbook_bridge::cancelling_a_finished_run_is_confirmed_and_an_unknown_run_is_not`
  (fails `Unknown` without the `cancel_run` change);
- `aq-playbook-host` `owner::a_stale_keyed_admission_is_typed_and_writes_nothing`.

Rewritten because a finished run is no longer in the State (intent kept):

- `aq-playbook-host` `owner::actual_engine_install_admit_tick_inspect_and_explicit_stop`:
  the live run is listed before the tick; after it the run is absent from the
  State, `Runs` is empty and `Inspect` returns it `completed`.
- `aq-playbook-host` `postgres::protected_actual_engine_exclusive_owner_restart_and_key_failure`:
  the stored State no longer holds the run; after restart `Inspect` (not
  `Runs`) lists it `completed`.
- `aq-playbook-host` `owner::engine_admission_capacity_is_typed_and_keeps_existing_work`
  became `owner::ignored_engine_limits_admit_new_work_and_keep_existing_work`:
  with `max_runs = 1` (ignored) a second keyed admission is admitted, and a
  replay of the first still returns its receipt.
- `playbooks::coding::tests::settle` and the async-completion test read root
  outcomes through `Request::Inspect` (`root_outcomes`).
- `playbooks::artifacts::tests::activation_switches_future_runs_only` reads
  each admitted run through `Request::Inspect`, in admission order.

Runs: `-p aq-playbook-host` (with the ignored PostgreSQL test) 15/15;
`it_playbook` 130 passed, 5 ignored; aq-server lib `playbooks::` 19/19;
`it_schema` 83/83; `it_profiled` `profiled_results::` 107/107;
`it_dashboard` 207/207 (run with `--test-threads 4`; at the default the
shared test database refused connections, "too many clients").
