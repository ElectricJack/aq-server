# 0011 R3: tick time over many routing runs in one protected namespace

Task 9 of the routing-policy plan. A measurement that decides scope; no product code changed.

## Decision

**Not under 1 s at run 500: bounded storage for protected namespaces and re-vendoring are required before the routing playbook is built on this.**

The release build never reached run 500. The slowest tick first passed 1 s at about run 80, and at run 207 a tick failed (`Error::Unknown`, 2.73 s). The routing design (one run per task in one playbook namespace) cannot be built on the vendored engine `aq-playbooks-0173673` as it is.

## What was measured

`crates/aq-server/tests/it_playbook/routing_scale.rs`, `five_hundred_routing_runs_in_one_protected_namespace` (`#[ignore]`). It reuses the `Pilot` harness of `real_adapter_pilots.rs` unchanged: Host8 catalog, PROTECTED PostgreSQL storage, fake graph, `FakeModel`, the driver limits the pilot sets (`transitions: 2`) and its storage factory (2 s statement timeout, 100 ms lock timeout). One namespace, `pilots::routing()` installed once, then runs started one after another, each driven to completion (`quantum()` plus polling the run, as `Pilot::settle` does) before the next.

- Each run has a distinct work subject (a fresh graph node cloned from the node a coding run produces, registered in `playbook_work_keys` as `work.ensure` would) and a fresh attempt id with no recorded facts, so every run takes the judgment path: `context.read`, `judgment.request`, the wait for `aq.judgment.completed`, `context.read`, `work.change`. That is the longer of the two routing paths. All measured runs ended `completed` / `routed`.
- "Tick" is one `driver.quantum()` call. Each run takes 8 ticks. The slowest tick of the run is reported.
- The pilot lifecycle helper is not called (it would re-version the work node and make the judgment stale); nothing else in the namespace needs it.
- The coding run that supplies the node shape runs in a separate throwaway pilot, so it adds no history to the measured namespace.

## Machine

- CPU: AMD Ryzen 9 5900X, 12 cores / 24 threads; WSL2 (kernel 6.18.33.2-microsoft-standard-WSL2), 31 GB RAM.
- PostgreSQL: 17.11 (Debian 17.11-1.pgdg12+2), container `agent-q-test-db`, 127.0.0.1:55432.
- Other processes: several unrelated Docker containers were up (another aq stack, a Supabase stack, two other Postgres). Load average (1/5/15 min) at the start of the release run: 2.14 / 3.77 / 9.30; at its end: 2.26 / 3.13 / 4.25. Debug run: 1.65 / 2.47 / 3.79 at start, 3.03 / 2.90 / 3.39 at end. An earlier release attempt (cut off at run 113 by nextest's 10-minute default `slow-timeout`, see Command) began while the host load average was about 25 and reached the same tick time at the same run (run 113: 1446 ms; run 116 in the final run: 1438 ms), so host load was not the driver of the growth.

## Command

```
eval "$(scripts/test-db)"
timeout 3600 cargo test --release -p aq-server --test it_playbook -- --ignored routing_scale --nocapture
```

The prescribed `cargo nextest run --release ... --run-ignored only -E 'test(/routing_scale/)' --no-capture` was tried first. `.config/nextest.toml` terminates any test after 10 minutes (`slow-timeout` `terminate-after = 10`), which cut it off at run 113 (it had reached 1.45 s per tick by then), so the recorded run used the `cargo test` fallback. The nextest config was not changed. Debug build: the same command without `--release` and with `ROUTING_SCALE_RUNS=100` (default 500).

## Release build results

Slowest single tick in the run, total time to complete the run, and the engine's storage for the namespace after the run. Runs 250 and 500 were not reached.

| Run | Slowest tick | Run total | `runtime_index` rows | `inspection_rows` | Namespace snapshot (`pg_column_size` of `encrypted_snapshot`) |
|---|---|---|---|---|---|
| 1 | 150 ms | 962 ms | 47 | 26 | 39,958 bytes |
| 50 | 815 ms | 4,652 ms | 2,252 | 1,300 | 1,890,378 bytes |
| 100 | 1,265 ms | 7,722 ms | 4,502 | 2,600 | 3,782,328 bytes |
| 206 (last complete run) | 2,829 ms | 16,984 ms | 9,293 (at failure) | 5,367 (at failure) | 7,815,846 bytes (at failure) |
| 207 | tick failed `Unknown` after 2.73 s | | | | |
| 250, 500 | not reached | | | | |

Other points of the same run (slowest tick): run 10 267 ms, 25 424 ms, 75 973 ms, 125 1,640 ms, 150 1,877 ms, 175 2,487 ms, 200 2,532 ms.

Storage layout (vendored `aq-playbooks-postgres` migrations 0001, 0002, 0004): the whole namespace state is one row in `aq_playbooks.namespaces` (`encrypted_snapshot bytea`, one ciphertext for protected namespaces). Alongside it, `aq_playbooks.runtime_index` (receipts, runs, events, timers) and `aq_playbooks.inspection_rows` (runs, receipts, effects) grow by about 45 and 26 rows per run. `catalog_rows` stays at 2, `disposal_tombstones` at 0. There are no separate event or transition tables; the single snapshot row is rewritten as it grows (`revision` 3103 at run 100). Its size grows linearly, about 37 KB per run.

### The failure

Run 207, release build. The tick returned `Error::Unknown` after 2.734 s. The last five ticks before it, in order: 2.584 s, 0.562 s, 2.502 s, 1.040 s, 2.734 s (the 0.56 s and 1.04 s ticks are the cheap ones within a run; each run has 8 ticks). The harness's storage factory sets a 2 s statement timeout, and the failing tick is consistent with one statement (the snapshot write, 7.8 MB at that point) running past it; this was not verified against the Postgres log, so the cause is unconfirmed. No limit, timeout or storage setting was changed to get past it.

## Debug build results

Debug build, `ROUTING_SCALE_RUNS=100`, stopped by failure at run 35.

| Run | Slowest tick | Run total | Notes |
|---|---|---|---|
| 1 | 679 ms | 4,008 ms | |
| 34 (last complete run) | 5,246 ms | 31,580 ms | |
| 35 | tick failed `Unknown` after 5.038 s | | last five ticks 4.923 s, 2.047 s, 4.998 s, 1.059 s, 5.038 s; 718 receipts, 35 runs, snapshot 1,294,208 bytes |

This matches the D11 observation (one tick growing past the driver's 5 s owner-call bound and failing `Unknown`), here after 35 runs rather than within a handful, because the pilot's `transitions: 2` limit keeps a tick short. Debug is about 4.5 times slower per tick than release at run 1 (679 ms against 150 ms) and about 10 times slower at run 34 (5,246 ms against 513 ms).

## What the numbers show

Tick time is not flat. It grows linearly with the number of runs already in the namespace, with a slight upward bend: a straight-line fit to all 206 release runs is about 85 ms + 12.5 ms per run, with slopes of 12.0, 11.8 and 17.8 ms per run over runs 1 to 70, 70 to 140 and 140 to 206. It tracks the snapshot, which grows linearly (about 37 KB per run) and is rewritten whole on every transition, which is the pre-PB-04 behavior D11 already identified. Run total follows the tick (8 ticks per run), so total time for N runs grows quadratically. Extrapolation, not measurement: the straight-line fit gives about 6.3 s per tick at run 500 (measured: 973 ms at run 75, 1,088 ms at run 80), about 63 s at 5,000 runs and about 625 s at 50,000 runs; a quadratic fit (197 + 9.26 n + 0.0156 n^2 ms) gives about 8.7 s at 500, 7 minutes at 5,000 and far more at 50,000. Both are only extrapolations from 206 points on one machine, and in practice the namespace stops working long before those sizes because a tick fails once it passes the statement or owner-call bound (run 207 in release with this harness, run 35 in debug). A namespace used for routing therefore has a hard life of a few hundred runs.

## Decision line

Not under 1 s at run 500: bounded storage for protected namespaces (aq-playbooks writer format 3 currently requires `protection_key_id IS NULL`, `aq-playbooks-postgres/migrations/0005_transition_records.sql:12-14`) and re-vendoring the engine are required before the routing playbook is built on this, and must be planned and done before Task 14. Rotating namespaces was not used as a workaround.
