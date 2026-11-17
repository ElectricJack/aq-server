# Multi-agent node server implementation — 2026-11-17

Server implementation steps 0–6 are complete. Build, clippy, formatting, focused
verification, protocols, catalog and budgets passed. No physical worker or live
rollout is claimed. The original full-run failure and its correction are recorded below.

## Repository state

Server branch: `feat/multi-agent-node`. Wire commit: `ae88f71a`.
Implementation checkpoint: **`f882225b898cdcf7b70f9bc26df0453c38856b03`**, created
by the owner outside these tool calls while verification was running; source was
clean at that checkpoint. Worker wire commits: **`d30c933`, `fb9f19d`**, confirmed
by read-only Git log. No worker files were edited in this run.

Every per-step staging/commit was attempted with explicit paths. All failed with
`Unable to create .../.git/worktrees/multi-agent-node-server/index.lock` because
the sandbox mounts that metadata read-only. The owner's checkpoint rescued all
implementation changes. The final documentation staging/commit was also blocked;
only report.md and progress.md remain uncommitted after that checkpoint.
Nothing was pushed. No Docker, test-db script or live demo endpoint was used.

## Steps and implementation

| Brief step | Status | Implementation | Evidence |
| --- | --- | --- | --- |
| 0 | Done | Spec, owner intent, hierarchy, count/state/wire, six contract answers, rollout | `docs/specs/0015-multi-agent-nodes.md:1` |
| 1 | Done | Shared count, migration/backfill, snapshotted default 2, revisioned default/node management, v2-only control count | `crates/aq-server/migrations/0135_node_slots.sql:2`; `crates/aq-machine-control/src/slots.rs:15`; `crates/aq-manual-sessions/src/management.rs:49`; `crates/aq-server/src/machine_api.rs:101` |
| 2 | Done | Typed compute proof collection, per-slot assembly, publication/allocation under management+node locks, redelivery to recorded index | `crates/aq-server/src/profiled_execution/compute.rs:15`; `crates/aq-server/src/profiled_execution/producer/workflow.rs:360`; `crates/aq-server/src/profiled_execution/producer/publication.rs:15`; `crates/aq-server/src/profiled_execution/producer/redelivery.rs:9` |
| 3 | Done | Slot-aware boot registration with mapped outstanding attempts, current-only launch rights, original boot history for terminal evidence, exact startup guard checks | `crates/aq-server/src/profiled_execution/registration.rs:35`; `crates/aq-server/src/profiled_execution/closure/history.rs:151`; `crates/aq-server/migrations/0135_node_slots.sql:139`; `crates/aq-server/src/store/compute41.rs:16` |
| 4 | Done | Manual claims reserve a concrete index from the same count; worker max_slots cannot override Settings; lowering retains running owners | `crates/aq-manual-sessions/src/worker.rs:117`; `crates/aq-machine-control/src/management.rs:84`; `crates/aq-machine-control/src/slots.rs:124` |
| 5 | Done | Authoritative total/occupied/idle; native agent slot; additive dashboard schema/fixture and worker-machine notes | `crates/aq-swarm/src/management.rs:6`; `crates/aq-swarm/src/agents.rs`; `crates/aq-dashboard-contract/src/swarm_contract.rs:210`; `protocol/dashboard-read/v1/schema.json` |
| 6 | Done | Per-node resizing/defaults, retirement of desktop nodes 4..9, server-first upgrade, retaining desktop nodes 1 and 2 and laptop node 3 | `docs/operations/multi-agent-nodes.md:1` |

Placement applies the same Setting to both the polling node and eligible peer
nodes; a stale backend advertisement cannot introduce a second slot count. The
peer-count read is batched (`crates/aq-machine-control/src/slots.rs:224`).

The compact authority type moved unchanged from aq-server into
`crates/aq-profiled-state/src/hot.rs`; aq-server reexports it. This keeps new
allocation/storage logic in small crates and the aq-server lib at its 105,119-line
ratchet (lowered by eight), with 75 integration binaries (unchanged).

## Implemented wire

- Control schema 1: unchanged response, no `slots`. Schema 2: schema 2 response,
  `slots: 0..1024`, from this node's Setting. Default changes affect future
  enrollments only. An old-server fallback uses one slot and no slot headers.
- Native register: canonical optional `aq-node-slots: 1..1024`. Presence enables
  slot-aware boot admission; advertised count never raises Settings. Same-boot
  count refreshes work under load. Replaying an operation with a different header
  conflicts. A zero-count node can retain/recover its boot while admitting no work.
- Native poll: canonical optional `aq-node-slot: 1..1024`, absent means 1.
  Legacy registrations cannot take slot 2. At most one offer per poll, persisted
  to its index, and redelivery cannot cross indices. Redelivery bypasses the
  count gate for an existing reservation but retains stop/drain/retirement holds.
- Inventory: one serialized node/boot stream; a poll can name an acknowledged
  sequence no greater than the current one and uses the latest fresh snapshot.
  Existing execution compatibility/epoch checks remain in force.
- Manual request shape unchanged; max_slots is still parseable but does not
  govern count. Admitted manual sessions pin `spec.node_slot` (one-based logical
  shared allocation). The worker must coordinate manual and native execution.
- Swarm native agent rows gain optional `slot` (1..1024). Node occupancy includes
  claimed manual sessions and native compute, including unknown owners; proven
  stopped attempts may still appear as finishing/task-held rows without using
  compute. Legacy fixtures without mappings fail closed for new allocation.
- Native transport schemas/fixtures and wire crate/vendor source bytes unchanged
  from the existing committed wire contract.

## Two-writers argument (five sentences)

Each native reservation identifies one attempt's guardian, sandbox and durable
node slot, while a manual claim reserves an index in the same allocation space.
Publication and manual admission serialize on the shared admission/node locks,
and the SQL allocation fence refuses reuse until that slot's prior attempt has
its original typed stopped evidence plus SQL witness, validated lost release,
never-started closure/release, or lawful task release.
A new boot cannot clear or move an outstanding reservation, and only the current
boot can poll, start, renew or fetch launch material.
Terminal evidence remains bound to the exact original attempt, grant, owner and
registration history, so a late or duplicated report for A cannot modify B.
Provided the worker isolates each slot's journals, workspaces, Git roots/sockets,
sandbox names, session host and scoped agent credentials, changing the admission
unit from node to slot preserves the original compute-reuse guarantee; server
fixtures do not qualify that physical worker isolation.

Backing tests: `native_offers::node_slots::*`,
`native_registration::queued_and_manual_allocation_share_one_count_and_unknown_holds_only_its_index`,
`native_registration::two_nodes_on_one_machine_each_reserve_three_slots_and_refuse_a_seventh`,
and the existing `phase_results::{frozen,refresh}::compute::*` exact stop-proof tests.

## Migration

Only new migration 0135. Existing nodes become count 1 (explicit zero stays zero),
including nodes that previously allowed additional manual sessions. New nodes
snapshot the default, initially 2. Durable attempt/manual slot maps are separate
from sealed attempts, immutable while their owners exist and cascade with owner
retention. Allocation fencing and the rewritten incarnation guard share exact
compute proof predicates. Startup validates the new guards alongside existing
compute/settlement guard profiles. No live database was migrated.

## Validation

Every test command used `CARGO_BUILD_JOBS=8`; all nextest commands used `-j 8`.
Every test command had `timeout 900`, except the requested full run's 2700.

| Command | Actual result |
| --- | --- |
| `timeout 2700 cargo nextest run --workspace --locked --no-fail-fast -j 8` | **2494 passed, 1 failed, 41 skipped**; compile 8m04s, execution 660.330s. Failure: `scoped_presence_inventory::swarm_reads_never_block_inventory_v2_on_the_node_row`, only one successful Swarm read during its writes. |
| `timeout 900 cargo nextest run --workspace --locked -j 8 -E 'binary_id(aq-server::it_native) & test(=scoped_presence_inventory::swarm_reads_never_block_inventory_v2_on_the_node_row)'` | Reproduced 0/1 before fix; **1/1 passed** after the occupancy fast path (3.336s). |
| Final touched-module/crate command below | **232/232 passed**, 152.954s, after the read fix. |
| Final placement/core command below | **31/31 passed**, 9.104s, after applying Settings to placement peers too. Includes the new count-one/lowering/restart cases and the previously failing read test. |
| `cargo build --workspace --locked` | **Passed**, 2m53s. |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | **Passed**, 4m52s. |
| `cargo fmt --all --check` | **Passed**. |
| `python3 scripts/build-budget.py` | **Passed**: 75 integration binaries, 105,119 server lib lines; ratchet lowered from 105,127. |
| `python3 scripts/migration_catalog.py`; `python3 scripts/test-migration-catalog.py` | **135**; **2/2 passed**. |
| `python3 protocol/dashboard-read/v1/check.py`; `python3 protocol/worker-machine/v1/check.py` | **Passed** fixtures and invalid cases. |
| `cargo run --locked -p aq-dashboard-contract --example dashboard_contract` | Generated the additive schema successfully. |

Touched-module/crate command:

```sh
CARGO_BUILD_JOBS=8 timeout 900 cargo nextest run --workspace --locked --no-fail-fast -j 8 -E 'package(aq-machine-control) | package(aq-manual-sessions) | package(aq-profiled-state) | package(aq-swarm) | package(aq-dashboard-contract) | (package(aq-server) & (test(/^native_offers::/) | test(/^native_registration::/) | test(/^scoped_presence_inventory::/) | test(/^phase_results::/) | test(/^manual_agents::/) | test(/^worker_api::/) | test(/^migration_catalog::/) | test(/^dashboard_swarm::/) | test(/^dashboard_aggregation::/)))'
```

Placement/core command:

```sh
CARGO_BUILD_JOBS=8 timeout 900 cargo nextest run --workspace --locked --no-fail-fast -j 8 -E '(package(aq-server) & (test(/^profiled_execution::placement::tests::/) | test(/^native_registration::/) | test(/^native_offers::node_slots::/) | test(=scoped_presence_inventory::swarm_reads_never_block_inventory_v2_on_the_node_row))) | package(aq-machine-control)'
```

The full-run failure was reproduced alone, then corrected: active/offered attempts
now return occupied before planning historical stopped-proof joins. Both existing
proof kinds require hot status Finished; finished attempts retain the exact old
proof checks. The concurrency test passed alone and in both subsequent focused
runs, so the original failure is **not** dismissed as a flake. No 53300 failure
occurred. No second full run was performed; the report does **not** claim the
full run was green.

Earlier intermediate failures were corrected: startup expected the old incarnation
guard body; a synthetic runtime omitted HostStopEvidenceV1; the old handoff fixture
then added that feature twice; two new API-test compile errors used a private pool
and mixed &String/&str. Logs: `/tmp/node-{full-tests,read-fastpath,final-focused,placement-final,build-final,clippy-final,fmt-final}.log`.

## Worker qualification still required

The server tests use synthetic host/process observations, not physical model
execution. The worker engineer must verify per-slot local isolation, one shared
boot and serialized inventory sequence, v2 control negotiation and legacy fallback,
same-boot resizing without killing running work, independent old-boot recovery,
and manual/native execution respecting the same reservations without a node-wide
busy pause. Retired old identities must report with their own credentials and retain
protected state until resolved. No assembled server+worker or live rollout is claimed.

The dashboard consumer must re-pin the additive dashboard-read contract before rollout.
