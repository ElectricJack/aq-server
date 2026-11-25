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

The fixture seeds the scheduler's durable clock rows and the historical result and forge evidence directly in the disposable database. It checks their interpretation by the mounted reads. Canonical usage goes through the observation owner. The three calls share one enrolled source, so they exercise cost correction across model groups without establishing separate failure, escalation or delegation attribution. Server restart coverage reopens the store and reconstructs the router against the same database.
