# Playbook authoring B1 (interim): the server on the live-state engine

> **Superseded by the final re-vendor (engine 0.2.0, commit 5b86e0f); see
> "Final re-vendor (done)" at the end.** The interim tree
> `vendor/aq-playbooks-e7df379` is removed. Everything below up to that
> section describes the interim step and still holds unless that section
> says otherwise. One precondition for deploying to a live namespace is
> still open: a measured first open and drain of a copy of the live
> namespace under the configured engine statement timeout.

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
2. **No request times** (fix round 1, ruling 1). `aq-playbook-host`
   `Request::AdmitOnce` and `Request::Event` carry `requested_at_ms:
   Option<i64>` (serde default `None`) and call `admit_once_at` /
   `admit_event_at`, but the server sends `None` on every request, first
   delivery or re-send. A request time based on the row's creation time
   refused work the engine had never seen (queue delay, an outage or a
   forward clock jump counted against the window). The accepted consequence:
   a re-send that reaches the engine after it has forgotten the key runs
   again. `aq-playbook-host::Error::Stale(text)` stays as a safety net: a
   stale answer holds that one row (reason `stale`) and never fails a
   quantum.
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
6. **Accepted events are never sent as new events again** (ruling 2). The
   outbox page says which rows the engine has accepted. For those the driver
   only continues the fan-out by the event's key (`Fanout`, then
   `EventStatus`). When the engine no longer remembers the event (`NotFound`
   or `Invalid`), nothing it still queues holds it, so the row is complete. A
   new row is acknowledged as accepted right after the engine accepts it,
   before its fan-out. `Driver::deliver` (`playbooks/driver.rs`).
7. **The drive never stops for good** (ruling 3, round 2 R2–R4).
   - A refused outbox row or admission is held (`refused`) and the quantum
     goes on. An unknown outcome (a timeout, or a commit that failed in
     storage, which the host now maps to `Unknown`) leaves a never-accepted
     row `pending`: the next quantum delivers it again and the engine answers
     a duplicate if it did commit. So an accepted admission is never held in
     a way whose retry could admit it twice.
   - A row whose decrypted event does not match its record (identity or
     registry) is held `invalid` by the outbox page. A row that cannot be
     decrypted is the keyring's problem: the quantum fails with the reason
     ("cannot decrypt queued events: keyring has no key for one", or "...:
     keyring or stored event"), the row stays queued, and fixing the keyring
     recovers it without a retry.
   - A collector (`expire_decisions`, `collect_*`, the finding
     notifications) that fails does not stop the quantum: its reason is kept
     and the outbox page and the tick still run. A bad item still blocks its
     own source until it is fixed; the others flow.
   - A failing quantum is logged and retried after a backoff from the drive
     interval, doubling up to the engine statement timeout (both
     configuration). The status read shows `automatic_driver_state`
     `retrying` and `automatic_driver_reason`, the reason of the last failed
     or partial quantum (`null` when clean). Only shutdown stops the drive
     (`aq_playbook_host::drive::run`).
8. **Engine timeouts are configuration** (ruling 4): `[playbooks.engine_timeouts]`
   `statement_ms` (default 30,000) and `lock_ms` (default 500), checked at
   load (positive, lock at most statement; no upper bound). They were
   literals (2 s and 500 ms) in `playbooks/runtime.rs`. The owner-call
   deadlines derive from them (`Timeouts::call_deadline`): for a call that
   commits `n` times, `statement × (n + 1) + lock` (one request queued
   ahead), so never below the statement timeout. Default: 60.5 s for one
   commit, 270.5 s for a tick of 8 transitions. They were a fixed 5 s.
9. **Held work shows its reason and can be retried** (rulings 5 and 6).
   Migration 0125 adds `held_reason` (`stale`, `uncertain`, `refused`, and
   `invalid` for outbox rows) to `playbook_admissions` and `playbook_outbox`.
   - Admissions: `GET /api/playbooks/inspection/admissions` items carry
     `held_reason`. `POST /api/playbooks/admissions/{id}/retry` re-sends a
     held one; its answer carries a `warning` that the work may run again.
     An admission the server has recorded as accepted is never admitted again
     (the retry and the dashboard's reconcile return its receipt). The
     dashboard's reconcile of a held admission is a lookup only.
   - Outbox events: no read listed outbox rows, so the nearest existing read,
     `GET /api/playbooks/status`, gains `held_events` (at most 25, newest
     first: event id, producer, creation time, reason). `POST
     /api/playbooks/events/{id}/retry` puts a held event back: a row the
     engine never accepted is delivered as new; a row with an engine receipt
     goes back to `engine_accepted` and resumes its fan-out, never a new event
     (round 2 R1). The outbox page judges "accepted" by the stored receipt.
   - Rolling deletion of these tables: see "B1b: rolling records" below.
10. **A failed engine open does not stop the server** (ruling 4, second
    half). `serve` starts the owner once; when that fails, the server starts
    anyway (`playbooks::runtime::open_later`): every `/api/playbooks/...`
    route that would need the owner, `GET /api/playbooks/status` included,
    answers 503 `{"error":"playbooks_unavailable","message":"playbooks
    unavailable: <reason>"}`, and the open is retried with a backoff (drive
    interval, doubling up to the statement timeout; `drive::retry`). When it
    opens, the admin router built with the owner replaces the unavailable
    one, and shutdown drains it as before. `Runtime::start` itself still
    refuses, so tests and one-shot owner commands see the error.
11. **Cancelling a finished run** that has also rolled out of history is
    confirmed when AQ admitted it (its receipt is in `playbook_admissions`);
    a run AQ never admitted (an event-started run that rolled out, or an
    unknown id) stays `Unknown`.
12. **History-forwarding storage wrappers.** `Engine::new` refuses a backend
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
- **The server's own namespace counters** were removed in B1b (below).

## Final re-vendor: preconditions for deploying

These are preconditions, not follow-ups: nothing on this branch may reach a
live playbook namespace before they are met.

- **The legacy drain**: the engine at or after 25d6545, plus the engine's
  Tasks 10–12, so a namespace written by 0173673 moves its finished runs to
  history instead of rewriting its whole old record on every commit.
- **The engine's remaining tasks** (simulation crate, CLI commands, legacy
  namespaces, thinner `collect_garbage`), at crates 0.2.0.
- A measured first open and drain of a copy of the live namespace under the
  configured engine statement timeout (`[playbooks.engine_timeouts]`).
- ~~The server's lifetime counters removed and rolling deletion of the
  playbook tables (B1b).~~ Done; see "B1b: rolling records".

Mechanics:

- New tree `vendor/aq-playbooks-<rev>/`, pin and archive
  `vendor/aq-playbooks-source-0.2.0.{json,tar}` with the same archive list
  plus `docs/specs/0003-live-state-only.md` and
  `docs/implementation/0003-live-state-only.md` (the semantics the server
  depends on; review M4), then remove this interim tree, pin and archive.
- Run the ignored 257-event test
  (`a_forgotten_accepted_event_completes_its_row_and_starts_no_second_run`)
  once by hand on each re-vendor.
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

## What the first open of an old namespace does (reference for the final re-vendor, not instructions for this one)

- The first open runs engine migrations 6 and 7 (new tables and indexes,
  empty for a namespace that has no history yet) and one bookkeeping commit
  (index rebuild, fills). The engine's reviewer verified that a namespace
  written by the vendored 0173673 engine opens and runs under this engine.
- That namespace still holds the runs that finished before the upgrade: this
  engine has no drain yet. They stay in the State, are served by inspection
  as before, and never block new work. New runs leave the State when they
  finish.
- With no request times, an old outbox row or admission is delivered on its
  first delivery however old it is.
- Retire the old owner before starting the new one, as for every engine
  change (one owner per namespace).

## B1b: rolling records (no lifetime counters)

Architecture 0006: nothing in the server's playbook bridge refuses work
because of how much has happened.

- **Counters gone.** Migration 0126 drops `playbook_namespaces`
  `grant_count` (1,024), `admission_count` (4,096), `effect_count` and
  `cancellation_count` (8,192), `outbox_count` (16,384) and `retained_bytes`
  (64 MiB) with their CHECKs. `Service::budget`, which raised them and
  refused at a cap (grants, admissions, effects, cancellations, every outbox
  producer, health scans and findings), is removed, and so are the
  host-operation bounds (`MAX_RETAINED_OPERATIONS` 10,000,
  `MAX_LINEAGE_OPERATIONS` 256, `MAX_CHECK_EXECUTIONS` 8, refusals
  `execution_limit` and `budget_exceeded`). Single-input size checks stay.
  DEP6 no longer returns outbox "capacity".
- **Rolling deletion.** `[playbooks.retention] keep_finished` (default
  10,000) and `delete_batch` (default 500), both positive, no upper bound
  (`aq_playbook_host::retention::Retention`, validated at config load). Each
  drive quantum runs one table's statement (`aq_playbook_host::retention::TABLES`),
  in turn: it keeps the newest `keep_finished` finished rows of the
  namespace generation and deletes at most `delete_batch` older ones. Work in
  progress never matches.

| Table | Finished (deletable beyond the newest N) | A repeat of a deleted id |
| --- | --- | --- |
| `playbook_outbox` | `engine_accepted` and fanned out | Producers dedupe on their own source (graph and settlement rows acknowledged at engine acceptance, decisions when copied, host-bridge events in their effect's transaction); a re-sent key is the engine's repeat protection |
| `playbook_admissions` | `engine_accepted`, **and** its grant expired, revoked or re-revisioned | Refused as expired: `prepare` needs a live grant (at most 24 h) for a request it has no row for, so it answers `Forbidden` and queues nothing |
| `playbook_effects` | Any recorded status but `pending`, and no running host operation for the intent; an `accepted` review decision only once final and copied | The engine's: it re-dispatches or reconciles only intents of live runs, and a live run's in-progress effect is never deleted |
| `playbook_cancellations` | `confirmed`, `too_late`, `refused` | The engine's: it cancels only live intents |
| `playbook_host_operations` | not `running` | A late completion finds no running row and is answered `Unknown` (no second completion event); `operation.get` comes only from live intents |
| `playbook_health_observations` | reconciled, or expired | The engine's: observation ids belong to live runs; a reconcile is answered from its effect first |
| `playbook_finding_notifications` | submitted | Notification ids are per episode revision, which only goes up; the notification owner dedupes its operation id |

Not covered: `playbook_finding_episodes` (current state per finding key, and
the 100-per-partition check counts absent keys too), `playbook_grants`,
`playbook_work_keys`, `playbook_integration_receipts`.

## The 500-run routing scale test

First measurement of the new engine inside the server.
`real_adapter_pilots::routing_scale::five_hundred_routing_runs_in_one_protected_namespace`,
unchanged (`docs/implementation/0011-R3-scale.md`: same harness, protected
PostgreSQL, `transitions: 2`, 2 s statement timeout), release build:

```
timeout 3600 cargo test --release --locked -p aq-server --test it_playbook -- --ignored routing_scale --nocapture
```

All 500 runs completed `routed` (846 s). Slowest tick per run, against the
old engine (0173673):

| Run | e7df379 | 0173673 | Snapshot (`pg_column_size(encrypted_snapshot)`) e7df379 / 0173673 |
| --- | --- | --- | --- |
| 1 | 175 ms | 150 ms | 5,650 / 39,958 B |
| 50 | 223 ms | 815 ms | 147,758 / 1,890,378 B |
| 100 | 235 ms | 1,265 ms | 292,858 / 3,782,328 B |
| 200 | 224 ms | 2,532 ms | |
| 206 | 228 ms | 2,829 ms | |
| 207 | completed | tick failed `Unknown` | |
| 250 | 250 ms | not reached | 374,283 B |
| 500 | 232 ms | not reached | 374,727 B |

Over all 500 runs the median slowest tick was 242 ms and the 95th percentile
481 ms. Eight runs had a slowest tick above 700 ms, three above 1 s (run 86:
1,198 ms; run 257: 1,272 ms; run 258: 1,218 ms); they are isolated, not a
trend. The host was shared with other agents' builds: load average 33.0 at
the start, 7.7 at the end.

The record stops growing once the remembered set is full: each run leaves two
event entries, so the 256-entry `replay_keep` bound is reached after about 128
runs; `runtime_index` holds 256 events at runs 250 and 500, and the snapshot
is 374 KB at both. The old engine grew about 37 KB per run. Finished runs
are in `aq_playbooks.run_history`, which the harness's storage report does not
count (`inspection_rows` stays 0).

## Tests

New:

- `playbook_bridge::an_old_event_and_one_after_a_clock_jump_are_accepted_on_first_delivery`
  and `playbook_bridge::an_old_admission_is_admitted_on_first_delivery`
  (ruling 1; both were held as stale with creation-time request times);
- `playbook_bridge::an_accepted_event_forgotten_by_its_window_completes_without_a_second_run`
  (ruling 2; with accepted rows re-sent as new events the row never
  completes) and the slow, ignored
  `playbook_bridge::a_forgotten_accepted_event_completes_its_row_and_starts_no_second_run`
  (257 newer events; 261 s in a debug build, run by hand);
- `playbook_bridge::a_row_that_always_fails_is_held_and_later_rows_are_delivered`
  (ruling 3: before, the page failed every quantum) and the host's
  `drive::tests` (a failing quantum is retried with a backoff of 10, 20, 40 ms
  and the drive recovers; it held for good before);
- `playbook_bridge::held_work_shows_its_reason_and_retries_as_a_new_delivery`
  (rulings 5 and 6);
- `playbook_activation::config_refuses_...` cases for `[playbooks.engine_timeouts]`
  and the host's `protected::tests` (ruling 4);
- `playbook_bridge::cancelling_a_finished_run_is_confirmed_and_an_unknown_run_is_not`
  (fails `Unknown` without the `cancel_run` change);
- `aq-playbook-host` `owner::a_stale_keyed_admission_is_typed_and_writes_nothing`.

Round 2 (each failed before its fix):

- `playbook_bridge::retrying_a_held_accepted_event_resumes_its_fanout_without_a_second_run` (R1);
- `playbook_bridge::an_undecryptable_queued_event_fails_the_quantum_with_its_reason` (R2);
- `playbook_bridge::a_storage_failure_leaves_new_work_pending_and_it_is_delivered_after` (R3);
- `playbook_bridge::a_failing_collector_does_not_stop_delivery_admission_or_runs` (R4);
- `playbook_activation::a_failed_engine_open_starts_the_server_unavailable_and_retries` (ruling 4);
- `cancelling_a_finished_run_is_confirmed_and_an_unknown_run_is_not`, extended:
  a run that also left history (M1);
- the host's `protected::tests` (no upper bound; call deadlines).

The identity-mismatch tests now change a row's producer (the digest is in
the sealing AAD, so changing it is a decrypt failure, which fails the
quantum instead).

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

## Server batch S1 (2026-11-13): fix round 3 and two more rolling records

- **N1.** A queued event that cannot be decrypted ends the outbox page before
  it instead of failing the quantum early: rows before it are delivered,
  effects are cancelled and runs tick, and the quantum then fails with
  `cannot decrypt queued event <id> (key id <key>)`. The row stays queued
  (fixing the keyring recovers it). `POST /api/playbooks/events/{id}/hold`
  (owner) holds a named queued row `invalid`; delivery goes on past it, and
  `…/retry` puts it back.
- **N2.** A graph source item that is not a valid event (no task id) is queued
  held `invalid` under its source id, so it is listed with the held events
  and later graph events are collected. The settlement and schedule
  collectors are unchanged.
- **N3.** A refused engine open says which step failed and why
  (`playbook owner startup refused: <step>: <cause>`), in the 503 message of
  the playbook routes, the retry log and the start error.
- **N4.** `[playbooks.engine_timeouts] open_ms` (default 30000, positive, no
  upper bound) replaces the 30 s literal on the engine open.
- **Minor.** An admission whose outcome is unknown stays pending with the
  reason `uncertain` and is taken after the other pending ones. The drive
  keeps one reason per failed stage, joined with `; `. A runtime that opens
  after shutdown began is shut down, and the shutdown drain waits twice the
  Stop call deadline.
- **Finding episodes.** `findings.reconcile` no longer refuses above 100
  episodes per partition. Cleared episodes are the eighth table of the rolling
  deletion (`keep_finished`, `delete_batch`); an open episode never matches.
  A notification's id now includes its time, because a finding that returns
  after its cleared episode was deleted starts again at revision 1.
- **Heartbeat log.** See `docs/operations/row-retention.md`
  (`profiled_presence_operations`, `[workers.heartbeat_log]`, migration 0127).

## Final re-vendor (done): engine 0.2.0, commit 5b86e0f

| | |
| --- | --- |
| Source | aq-playbooks, branch `feat/playbook-authoring`, commit `5b86e0f7d37606f3cab80717d98db0453da56a50` |
| Tree | `vendor/aq-playbooks-5b86e0f/` (182 files) |
| Archive | `vendor/aq-playbooks-source-0.2.0.tar`: `git archive --format=tar 5b86e0f Cargo.toml Cargo.lock crates scripts contracts skills/aq-playbook-author docs/implementation/current-catalog-inspection-design.md docs/specs/0003-live-state-only.md docs/implementation/0003-live-state-only.md docs/implementation/0004-playbook-authoring.md docs/aq-host-integration.md` |
| Pin | `vendor/aq-playbooks-source-0.2.0.json`, checked by `scripts/check-playbook-source.py` |
| Crate versions | model, compiler, runner, postgres, simulation, CLI: all 0.2.0 |

`crates/`, `contracts/` (whole) and `skills/` stay siblings: the compiler
embeds `skills/aq-playbook-author/SKILL.md` and
`contracts/aq-host-2.2.0/{registry,notes}.json` by relative path. The
simulation crate and the compiler's `authoring-examples/` are in the tree.
The interim tree, pin and archive are removed.

What changed for the server (the engine's list of 38 differences is
`vendor/aq-playbooks-5b86e0f/docs/implementation/0004-playbook-authoring.md`):

- **No server code used a changed item.** The server builds no
  `RetentionPolicy` literal, calls neither `history::trim_budget`,
  `retention::verify_removals` nor `collect_garbage`, keeps no receipt index
  cursor, and used none of the removed CLI commands or the CLI library
  (`fake_host`, `pilots`). Only the dependency paths and versions changed.
- **`COMPILER_VERSION`** is `aq-playbooks-compiler/0.2.0`: a draft sealed now
  has a different digest than under 0.1.0. Stored artifacts keep their stamp
  and still load.
- **Legacy drain.** A namespace written by 0173673 now moves its finished
  runs to history, a bounded amount per commit.
- **`PostgresStorage::connect` runs `ANALYZE`** on engine tables without
  planner statistics, at open, outside any commit.
- **Engine retention is configuration.** `[playbooks.engine_retention]`:
  `event_replay_ms` (86,400,000), `admission_replay_ms` (604,800,000),
  `replay_keep` (256), `history_keep` (10,000), `audit_ms` (604,800,000);
  the defaults are the engine's. Windows and `replay_keep` must be positive;
  no upper bound. The owner thread writes them into the namespace's policy
  right after `Engine::new`, in one commit, only when they differ from the
  stored policy (`aq_playbook_host::engine_retention`). The table is separate
  from `[playbooks.retention]` (the server's own bridge rows). The per-commit
  bounds of the engine policy (drain, window drops, evictions, history trim,
  GC page, settle) are not configuration; they keep their stored values.
- **Ignored engine limits dropped.** `playbooks/runtime.rs` no longer passes
  `max_runs` and the other `EngineConfig` values, which the engine ignores;
  the host's defaults apply.
- Unchanged rulings: the server sends no request times, and an accepted row
  is never sent as a new event again.
