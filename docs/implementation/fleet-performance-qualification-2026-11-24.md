# Fleet Performance qualification

## Live AQ on November 24, 2026

Read-only requests to the existing `aq-ms1` admin listener at `http://127.0.0.1:28080` returned the following.

| Request | Result |
| --- | --- |
| `GET /api/v1/bootstrap` | 200. The Performance capability was absent. |
| `GET /api/v1/swarm` | 200. The inventory listed two local machines, both online at the time of the read. |
| `GET /api/v1/performance/summary` | 404 |
| `GET /api/v1/performance/machines` | 404 |
| `GET /api/v1/performance/holds` | 404 |
| `GET /api/v1/performance/tasks/{id}` | 404 for a fixture UUID. |

Swarm supplied a current inventory snapshot. Candidate Performance samples, a live eligible-to-claim interval, and comparisons to real hold and escalation evidence remain unqualified until the candidate is available. The qualification used GET requests and made no deployment or settings changes.

## Candidate fixture assertions

Run the Performance cases in the existing dashboard integration binary against disposable PostgreSQL.

```sh
cargo test --locked -p aq-server --test it_dashboard -- performance::
```

| Evidence | Expected read |
| --- | --- |
| First ready 09:00, eligibility 09:02, claim 09:05 | Three minutes for the covered claim. A claim without eligibility has no elapsed time. |
| Verified merge 10:00, confirmed completion 10:20 | One merge in 60 minutes and one completion in 80 minutes. A cutoff before 10:20 excludes the later completion. |
| Merge delivery definition in force at 09:00 | One delivered task. A settings revision after first ready preserves the cohort's original definition. With no definition both observed outcomes still appear. |
| Hold starts 09:00 and receives a reminder | The 10:30 report shows age 90 minutes and the rescheduled next check. |
| Two machines share one ready count. The idle machine has a stale heartbeat at 09:11. | Count one on the fleet timeline. The stale minute splits the idle intervals. |
| Canonical model receipts with a synthetic USD rate book | USD 0.50 estimated across three canonical calls. A later correction replaces USD 0.05 with USD 0.07, yielding USD 0.52. Replaying both packets after reopening the store preserves one contribution per call and the report at the earlier cutoff. |

The fixture seeds the scheduler's durable clock rows and the historical result and forge evidence directly in the disposable database. It checks their interpretation by the mounted reads. Canonical usage goes through the observation owner. Two calls belong to the original attempt, whose retained synthetic replacement record records failure. The third belongs to a second attempt with a different selected and executing model. The native observation record binds calls to attempts and lacks a parent/child attribution field, so the additional call stays with the original attempt. Server restart coverage reopens the store and reconstructs the router against the same database.

## Restored AQ database

The previous format-177 database backup restored into an isolated PostgreSQL instance (894 MB after restore) and migrated to 178. With the cutoff fixed to the backup time, the seven-day fleet queries found 4,209 canonical calls across two models, 443 attempts with expected usage, and 575 retained claims. None of the historical claims had an observed eligibility clock. The calls used subscription billing and had no price references or provider-reported prices. This dataset supports token and coverage comparisons. It does not establish spend per delivered task.

The qualifying sample included 41,871 retained observation events. Across twelve runs on a host with concurrent builds, the fleet call query took 401 ms at p50 and 576 ms at p95. Queue diagnostics took 3.3 ms and 6.1 ms respectively. Project-scoped outcomes took 22 ms and 43 ms. These are SQL execution times.

In an alternating twelve-run comparison, projecting attempt identity and the unresolved flag before grouping removed 4,543 temporary blocks from the usage-attempt query. The same 443 result rows were returned, and p50/p95 fell from 398/682 ms to 272/357 ms. The release includes the narrower projection.

## Authenticated worker polling

The benchmark uses the format-178 integrated server with clock attestation (8488faff) and one unchanged test executable for all four arms. It runs the candidate server’s normal startup and authenticated worker HTTP listener over loopback. Each arm creates its own database on the disposable PostgreSQL 17.11 test instance. The existing native fixture supplies the synthetic Graph and Git owners and a registered worker. The server publishes one new offer in each arm, then 250 polls are paced at 30 ms. The mix is 200 zero-capacity polls and 50 redeliveries of the same durable offer. Every response is decoded and validated against its request. There are two diagnostics-disabled and two enabled arms in off/on/on/off order. Fleet sampling is disabled in every arm to isolate scheduling capture.

The reported percentiles are nearest rank over requests, inclusive of authentication, HTTP, database reads, serialization and response validation. Throughput reflects the paced workload. Database writes are the PostgreSQL insert/update/delete tuple-counter differences, taken after the five-second collector flush and the stats publication wait. Diagnostic tuple writes are restricted to the performance scheduling tables. Warmup and flush waiting are outside the measured request interval.

The workload compares admission and redelivery behavior with diagnostics toggled. It uses a synthetic scheduler fixture with one worker, bounded concurrency and a fixed fixture clock. Production two-machine behavior, machine-sampler overhead, saturation throughput and first-claim latency need separate measurements.

## Results

| Arm | Polls | Polls/s | p50 ms | p95 ms | All tuple writes/s | Diagnostic tuple writes/s | Captured | Dropped |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 off | 250 | 19.96 | 16.579 | 196.785 | 12.85 | 0.00 | 0 | 0 |
| 2 on | 250 | 33.15 | 10.014 | 116.088 | 34.47 | 26.91 | 200 | 0 |
| 3 on | 250 | 27.32 | 11.345 | 141.798 | 37.48 | 22.18 | 200 | 0 |
| 4 off | 250 | 27.47 | 12.295 | 142.341 | 15.60 | 0.00 | 0 | 0 |

Every arm published one durable claim, returned 200 valid empty zero-capacity pages, redelivered the same offer 50 times, and returned zero cancellations and zero poll errors.
Write rates normalize tuple changes to the request interval after allowing the collector to flush.

Baseline p50 varied from 12.295 to 16.579 ms and p95 varied from 142.341 to 196.785 ms. With diagnostics enabled p50 was 10.014 to 11.345 ms and p95 was 116.088 to 141.798 ms. The four runs showed no latency increase.

Two runs per setting screen for regressions. Builds were active on the same host throughout the measurement, so host variation could account for the lower enabled latencies or conceal a small regression.
