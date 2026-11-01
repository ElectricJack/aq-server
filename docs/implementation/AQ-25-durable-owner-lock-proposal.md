# AQ-25 durable owner and lock-order proposal

Review only, following qualified pure source `10dcfbd` / evidence `5fbfbb4`.
No SQL, migration number, runtime owner, capability or source adapter is added.
Migration38 remains unallocated. AQ-08, WRK-16 and AQ-26 are not complete by
virtue of this proposal. This narrows the next internal-owner slice; Root review
precedes DDL and implementation.

## Existing AQ-08 boundaries actually read

Against agent-q `33dcc41`:

- `execution/authority.rs::load_record_locked` takes node then exact attempt;
  `execution/authority/hot.rs::load_adopted_locked` adds hot row and immutable
  launch/selection joins in that order. This is the current authority owner.
- `agent_access.rs::authenticate_locked` does a bounded credential identity peek,
  invokes that owner, then locks the agent launch/credential row and rechecks
  current expiry/version/binding. `authenticate` wraps this in a short transaction.
- `profiled_execution/producer/publication.rs::publish` performs remote work
  before its serializable publication transaction, locks current node first,
  rechecks runtime/selection/source/time and the common task claim, then writes
  the original offer and its hot/retained state. There is no wait-hold check.
- `producer/redelivery.rs::existing` locks node then chooses/locks an offered
  attempt/hot row. It cannot add a new pre-node gate after that selection without
  restructuring the transaction entry.
- `initial_grant.rs::locked` locks current node, loads current attempt authority,
  checks launch closure and exact original offer, and locks the hot row/material
  owners. `prepare` performs external observations outside that final owner
  transaction. A future hold check must preserve those fences, not replace them.
- `execution.rs::Store::offer` is a legacy fixture publisher with node-first
  ownership and the same unique unsettled-task index. It remains a separate
  path that cannot silently bypass a globally claimed enforced continuation hold.
- `agent_access/messaging.rs::append` owns mailbox head→message/cursor mutation;
  agent send authenticates before calling it. It never needs a wait row or a
  continuation gate. Historical authenticated sender evidence must not cause
  a wait observer to acquire another sender node's live authority locks.

The native and central publication paths are still evolving. Before public wait
registration is enabled, re-inventory every actual initial-offer, redelivery,
initial-grant, central admission and successor path at the final integration
revision. An isolated SQL owner can qualify without pretending that inventory is
already wired.

## Proposed single hierarchy

Keep the existing authority owner's node→attempt/hot order. Insert new wait gates
**before** that chain, never while holding those existing locks:

```text
quota locks (only when reserving/changing quota, installation→project)
  → project wait-graph guard (when changing/checking known dependency edges)
  → task-continuation gate(s), stable lexical order
  → current node
  → exact execution attempt / hot row / immutable authority joins
  → current agent credential binding, as in existing authentication
  → source mailbox head (local mail source only)
  → wait registry row
  → member/progress rows
  → work lease / original operation / outcome / resume-intent leaf rows
```

These are new, narrowly scoped guard records, not the existing project execution
binding row relabelled as a universal early lock. Existing project/selection/
source fences retain their current relative ordering under the authority owner.
No source writer takes project/task/wait guards merely to emit a hint. Do not
introduce a global read mutex, block all source writers on a watermark reader,
or take a second node's authority lock while holding the waiter's node lock.

Initial registration can reuse existing authentication in a first short
transaction, release it completely, then begin the guarded registration
transaction and call current authentication AGAIN after the new gates. Compare
the exact original binding/principal/source-policy revisions; sample time after
all relevant waits. Do not carry a node lock from the preflight into this order.

For an operation-only recovery/inspection lookup, a bounded nonlocking immutable
scope peek can identify its guard route. Lock the authoritative scope/principal
and then the fresh registry/operation row; compare every immutable owner/binding
pin again. Peeked mutable status, deadlines, flags and leases are never verdicts.
No private response is returned from a UUID-only lookup.

The project guard serializes complete known-component cycle checks with adding/
retiring wait edges. It does not authorize a fake complete remote Graph view.
Read the bounded edge set without locking blocker task/node rows. Admission
needs only its task gate and must not acquire the project guard after it; any
path needing both starts with project guard first. Cancellation/retirement must
follow that same order. Source observation completion must choose its full
transaction prelude before taking leaf locks, not upgrade into a project guard
after discovering a terminal result while holding a wait row.

## State/store shape to review before DDL

Use the existing PostgreSQL database and protected-record helpers. Proposed
logical records are:

1. Immutable original operation/request receipt: actual initiating principal,
   exact canonical CON-03 bytes/digest, original binding and policy; stable wait,
   continuation, source-subscription, outcome and resume-intent identities.
2. Registry: immutable creation/deadline/identity anchors; protected bounded
   Snapshot with compare-and-set revision; explicit installing/pending/held/
   terminal projection and due index. Indexed columns must be verified against
   decoded canonical state, not maintained as an independent task-state database.
3. Project cycle guard and task-continuation gate: exact scope/task lineage,
   monotonic hold generation and linked active wait. This gate does not claim a
   slot or imply that the old writer is stopped. Original `execution_attempts`
   ownership stays the source of physical-assignment exclusion.
4. Source-member/work records: bounded indexable source keys and stable install
   identities; original/current owner boundary/snapshot references remain in the
   protected Snapshot. Work has fenced lease, due time and dirty generation.
5. Immutable terminal outcome and resume-delivery intent, inserted atomically
   with the winning registry CAS. One winner per exact wait generation; an old
   intent is retired on task cancellation without rewriting the outcome.

Do not add a second mailbox, Graph event journal, artifact store, task status
projection or ordinary playbook command set. Original/current source evidence
uses references to its owner; the AQ projection digest remains separate from
the owner snapshot hash.

Before DDL, freeze exact serialized-record/receipt maxima and reserve their full
bounded byte/count budget at admission. Progress inside a pre-reserved envelope
must not reacquire an installation-global quota lock on every source poll. A
record exceeding its envelope refuses/holds without truncating identity/evidence.
Quota release/tombstone retention must preserve the documented replay horizon.
The pure256KiB Snapshot cap is not a claim that all extra durable owner records
fit in the same256KiB; total owner accounting needs its own explicit bound.

SQL constraints/guards must enforce immutable pins, monotonic transitions,
single winner and exact receipt/index/projection relationships. Startup must
verify critical definitions, keys, actions, nullability and retained owner
consistency using the established state36/37 patterns; a same-name CHECK(TRUE),
wrong-key index, disabled/re-enabled guard or altered original receipt must not
pass. No repair-on-startup is proposed.

## Transaction/service increments

**First internal owner increment:** no public routes or hold capability. Supply
an opaque verified-registration context through an actual AQ-08 owner/test seam,
persist the pure transition plan and source-install intent with stable IDs, and
return only an internal installing/pending view. A stored gate/hold intent is not
an enforced public hold receipt while scheduler hooks are absent.

**Claim/IO:** claim bounded due work in a short transaction and commit before
calling any source port. A claim transaction holds only its work row, then
releases it; it must never wait for project/node/source authority while retaining
that leaf lock. At most the configured small number of physical calls can run;
admission does not wait in an unbounded semaphore/channel queue. If blocking IO
is used, physical admission remains owned until its callback actually returns,
even if the async waiter/lease expires. Remote install response loss remains
unknown until exact stable-identity lookup/retry, never a second subscription.

**Completion:** reacquire the complete hierarchy, current continuation/source
access and fresh registry/work lease. Sample current time after locks. Apply the
pure plan only if its expected revision/ticket is still current; stale replies
cannot restore an older phase or clear a newer hint generation. Persist exact
owner snapshot reference/hash plus independent projection digest; a source hint
cannot supply either a verdict or authority. Retry/gap/backoff state and next due
remain durable across restart. No transaction is held across source IO.

**Mailbox:** after exact recipient/sender authorization, use a narrow internal
helper next to `append` to lock mailbox head and obtain its committed cursor,
read matching messages and install the source-member boundary in the same AQ
transaction. Head comes before wait/member rows, including response-loss retries.
Message append never locks a waiter. An exclusive message cursor advances only
over retained owner evidence and never acknowledges/read-delivers the message.

**Graph:** the separate committed-observation owner protocol is prerequisite.
Current Graph history/HTTP or AQ-only confirmed-mutation outbox is not sufficient.
All remote calls are outside the AQ transaction. Source-authority/project binding
and exact condition/incarnation/receipt are rechecked before persisting a result.
No AQ+Graph atomic commit, history-gap repair or stable content-only revision is
invented. The current service remains unsupported for real Graph wait admission.

## Actual admission hook is a separate required increment

Offer publication, redelivery and initial grant must take the task-continuation
gate BEFORE current node/attempt/hot locks, recheck it at the effect's real
publication/grant boundary, and refuse a held task. Early candidate/source reads
are only proposals. Redelivery must route to its immutable task before its final
transaction, then revalidate that exact selected offer after the gates; it cannot
switch tasks under an already-held node lock.

This needs entry-point restructuring, not an appended `waits.check()` inside
`authority::load_locked`. Existing node-only renewal/read/physical-recovery paths
must not acquire the new outer gate while already holding their authority locks.
Any such path that truly needs the gate must restart through the approved outer
prelude. Warm-wait authority remains its existing bounded grant; revocation and
physical suspension are separate WRK-16/AQ-26 transitions, not implied here.

Initially no held-task bypass exists. AQ-26 later creates an exact continuation
dispatch generation/winning successor, after real stopped-writer, retention,
effect and eligibility joins; only that reviewed owner may authorize the
corresponding successor through a hold. A resume intent from this registry is
not that authorization. Central supervisors need their real authority adapter;
do not fabricate a remote LaunchBinding or retain supervisor credentials on nodes.

## Disposable actual-owner tests required

- Hold a task gate externally, begin registration after its preflight, wait for
  that gate, and prove the external transaction can NOWAIT-lock the node: no
  node→gate inversion. Repeat at real publication/redelivery/grant hooks later.
- Hold a mailbox head, start registration waiting for it, and prove the waiter
  registry row is not already locked; actual append can commit without acquiring
  any wait gate. No source transaction waits for a worker's round trip.
- Concurrent A→B/B→A registration under the project guard yields one accepted
  known graph and one cycle refusal. Incomplete/deep/oversized proof refuses;
  it cannot declare NoKnownCycle or lock a remote task graph.
- Actual registration/lease/original receipt response loss and restart preserve
  stable IDs, bytes/age/deadline/budgets. Two drivers produce one outcome and one
  resume intent. Newer hints survive old IO completion and stale leaf leases.
- Held source IO leaves all relevant SQL locks free; source incarnation/access,
  task cancellation and original deadline changes are rechecked after IO/locks.
  A deadline reached while waiting cannot become a late satisfied outcome.
- Real mailbox exclusive/thread/sender/recipient and task-versus-session cases;
  missing replies and gaps remain pending with persisted bounded retry.
- Exact startup/corruption/drift/retained-byte checks and quota races, using
  actual disposable PostgreSQL only when approved; no canonical data changes.

Only after these internal owner tests and actual scheduler hooks qualify may the
existing scoped API advertise registration. Public own-agent cancel/warm-result
ack still needs the separately reviewed contract bridge identified in the audit.
