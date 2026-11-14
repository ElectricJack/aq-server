# Retention audit: which AQ stores delete old records, and which refuse work instead

**Date:** 2026-11-13. **Scope:** aq-server, aq-worker, aq-graph, aq-git on
`feat/internal-work`; the playbook engine was excluded because it is being
fixed under the same rule (aq-playbooks spec 0003). The audit read code and
documents only. Nothing was run.

## The rule

Owner, 2026-11-13: "Just remove all 'limits' and just enforce deleting old
records. This should be how ALL SYSTEMS that log in agent queue are designed."
And: "Nothing should have cap that blocks additional runs… over time we will
have infinite runs."

A store that accumulates per-item records keeps a rolling set of fixed size
or age and deletes the rest automatically. It never refuses new work because
it is full. Records that are the product (tasks, projects, settings,
artifacts, the knowledge graph) are not logs.

## Headline findings

1. Many "retained operations / bytes" counters only ever go up and are
   enforced by a CHECK or a guarded UPDATE. At the cap, new work is refused
   for good. Spec 0008 phase A converted only the sealed-admission caps.
2. Spec 0008 phase B (deletion of sealed history) has no code.
3. The one running deletion job (DEP6, `aq-row-retention`) only collects work
   of tasks the graph reports `completed`. Cancelled, retired or never
   completed tasks never age out.
4. `dashboard_operations` retirement exists, and only tests call it.

## A. Lifetime counters that permanently refuse work (fix first)

| # | Store | Cap | What stops at the cap | Where |
| --- | --- | --- | --- | --- |
| 1 | Supervisor conversations (global and per project) | 4,096 messages / 64 MiB, a trigger forbids decrease | The supervisor cannot be messaged or started. Every owner message, reply and system notice counts | `migrations/0035`, `0109`; `aq-supervisor-control/lib.rs:56-58,139`; `writes.rs:266-272` |
| 2 | Central supervisor actor operations | 4,096 / 64 MiB | configure, reserve, cancel refused | `0037:74-92`; `reservations/writes.rs:84` |
| 3 | Notifications | 4,096 messages, 4,096 deliveries, 8,192 operations, 128 MiB; never decremented | every notification fails (429) | `0011:3-9`; `notifications/persistence.rs:99-110` |
| 4 | Dashboard task creation journal (`human_graph_operations`) | 4,096 rows / 64 MiB, undeletable | creating a task from the dashboard is refused | `agent_access/human/commands/journal.rs:166-172` |
| 5 | Server playbook bridge counters (`playbook_namespaces`) | admissions 4,096; effects 8,192; cancellations 8,192; bytes 64 MiB; host operations 10,000 | every playbook operation fails, including judgments. Health scans and findings charge the bytes | `0023:12-20`; `playbooks/service.rs:330`; `host_adapters/requests.rs:25-27` |
| 6 | Review budgets | 10,000 lifetime operations; 256 per task | reviews and decisions refused | `0026:3-12`; `reviews.rs:318-354` |
| 7 | Result media captures | 4,096 ever; 512 per project | media capture refused | `0036:19-25`; `result_media/db.rs:57` |
| 8 | Profile catalogs | **128 catalogs** / 64 MiB; one is minted per effective profile edit | saving a profile change in Settings is refused | `profile_catalog.rs:18-19,157-163` |
| 9 | Settings save histories | 4,096 to 100,000 per kind | a Settings save is refused | `profile-settings/store.rs:168`, `structure.rs:241`, `aq-internal-work/settings.rs:202`, `swarm-settings/providers.rs:130`, `supervisor-control/api.rs:108` |
| 10 | Terminal registry | 4,096 sessions; 1,024 operations per terminal | new terminals or input refused | `terminal/service.rs:420-424`; `persistence.rs:262,460` |
| 11 | Observation ledger per node | 65,536 events; 256 sources, never decremented | ingest refused (engine not adopted yet) | `0014:5-9`; `observations.rs:235-238` |
| 12 | Spec 0008 items B1–B8 | e.g. 1,024 registration receipts per node | registration, renewal, settlement refused | `closure/history.rs:114-122`; `renewal/history.rs:15-17` |

Likely to be hit first on a small installation: profile catalogs (128),
supervisor conversations, notifications, the playbook bridge once triggers
are live, and dashboard task creation.

## B. Never deleted, and no cap (grows until the disk is full)

| Store | Growth | Note |
| --- | --- | --- |
| `profiled_presence_operations` (the worker heartbeat log) | one row per node about every 41 s, up to 68 KiB each | delete is refused by a trigger |
| Sealed attempt tables (about 30) | about 300 KB per attempt including indexes | spec 0008 phase B, unbuilt |
| `command_audit` | one row per agent command | nothing reads it but a dashboard "denials" read |
| `internal_tasks`, `internal_task_transcripts` | up to about 2.5 MiB per run | added by this branch |
| `routing_policies`, `playbook_authoring_runs`, `task_routes` | per compile / run / route | added by this branch |
| Operation receipts: dashboard, comments, mailbox, delivery (up to 8 MiB each), central host launches, manual sessions, worker setting/login/diagnostic operations, handoffs, hibernation, waits | per action | idempotent replay reads them |
| Worker per-attempt archive directories | one directory per attempt per slot | only empty ones are pruned |
| aq-git operation journal; aq-graph idempotency receipts | per mutation | kept "indefinitely" by their specs |

## C. Deletion exists but does not reach the rows

- DEP6 `refresh()` uses `completed()` only (`aq-row-retention/src/lib.rs:41-50`); `closed()` exists and is unused.
- `dashboard_api/actions/journal.rs:155 retire()` is called only by tests.
- The server vendors the old playbook engine; its fix arrives with the re-vendor.

## D. Where a spec required keeping, or refusing (owner decided: the rule wins)

**Owner decision, 2026-11-13:** the rule wins. "Just remove all 'limits' and
just enforce deleting old records. This should be how ALL SYSTEMS that log in
agent queue are designed." The principle is
[architecture 0006](../architecture/0006-records-are-rolling.md). Each spec text
below has been rewritten to it (implementation pending); the table keeps what the
specs said before.

| Store | Spec text | Source |
| --- | --- | --- |
| Sealed admission and launch history | "expiry never deletes replay/stop proof. History growth refuses new work until an explicitly qualified maintenance policy exists." | `docs/implementation/AQ-08-authority-bridge.md:195-196` |
| aq-git journal | "Journals retain completed operation IDs indefinitely in the baseline." | aq-git spec 0001 |
| aq-graph receipts | "Receipts are retained indefinitely in the pilot." | aq-graph spec 0001; migration comment |
| Manual transcripts | "Keep archived normalized output until an explicit future retention/deletion policy is approved." The implementation keeps the first bytes and drops the newest. | spec 0009 §5.2 |
| Internal-work runs | "kept as history" | spec 0012 §3 |

## E. Not violations

- Limits on one input's size (payload, field length).
- Controls on work happening now: request slots, offers per node per hour (30), the owner's swarm concurrency matrix, worker slots.
- Budgets the owner set on purpose: retry and time budgets per task lineage, provider quota, cost ceilings. Deleting attempts must preserve the counts these read.
- Per-operation batch sizes.

## Smallest fix, by pattern

1. **Counter caps (A):** stop counting lifetime totals. Keep the newest N rows per owner key, delete older ones on insert or in the existing 10-minute sweep, and remove the cap check (or count live rows only). Drop the "cannot decrease" triggers.
2. **Uncapped logs (B):** add "keep newest N" or "older than T" deletion to the existing sweep. A replay of a deleted operation id is refused as expired, not re-executed.
3. **DEP6 (C):** treat cancelled and retired tasks as settled.
4. **Sealed history:** build spec 0008 phase B as "keep the newest N attempts per project", keeping the per-lineage budget counts.

The full agent report, with every table and cap, is in the session transcript
of 2026-11-13; this file is the working summary.
