# Node row locks

Status: implemented by migration 0185 (2026-11-26).

## Problem

Every worker request checked its credential with
`SELECT … FROM nodes … FOR UPDATE` and held that row lock across the rest of
its transaction. About 70 sites did this. Worker requests of one node
therefore excluded each other, and so did the heartbeat
(`UPDATE nodes SET last_seen_at`). Inventory-v2 answered 503
`row lock contended` under its 150 ms lock timeout.

Publication also took the global advisory lock 110110 and counted the node's
occupied slots through `native_attempt_holds_compute`. For a finished
attempt that still held its task, that function re-ran the stop proof each
time, about 4 to 10 ms per attempt, so the count took 48 to 76 ms while both
locks were held.

## Design

- **`nodes` is the credential row.** `worker_api::lock_current_node`, the
  only helper, checks it `FOR KEY SHARE`. That mode is compatible with
  itself, with heartbeats and with every other non-key update.
  `read_current_node` (FOR SHARE) is folded into it.
- **Revocation** sets `revoked_at` under `FOR NO KEY UPDATE`, which does not
  wait for KEY SHARE checks. It commits at once, and from then on no check
  passes, so a stream of checks cannot hold it off. After the commit,
  `aq_machine_control::drain_revoked` waits for the checks that passed
  before it. It takes the rows `FOR UPDATE` with a bounded wait
  (`revoke_drain_wait_ms`) and retries (`revoke_drain_tries`). It then
  applies the revocation's effects again (permit revocation,
  `release_revoked`) for anything those transactions committed in the
  meantime. Nothing rotates a credential, so revocation is the only
  `FOR UPDATE` on `nodes`. Row-retention sweeps keep theirs as well (see the
  audit).
- **`node_allocation(node_id, generation)`** gives each node one row,
  created by `initialize_node_slots`. Work that must exclude other work on
  the node locks this row `FOR UPDATE`: slot allocation and publication,
  manual-session claims, runtime registration and boot changes (including
  delegated register and readopt), the host shell, and the legacy fixture
  protocol. When the node's machine has a maximum, `allocation::lock` locks
  every row of that machine in id order. This replaces the global advisory
  lock 110110. Owner fences (Stop, Drain, availability, Settings saves) lock
  the affected rows in id order and bump `generation`. A SERIALIZABLE
  publication that read the old state then fails to serialize.
- **Inventory** has no exclusive lock. Its sequence fence is the
  `intelligence_node_snapshots` upsert:
  `DO UPDATE … WHERE revision < excluded.revision AND (boot differs OR
  sequence < excluded.sequence)`. If no row is written, the request gets
  `changed()`.
- **`execution_attempt_compute(attempt_id, node_id, released)`** stores
  `NOT native_attempt_holds_compute(...)`. A trigger recomputes it when any
  table the proof reads is written, after locking the row. The proof is
  unchanged and runs once per write. Capacity, slot views, the slot guard,
  the Swarm, the dashboard and manual sessions read the stored value as
  `holds_task AND NOT EXISTS(released row)`. A held attempt without a row
  (fixtures that write past triggers) therefore still counts, so the check
  fails closed. That read uses the primary key, so there is no partial index
  on unreleased rows.
- **Limits are Settings** (`[workers.limits]`):
  - `reserved_acquire_timeout_ms` (250), `reserved_statement_timeout_ms` (750),
    `reserved_lock_timeout_ms` (150) and `reserved_idle_timeout_ms` (2000)
    bound the renewal and presence pools.
  - `inventory_freshness_ms` (2000).
  - `owner_lock_timeout_ms` (1000) replaces the `OWNER_LOCK_TIMEOUT` constant.
  - `revoke_drain_wait_ms` (1000) and `revoke_drain_tries` (10).

## Lock order

The lock order is: owner or service locks, then `node_allocation`, then
`nodes` KEY SHARE, then attempt or session rows, then the registration row
(`execution_nodes`). The `nodes` KEY SHARE waits only for a revocation drain,
which holds nothing else, so its place in this order cannot deadlock.

Registration locks the held attempts before `execution_nodes FOR UPDATE`,
because other worker requests lock their attempt before they read the
registration `FOR SHARE`. Redelivery and publication read the registration
before they lock an attempt. Publication holds `node_allocation`, as
registration does, so the two cannot interleave. Redelivery can still meet a
boot-changing registration in the opposite order. PostgreSQL then aborts one
of them with 40P01, which the worker retries.

## Audit

The audit covers every caller of `lock_current_node` and every raw
`FROM nodes … FOR UPDATE` site at the time of the change. For each one it
records what the node lock serialised beyond the credential check, and the
lock that now covers it.

| Caller | What it needs beyond the credential | New lock |
| --- | --- | --- |
| `execution/authority.rs` `node_revoked` (under `load_locked` and `load_attempt_locked`, about 25 sites) | nothing; the attempt and hot rows order the work | KEY SHARE + attempt (already) |
| `delegated_permits/runtime.rs` `lock_owner`, `storage.rs` `insert_locked` | revocation; the boot read | KEY SHARE + attempt (already); `boot()` reads `delegated_node_boots`/`execution_nodes` FOR SHARE |
| trigger `delegated_permit_guard`, SQL `profiled_close_ungranted` | an upgrade from the caller's lock would deadlock | KEY SHARE (redefined in 0185); attempt row (already) |
| `phase_terminal/common.rs`, `lost.rs`, `results.rs`, `source_api.rs`, `initial_grant.rs`, `preparation.rs`, `material_api.rs`, `stopped.rs`, `artifact_api.rs`, `started.rs`, `phase_execution/pinned.rs`, `delegated_renewal.rs`, `login_refresh/api.rs`, `owner_replay_fences.rs`, `closure/service.rs`, `accepted_preparation/*`, `renewal/pinned.rs`, `presence/report.rs`, `task_workspace_delivery/node.rs`, `lifecycle_artifacts.rs`, `source_retention*`, `agent_access.rs` (issue, revoke), `messaging.rs`, `delegated_permits/incarnation.rs` renew, `cancellation.rs`, `no_grant.rs`, `disposition.rs`, `lapse.rs`, `replacement_sql.rs`, `aq-manual-sessions` conversation | attempt-scoped work. Replays of `(node, operation)` receipts are unique keys; quotas have their own advisory locks or SERIALIZABLE | KEY SHARE + attempt/session row (already) |
| read-only sites (`cancellation.rs` pending, `phase_terminal/history.rs`, `reconciliation`, `workflow.rs` gates, `git_grant` list/proxy/bundle, `source_retention/base.rs`, `delegated_permits/receipts.rs`, `machine_api.rs`, `test_support`, `terminal/peer.rs`, `project_bindings.rs` fence, `manual_agents/session_login.rs`, `intelligence/inventory.rs` (registry singleton orders it), `intelligence/feedback.rs`, `observations.rs`, `diagnostics.rs` poll, manual `logins.rs` request) | nothing | KEY SHARE only |
| `registration.rs` `register_locked`, `delegated_permits/incarnation.rs` register and readopt | the replay, the receipt count, the `execution_nodes` write, the held-attempt check against new claims, and the boot switch | `node_allocation` FOR UPDATE; held attempts before `execution_nodes` |
| `registration::current_runtime_locked` | the boot was compared without a lock | one `execution_nodes` FOR SHARE read |
| `producer/publication.rs` via `lock_publishing_node` | count-then-insert of slots; fences | `node_allocation` FOR UPDATE (the whole machine under a machine maximum) |
| `producer/redelivery.rs` | owner fences; the shared-then-exclusive retry loop existed only for the node lock | KEY SHARE only; loop removed. It claims nothing new, and an offer redelivered while a Stop commits is an attempt that Stop already hands off |
| `presence/inventory.rs` (both transactions) | per-node revision and sequence; nothing locked the first-ever row | KEY SHARE; the fence is the snapshot upsert; the semantic head row is locked (already) |
| `git_grant/workspaces.rs` enroll | ordered the attempt's concurrent enrollments | attempt FOR UPDATE (added) |
| `agent_access/mutations.rs` recovery | per-attempt cursor cap; the page's `operation_seq` bound | attempt FOR UPDATE (added) |
| `observations/vm_time.rs` | first insert, replay and stop of one attempt's VM time | attempt FOR UPDATE (added) |
| `observations/transport.rs` `boot()` | the boot read was covered only by the node lock | `execution_nodes` FOR SHARE |
| `execution.rs` `lock_node` (legacy fixture protocol) | rewrites the node's offers and registration without row locks | `node_allocation` FOR UPDATE + KEY SHARE |
| `manual_agents/logins.rs` material | the machine must be idle | every `node_allocation` row of the machine |
| `aq-manual-sessions` worker poll, `api.rs` reserve and slots, `logins.rs` request, `diagnostics.rs` request | slot count-then-insert; machine maximum; per-node checks that had been under 110110 | `node_allocation` (`allocation::lock`) + KEY SHARE |
| `aq-manual-sessions` `host.rs` open | find-or-insert of the node's one active shell | `node_allocation` FOR UPDATE + KEY SHARE |
| `aq-manual-sessions` `management.rs` save (node, machine, default) | Settings fence against allocators; idempotency | `allocation::fence` of the target's nodes (all nodes for the default); retiring a machine drains its revocation |
| `aq-machine-control` `set_desired`, `set_owner_availability` | the fence against publication | `allocation::fence` (generation bump) in id order |
| `aq-machine-control` `revoke_machine`, `store.rs` `revoke_node`, retire machine | revocation | `FOR NO KEY UPDATE`, then `drain_revoked` (FOR UPDATE, bounded and retried) |
| `aq-machine-control` `claim_key` | relied on node exclusion against a second claim by the same node | `UPDATE … WHERE machine_key IS NULL` with a row-count check |
| trigger `node_slot_assignment_guard` | node maximum and occupancy against Settings | `node_allocation` FOR UPDATE; stored release count (0185) |
| `aq-row-retention` observation sweeps | per-node ledger against ingestion, which reads the fence before its own row lock | kept `nodes` FOR UPDATE; a sweep is short and conflicts with ingestion's KEY SHARE as before |

## Remaining

- `compute::busy_locked` still validates each held stop in Rust for the
  typed witness that publication and registration present to
  `native_compute_reuse_proven`. The capacity count no longer does.
- Two requests that reuse one operation id at the same moment now race to the
  receipt's unique key. The loser gets a database error instead of a replay.
