# 0015: Multi-agent worker nodes

Date: 2026-11-17. Server implementation contract; worker qualification is separate.

## Intent and model

Owner: “worker nodes can run more than one agent at a time with more than one work
item at a time.” Also: “I also want to keep two worker nodes on my desktop machine
just for testing, and one on my laptop.” The hierarchy is machine → worker nodes
→ slots. Nodes retain independent credentials, boot identities and Settings;
several nodes on one machine are supported. A node's slots run independent agents.

`worker_settings(kind='node').max_agents` IS the one slot count shared by queued
attempts and hand-started sessions. It ranges from 0 to 1024; zero pauses new
admissions. A server Setting supplies the default for newly enrolled nodes (2).
Migration 0135 fixes existing nodes at 1 (an explicit zero remains zero).
Lowering a count leaves existing execution running and blocks new admissions
until occupancy is below the count; indices above the count cannot take work.
Machine and provider limits continue to apply independently.

## Wire and persistent state

Control request schema 2 receives schema 2 and `slots` with this node's count.
Schema 1 receives the unchanged response, without `slots`. A new worker against
an old server falls back from schema 2 (400) to schema 1 and one slot.

Native documents stay byte-identical. Register carries `aq-node-slots: N`
(canonical decimal 1..1024); poll carries `aq-node-slot: K` (1..1024).
An absent poll header means slot 1. Headers are sent only after v2 control.
Registration's count advertises slot awareness, not authority to raise Settings.
The node registers once per service lifetime with a shared boot. Count changes
may repeat Register with the same identity and a fresh operation id, including
while attempts are held. A zero-count node keeps its registered identity and
recovery loops; if it must register, it advertises at least one slot, but polls
with zero available capacity. The server Setting remains authoritative.

Migration 0135 adds immutable `execution_attempt_slots(attempt_id,node_id,slot)`
next to the sealed attempts table, backfills existing attempts to slot 1, and
stores slot awareness as `execution_nodes.state.slots`. One poll returns at most
one new offer, assigned transactionally to its polling slot. Undelivered offers
are redelivered only to their recorded slot, even when the count is lowered.
Missing slot rows and legacy attempt kinds fail closed across the whole node.
The server-wide default is revisioned and editable through worker management;
new nodes snapshot it so subsequent default changes do not resize existing nodes.

## Stop proofs and the two-writers rule

The unit that must be proven stopped is the attempt's guardian, sandbox and
writers occupying a slot, not its task or the whole node. Reuse still requires
the exact existing stopped receipts (grant, binding, guardian, pane and kernel
wait ECHILD), a validated lost-attempt release, never-started lapse, or lawful
task release. Unknown or lost-but-unreleased attempts hold their own slots.
The SQL slot assignment fence and publication under the enrolled-node lock
prevent another attempt occupying the same slot. The SQL incarnation guard
keeps the old rule for legacy workers; a slot-aware new boot can register while
old attempts remain mapped and held, without releasing any of their slots.
An old boot can deliver terminal evidence through admitted registration history;
it cannot poll, start, renew or fetch launch material. No heartbeat, timeout,
registration or newer inventory is a stop proof. Worker isolation of slot state,
journals, workspaces, Git roots, session hosts, sandbox names and scoped agent
credentials is a required counterpart; the server does not qualify local process
isolation by accepting a registration.

## Answers to the worker's six contract questions

1. Inventory is one serialized stream per node/boot. A slot's poll may name an
   acknowledged sequence at or below the current sequence of that boot; selection
   uses the latest fresh inventory. Execution preparation already checks the
   current inventory and compatibility epoch against the offer, so a sibling's
   compatible refresh does not invalidate it. No worker lock spanning HTTP calls
   is required. An incompatible refresh still refuses execution.
2. Same-boot Register is accepted while attempts are held, including a refreshed
   slot count. Changed operation bytes under the same operation id conflict.
   `execution boot must be registered`, `inventory boot is stale`, or a missing
   registration after server recovery requires node-level registration, never a
   registration from an individual slot loop. Superseded boot identities must
   not be registered again to deliver old evidence.
3. An old server does not need registration per attempt when the same admitted
   boot/runtime remains unchanged. Old workers may continue registering per
   attempt; new workers falling back to schema 1 use one slot without headers.
4. Extra retired node identities recover outstanding attempts using their own
   original credentials. They never poll for new work. Operator removal follows
   recovery and retains attempt history; silence alone does not release compute.
5. Keep two desktop nodes (Local Worker 1 and 2), one laptop node (Local Worker
   3). Retire Local Worker 4..9 after recovery. Each remaining node has its own
   slot count. Do not collapse a machine into a node.
6. Slot headers and v2-only `slots` are the agreed additive contract in
   aq-admission 0.1.13 and aq-protocol 0.1.3. No transport fixture digests change.
   Worker ControlResponse literals need `slots: None` where appropriate.

## Manual sessions and reads

Manual claims and queued publication use the same management admission lock and
node occupancy. A manual worker's legacy `max_slots` remains parseable but cannot
raise or lower the node Setting. Pending sessions occupy no slot; claimed sessions
occupy capacity until stopped/failed, including stop uncertainty. Manual allocation
must reserve a concrete free index, so queued polls cannot collide with it.
Swarm reports one `slots: {total,occupied,idle}` per node and a slot index on each
agent row. Proven-stopped attempts awaiting settlement retain task ownership,
not compute occupancy. Idle saturates at zero when a count is lowered below use.

## Rollout and operation

Upgrade the server first, then the worker. Existing workers remain on slot 1;
raise a node's count after its slot-aware worker is installed. Save `max_agents`
through `/api/worker-management/node/{id}` using operation id and expected
revision. Changing the server default affects new enrollments only. A removed
node is held from new work; retain its credential until outstanding attempts
have been individually resolved. Recovery and deletion of local state remain
worker responsibilities. No live-stack rollout is part of this implementation.
