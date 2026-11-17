# Multi-agent node server implementation — 2026-11-17

Verification is in progress. The final validation section must be completed before
this document is used as a completion claim.

## Repository state

Server branch: `feat/multi-agent-node`, base/HEAD `ae88f71a` (wire contract).
Worker wire commits: `d30c933`, `fb9f19d` (confirmed by read-only Git log).
No worker files were edited in this run; worker behavior changes belong to its engineer.
Every requested per-step staging/commit was attempted with explicit paths. All
failed because the sandbox mounts the shared Git worktree metadata read-only:
`Unable to create .../.git/worktrees/multi-agent-node-server/index.lock`.
All server implementation changes therefore remain uncommitted. Nothing was pushed.
No Docker command, test-db script or live demo endpoint was used.

## Steps and implementation

| Brief step | Implementation | Evidence |
| --- | --- | --- |
| 0 | Spec, owner intent, hierarchy, count/state/wire, six contract answers, rollout | `docs/specs/0015-multi-agent-nodes.md:1` |
| 1 | Shared count, migration/backfill, snapshotted default 2, revisioned default/node management, v2-only control count | `crates/aq-server/migrations/0135_node_slots.sql:2`; `crates/aq-machine-control/src/slots.rs:15`; `crates/aq-manual-sessions/src/management.rs:49`; `crates/aq-server/src/machine_api.rs:101` |
| 2 | Typed compute proof collection, per-slot assembly, publication/allocation under management+node locks, redelivery to recorded index | `crates/aq-server/src/profiled_execution/compute.rs:15`; `crates/aq-server/src/profiled_execution/producer/workflow.rs:360`; `crates/aq-server/src/profiled_execution/producer/publication.rs:15`; `crates/aq-server/src/profiled_execution/producer/redelivery.rs:9` |
| 3 | Slot-aware boot registration with mapped outstanding attempts, current-only launch rights, original boot history for terminal evidence, exact startup guard checks | `crates/aq-server/src/profiled_execution/registration.rs:35`; `crates/aq-server/src/profiled_execution/closure/history.rs`; `crates/aq-server/migrations/0135_node_slots.sql:132`; `crates/aq-server/src/store/compute41.rs` |
| 4 | Manual claims reserve a concrete index from the same count; worker max_slots cannot override Settings; lowering retains running owners | `crates/aq-manual-sessions/src/worker.rs:117`; `crates/aq-machine-control/src/management.rs:84`; `crates/aq-machine-control/src/slots.rs:124` |
| 5 | Authoritative total/occupied/idle; native agent slot; additive dashboard schema/fixture and worker-machine notes | `crates/aq-swarm/src/management.rs:6`; `crates/aq-swarm/src/agents.rs`; `crates/aq-dashboard-contract/src/swarm_contract.rs:210`; `protocol/dashboard-read/v1/schema.json` |
| 6 | Per-node resizing/defaults, retirement of desktop nodes 4..9, server-first upgrade, retaining desktop nodes 1 and 2 and laptop node 3 | `docs/operations/multi-agent-nodes.md:1` |

The compact authority type moved unchanged from aq-server into
`crates/aq-profiled-state/src/hot.rs`; aq-server reexports it. This keeps new
allocation/storage logic in small crates and the aq-server lib at its 105,127-line
ratchet, with 75 integration binaries (unchanged).

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

Pending final results. Logs under `/tmp/node-*.log` contain actual commands/results.

Observed intermediate results:

- First registration run: 1 passed, 5 startup failures (`native compute41 guard
  drift`); fixed version-aware startup validation, then 6/6 passed.
- Core run: 10/11 passed; the synthetic runtime lacked HostStopEvidenceV1. Fixed
  fixture support; rerun 11/11 passed, including old-boot stopped evidence.
- Touched crates: 61/61 passed before adding two small slot unit regressions.
- Broad targeted run first hit two new-test compile errors (private pool access,
  &String/&str mismatch), fixed. Subsequent run: 43 passed, two handoff fixture
  failures, 91 unrun due to fail-fast; fixed duplicate feature insertion.
- Migration catalog reports 135; Python catalog regressions: 2 passed.
- Build budget: 105,127 server lib lines, 75 integration binaries, pass.

## Worker qualification still required

The server tests use synthetic host/process observations, not physical model
execution. The worker engineer must verify per-slot local isolation, one shared
boot and serialized inventory sequence, v2 control negotiation and legacy fallback,
same-boot resizing without killing running work, independent old-boot recovery,
and manual/native execution respecting the same reservations without a node-wide
busy pause. Retired old identities must report with their own credentials and retain
protected state until resolved. No assembled server+worker or live rollout is claimed.
