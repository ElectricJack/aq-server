# PostgreSQL row retention (DEP6)

The server starts `row_retention::run` with its other maintenance jobs, runs a
pass at startup and every ten minutes, and stops it with the server. Each delete
statement has a limit of 512 rows. Related child/parent deletes share a bounded
transaction; a large backlog drains over successive ticks. The job logs
`rows_deleted` and `table` for every covered table, including zero counts.
Errors retain remaining rows for a later tick. Tests lower the limit to two.

Graph is the task-state owner. `retention_tasks` records the first observation
of a completed task with **all** its attempts released (`holds_task = false`)
and in `finished`, `declined` or `never_granted` state. A fresh Graph node read
must confirm completion again on the collecting pass. Open, missing, unavailable,
deleted or retired Graph state resets that clock. Unknown or unsettled attempts
also reset it. This is a conservative age: installation, an outage or a reopen
can lengthen retention. Discovery and checks rotate through retained task scopes
in batches. An unchecked scope never authorizes deletion on a later run.

The predicates live beside the queries in
`crates/aq-server/src/row_retention.rs`; migration 0061 adds their metadata.

| Table | Age and state required for deletion |
| --- | --- |
| `observation_events` | Recorded more than 14 days ago; owning task settled for 14 days and freshly rechecked; original attempt has a durable closed observation window. At most one node's batch per pass, under the node lock used by ingestion. |
| `observation_facts` | Only facts belonging to the selected eligible event batch; deleted before their parent events. |
| `observation_packets` and `observation_packet_retention` | Recorded more than 14 days ago; **every** contributing attempt has a closed window and an eligible task; no event in the packet's sequence range remains. New packets carry their exact ledger quota charge, returned on deletion. |
| `observation_call_heads` | Original owner is fenced and its task eligible for 14 days; no original-attempt event or referencing fact remains. |
| `observation_attempt_projections` | Last record and settled-task age exceed 14 days; attempt fenced; no event or canonical call head still needs the aggregate. |
| `observation_projections` | Last record exceeds 14 days; a retained source identifies an eligible fenced owner; no lineage event or canonical call head remains. |
| `agent_graph_outbox` | Both `delivered` and `notification_observed` have been observed complete for seven days; original task is eligible for seven days. An independent notification consumer can therefore delay collection. |
| `playbook_outbox` | `engine_accepted` **and** `fanout_complete` observed for seven days. All consumers are done; immutable admissions/effects and the engine own replay receipts independently. Returns the namespace's outbox count and ciphertext plus queue-overhead byte charge. |
| `review_owner_outbox` | `acknowledged` observed for seven days; decision no longer pending; subject task freshly confirmed settled for seven days with no unsettled attempt. Finality and operation receipts remain. |
| `terminal_outbox` | `acknowledged` observed for seven days; original task settled for seven days. Pending or unacknowledged commands remain. |
| `terminal_subscriptions`, `terminal_control_acl` | Expiry and original task settlement both more than seven days ago. |
| `terminal_input_receipts` | Task settled for seven days; deadline and last update older than seven days; final disposition `applied`, `delivery_unknown`, `expired` or `rejected`. Pending/injecting input stays. |
| `terminal_operations` | Created more than seven days ago and original task settled for seven days, closing the ended session's replay window. |
| `terminal_sessions` | Last observation and original task settlement older than seven days; all terminal operations, receipts, ACLs, subscriptions and outbox children gone. Task settlement is a conservative bound on session end. |
| `session_inspection_acl` | Expired for seven days; no retained terminal has that session identity. |
| `mailbox_messages` | Message and owning-task settlement older than 30 days, plus a read receipt older than 30 days **or** expired exact-session recipient (released attempt). There is no arbitrary message TTL in the current schema; unread task mail stays. Global-supervisor originals, reserved mail members and associated supervisor replies stay authoritative and are excluded. |
| `mailbox_receipts`, `mailbox_deliveries` | Children of eligible messages, each with its own batch limit. `retention_ready_at_ms` preserves the eligibility decision while child deletion spans multiple passes; every pass still rechecks the task. |
| `intelligence_observations` | Received more than 30 days ago and explicit expiry elapsed; operator observations are eligible directly, worker observations additionally require their original task settled for 30 days. |
| `intelligence_node_snapshots` | Keep the latest per node. The current schema uses `node_id` as its primary key and replaces that row in place, so **every stored row is the latest** and the deletion count is zero. |

Observation ingestion checks `retention_closed_attempts` under its existing node
lock before accepting any old or new packet. Closed attempts cannot recreate
pruned accounting history. Node sequence watermarks, immutable source bindings,
source revocations and transport receipts remain; deletion never resets a
sequence or reuses an observation identity namespace. A newly admitted attempt
gets its own window. Ordinary observation updates/deletes still fail: only the
retention transaction's local setting enables history deletion.

Packet metadata is backfilled from the retained receipt sequence ranges when
migration 0061 is applied. Historical packets did not store their exact quota
charge, so that conservative legacy charge remains reserved; new packets return
their full exact charge. This does not keep their historical row bodies on disk.

Mailbox cursors and immutable operation receipts are retained, so retrying an
expired send does not recreate its body. The job does not prune execution,
settlement, source retention, global-supervisor or other immutable owner records.
Autovacuum remains enabled. Normal periodic `VACUUM (ANALYZE)` can refresh planner
statistics and reuse freed space; this job does not run `VACUUM FULL` or block
normal work to shrink database files.

Run the database fixtures with the URL printed by `scripts/test-db`:

```sh
DATABASE_URL=postgres://postgres:test@127.0.0.1:55432/postgres \
CARGO_INCREMENTAL=0 D1A_CHECK='cargo test --locked --offline -p aq-server --test row_retention' \
D1A_CARGO_LOG=/tmp/dep6-retention-tests.log \
aq test docs/implementation/D1a-checks/run_cargo.py -q
```

The existing AQ pytest subprocess wrapper supplies the test semaphore, two Cargo
build jobs and two Rust test threads. Tests use fake task state and explicit
clock values, with no sleeps, large writes or external services.
