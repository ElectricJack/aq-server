# AQ-25 verified-wait foundation: source audit and first slice

Proposal against agent-q `33dcc41`; no implementation, schema number, SQL,
runtime capability, migration or live-agent operation is introduced. AQ-25 stays
planned/partial until its real scheduling and source-owner joins qualify.

Build a small deterministic wait reducer and a durable AQ registry around the
existing CON-03 types. First qualify registration/reobservation/restart correctness
with fake source owners, then a real mailbox/timer adapter. Keep real Graph wait
admission unavailable until its owner publishes a proven committed observation
boundary. A wait outcome must never stand in for physical suspension or execution
admission. AQ-08, WRK-16 and AQ-26 remain independent integration requirements.

## Audited reuse and gaps

| Owner/source | Existing fact to reuse | Boundary or missing work |
| --- | --- | --- |
| `aq-lifecycle-api/src/waits.rs`, `lib.rs` | Closed conditions; single/all/any, max32 members; exact scope/binding/generations; SourceBoundary; deterministic outcome validation; separate capabilities | Runtime authentication, registry, subscriptions and source verification absent. Fake `ConformanceApi` is not a production engine. |
| `aq-admission/src/transport.rs` | `lifecycle_binding(ProfiledOffer)` and `task_lineage_id` derive exact binding/lineage from the retained offer | Never reconstruct lineage from agent-access JSON or accept agent-selected lineage. Reuse current AuthorityView/retained-offer joins. |
| `agent_access/messaging.rs`, migration0005 | `append` locks mailbox head, inserts message and increments next cursor in the SAME transaction; task versus exact-session recipients; sender and original operation receipt | Useful committed mailbox boundary; no wait subscription/helper exists. Inbox transport receipts are not source outcomes or bootstrap acknowledgments. |
| `agent_access/graph.rs`, `scheduling.rs` | Scoped node/edge validation, bounded/versioned HTTP, exact task version/content revision and observation stamp | Ordinary HTTP stamp is not a committed replay boundary or durable subscription. No Graph state is copied into an AQ task database. |
| `agent_graph_operations`/outbox, migration0007 | Immutable original graph-operation intent and confirmed receipt/outbox | Covers AQ mutations only; cannot observe every direct Graph/human writer. BIGSERIAL operation sequence is not committed order. |
| aq-graph `225329c`, `store.rs`/`service.rs`, migration0001 | Authoritative nodes/edges, historical revisions, lifecycle history and bounded reads | History sequence is allocated identity; exposed read/history APIs have no proven committed subscription barrier. Polling can miss completion→reopen. Graph owner lane is proposing an additive observation protocol. |
| `reviews.rs`, `playbooks/service.rs` and driver | Real decision completion owner/outbox; existing bounded copy/ack/fanout pattern | Decision owner completion can later be a source port. Do not consume/ack the playbook's event delivery on behalf of waits, or treat its outbox sequence as committed source truth. |
| source-retention, profiled source exports/results | Actual retained receipt, source and effect owners | Checkpoint claim acknowledgment is not retention. Waits store references; they do not build a second source/artifact store. |
| `execution/authority.rs`, profiled initial grant and execution task claim | Current node/attempt/grant fences and one unsettled physical task assignment | No durable continuation/wait scheduling hold is installed. A wait must not keep a live grant/node slot just to remember a sleeper. |

The bounded pinned v1 comparison uses `agent-queue2` at
`9a9b8e8ef992d1ff699755a9383ed3c91e2844ea`: registry delegates to typed
adapters; registration subscribes then reobserves; dispatcher rechecks source
and owner rather than trusting events; its source allocator/watermark uses a
writer fence and READ COMMITTED. Retain those obligations, not v1's shared
task/source SQL or global graph lock across remote calls. V1 tests are references,
not executed v2 proof. Exact paths and hashes are in the accompanying audit JSON.

Two frozen-contract limits must stay explicit. `CancelWait` is currently an
operator-only action; do not give agents the operator capability to implement
own-wait cancellation. `ContinuationAck` acknowledges a successor attachment,
not a warm wait result. A later additive, versioned agent-own cancel/ack bridge
needs its own review. Existing `Inspect` supports the first read/explain seam.

## Proposed first implementation slices

1. **Pure reducer/source conformance**: same-workspace `aq-wait-engine`,
   `publish=false`, using existing `aq-lifecycle-api`, serde and existing error/
   digest helpers. No async runtime, database, graph client or new external
   dependency. Inputs are validated CON-03 requests plus trusted source-owner
   observations; time and IDs are supplied. Outputs are bounded transition plans.
   Do not change the frozen lifecycle archives/API or copy its fake service as
   the production registry. No JSON endpoint accepts caller-supplied verdicts.
2. **Durable owner**: `aq-server/src/waits/{owner,store,driver,sources}` with
   immutable intent, continuation-hold intent, bounded install/recheck work,
   source boundaries, outcomes and resume intents in PostgreSQL. Exact owner
   transactions/CAS and disposable SQL fault tests are a separately reviewed
   schema increment; **no number is allocated by this proposal**. First use
   trusted test authority/fake-source ports; expose no public runtime capability.
3. **Concrete mailbox/timer**: owner-internal transactional mail observation and
   persisted clock due jobs. Actual message insertion/registration races prove
   this adapter. Graph owner protocol and AQ-08 hold enforcement integrate in
   their own qualified slices; only then mount scoped agent registration.

An internal prototype may return `Installing`/`RecoveryHeld` status. It cannot
emit public `WaitRegistered` or a claimed enforced central-hold receipt before
the actual AQ-08 admission hook is wired. A stored hold intent is not proof that
every admission path checks it. No hibernation, wake scheduler, checkpoint upload,
playbook command, dashboard surface or external notification is part of slice1.

## Identity, storage and transaction proposal

Persist canonical CON-03 RequestBody bytes/digest and original operation ID,
authenticated original principal/binding, wait and continuation generations,
resolved policy reference/body digest, original creation time, derived immutable
deadline, source access anchors and retained-byte charge before external work.
The protocol digest is canonical typed content; whitespace is not silently made
a second, conflicting idempotency protocol. Closed/duplicate-field JSON parsing
still precedes decoding. Changed logical payload/owner with the same operation
ID conflicts; a replay returns its original receipt without resetting age,
deadline, retry budget or source cursor. Status is a separate current view.

Proposed logical storage records (not DDL):

- Operation/registration: immutable request, original actor, scope, generations,
  policy and timestamps; monotonic revision/phase and retained original response.
- Task continuation gate: one active wait/continuation per exact task lineage,
  pointer to this registry record and monotonic hold/admission generation. This
  is not a worker capacity reservation or another copy of task state.
- Member subscription: exact source key/incarnation, condition digest, original
  cursor/revision, stable subscription ID, owner receipt/snapshot digest, replay
  coverage and next recheck state. No message body or graph task payload mirror.
- Work: bounded, leased installation/reobservation jobs with fence generation,
  persisted retry due/attempts and dirty-through hint generation. Leases never
  make late IO valid; a result must still match current binding/lease/revision.
- Terminal outcome and delivery/resume intent: one immutable winner per wait
  generation. Outcome plus idempotent intent commit together. AQ-26 consumes
  that intent; its existence is not `running` or an execution grant.

Small bounded evidence bodies can live with their registry receipts; source,
summary/checkpoint and process artifacts stay with existing retention owners.
Use existing protected-storage helpers when adding SQL storage, not another
encryption format or artifact database. Terminal tombstones/operation identity
survive the supported replay window; quota exhaustion refuses new registration
rather than deleting identity and allowing old requests to resurrect a wait.

Registration/cancel/outcome/admission serialize through one task-continuation
gate and a bounded project wait-graph revision. The actual lock hierarchy must
be reviewed with AQ-08's node→attempt/hot ownership before SQL is authored.
Do not add an opposite wait→node edge or hold graph/project/wait SQL locks while
performing remote source IO. Source writers must not acquire a wait lock merely
to append their source fact. Local mailbox completion takes mailbox authority
before wait rows; job claiming commits/releases its wait lock before any IO.

For detached hibernated waits, use a retained, explicitly scoped continuation
authority derived at registration, rechecked against current project/service
binding and task cancellation before/after source IO. Do not persist a worker
token or require its live execution grant to remain valid while sleeping. This
retained scope grants only source observation/continuation coordination; it
cannot authorize graph mutations or execution. A revoked/rebound project/source
principal becomes unavailable/held, never success. Human inspection must use the
current full authenticated principal and normal visibility checks.

## Registration and source protocol

Source ports are trusted daemon adapters, not a dynamic command/event registry.
Each declares supported closed conditions and its qualified observation protocol:

```text
prepare(scope, exact condition, stable subscription identity)
  -> committed subscription+snapshot proof | typed refusal/unavailable
replay_or_reobserve(scope, stored boundary)
  -> owner observations + safe coverage boundary | explicit replay gap
```

Ports return exact condition/source identity, incarnation, revision, immutable
evidence reference/digest and coverage. SourceBoundary alone is a record, not a
cryptographic authority proof; adapters qualify their real owner protocol and
verify the returned binding. Agents can submit only the frozen condition.

1. Validate identity/capability, scope, known source kind, static limits and
   explicit current operation/run self-waits. Resolve/pin policy; check known
   cycles under a serialized bounded wait-graph revision.
2. Commit registration/hold intent and deterministic member install identities.
   Remote subscribe retries/reconciliation use those SAME identities after
   response loss. No success receipt claims a subscription that is still unknown.
3. Acquire source-owner snapshot/subscription proof. Persist that exact boundary
   and unconditionally reobserve authoritative source state after installation.
   A resolution before/during installation returns verified `WaitOutcome` without
   forcing an unnecessary suspension once hold enforcement is integrated.
4. Recheck current registry/continuation generation, project/source access anchor,
   cancellation and policy after IO and under the final AQ transaction. Commit
   evidence/progress or the one terminal outcome+resume intent. No remote
   observation claims an AQ+Graph atomic transaction.
5. Every unresolved member has a persisted next-check deadline independent of
   event delivery. Restart scans durable work; periodic reconciliation covers
   missing hints, source outbox retention and lost worker/node connectivity.

Events are **hints only**. Coalesce them into a bounded dirty flag/generation;
never copy event `satisfied` into an outcome. A worker must reobserve all relevant
members, then CAS the generation captured before IO. If a new hint arrives during
observation, retain it for another pass; clearing an older job must not erase it.
Bounded overflow marks that source dirty/rescan-needed rather than dropping the
only correctness path. Notifications/transport acknowledgments cannot answer a
human decision or satisfy a dependency.

### Committed boundary and gap obligations

A replay boundary means no earlier allocated in-flight source mutation can later
commit at/below the cursor unseen by replay. MAX(sequence), nextval, arrival order
or an HTTP timestamp do not prove this. Fake-owner conformance must hold an early
allocation, commit a later allocation, and prove the owner does not expose an
unsafe high watermark. Rollback gaps are not themselves missing committed facts.

A replay retention gap triggers an authoritative owner resnapshot/history
reconciliation under a fresh boundary. Never advance to retained floor, label
history complete, or synthesize success from a hint. If the owner cannot recover
the relevant exact-revision evidence (including a transient completed→reopened
transition), remain pending/RecoveryHeld with explicit unavailable coverage.
An adapter with no such protocol is unsupported for production registration.

## Concrete owner seams

**Mailbox:** derive the exact task recipient from frozen scope/Binding; first
Message adapter searches that task mailbox only. It does not merge old session
mail or retarget it. Source identity includes mailbox key AND thread ID, not a
globally assumed thread UUID. Reuse a narrowly exposed service helper beside
`append`/`inbox`; never call a public route as an internal source owner. Under
the same mailbox-head lock, obtain `next_cursor-1`, observe matching immutable
messages after the predicate's exclusive cursor, and install the wait's member
boundary in the same AQ transaction. Row-based cursor advancement rolls back
with message insertion, so later commits cannot appear below the committed head.
Reobserve after install; ongoing bounded mailbox reads need no new message/event
database. Sender filtering uses retained authenticated envelope identity, with
exact session constraints verified against retained real lifecycle binding.
Unsupported sender/identity mapping refuses rather than guessing. No implicit
message receipt/read acknowledgment is produced by source observation.

**Timer:** retain due instant and original deadline; central supplied time is
checked after locks. A durable due job rechecks the same condition/generation.
Clock rollback cannot reset creation age or make a terminal outcome regress.

**Graph:** reuse existing bounded versioned transport and scope validation, then
add the actual Graph-owner observation port once that lane qualifies it. Predicate
revision encoding must be explicitly frozen with Graph. Its existing
`content_revision` is a logical revision that also changes for lifecycle and
structural-edge mutations; it is NOT a stable content-only pin through completion.
The conservative first adapter should bind an exact starting logical revision
and owner-proven recorded transition with explicit before/after revisions, never
reinterpret that field or guess opaque Text semantics. Dependency proof binds the
exact edge revision/type/direction/endpoints and the required upstream task pin.
Successful requires owner-proven completion; settled cancellation/failure remains
a distinct evidenced outcome. Changed/retired/deleted/reopened sources have typed
policy, never automatic satisfaction. Ordinary polling/history IDs or AQ-only
mutation outbox are not a substitute for the missing owner protocol.

**Other conditions:** fake decision/operation/playbook/resource ports first,
then exact owner joins in AQ-27/GATE-05. Reuse `reviews::completion_in_tx`, confirmed
graph/source operation and playbook owner receipts rather than interpreting
transport acceptance as terminal work. Resource observations never reserve
capacity; wake admission rechecks. Unknown/unqualified source kinds refuse.

## Reducer semantics and finite work

Retain predicate member order. Any chooses the first verified satisfied member
in that order, not event-arrival order. All requires every member satisfied. An
Any with a still-pending or retryably unavailable member cannot fail just because
another member failed. A SourceFailed result needs exact owner evidence; arbitrary
404, network error or vanished credential is not that evidence.

Distinguish retryable source unavailability internally from the frozen terminal
SourceUnavailable outcome. The fake CON-03 peer's eager unavailable resolution
is conformance behavior, not production retry policy. Retryable faults preserve
pending state, last-known freshness and original age, with persisted exponential
backoff plus deterministic per-wait jitter. Proposed initial bounds:1s base,60s
cap,2s adapter call deadline, at most4 physical source operations,16 waits/tick,
64 hint matches/page; all admission is nonwaiting/bounded. If blocking adapters
exist, physical permits remain owned until their callbacks actually finish.

Keep protocol32-member/64KiB request/256KiB view bounds. Proposed initial storage
ceilings:256KiB retained registry evidence per wait; one active wait per attempt/
task continuation;1,024 active waits/project,8,192/daemon; bounded historical
identity/byte budgets reserved before acceptance. Final values are review inputs,
not claims about configured runtime limits. Bounded DFS checks at most1,024 known
nodes/4,096 edges/depth64; exceeding proof bounds refuses `Overloaded`, not
NoKnownCycle. Concurrent opposing registrations serialize and cannot both pass.
Remote/unprovable dependency coverage is explicitly ExternalUnknown; do not
pretend AQ's local graph is the complete remote dependency graph.

Outcome/cancel/deadline compete under the same registry generation. Existing
outcome always replays; task cancellation retires continuation admission even if
the wait was already satisfied. Proposed policy gives current cancellation then
due deadline precedence at commit; source observations remain evidence and are
not labelled failed because a deadline elapsed. Extensions need a separate exact
revisioned operation and fixed original age/maximum ceiling. Retry exhaustion
enters RecoveryHeld/inspection, not invented source failure or endless hot loops.

## WRK-16 / AQ-26 handoff and acceptance

AQ will eventually deliver an authenticated fenced SuspensionIntent tied to
exact Binding, ContinuationRef and actual enforced central-hold receipt. WRK-16
owns checkpoint intent/journal, mutation/input fences, guardian stop and actual
ParkingReport evidence. CheckpointRecorded proves a claim digest only, not source
or summary retention. AQ-26 must join actual retained source/checkpoint receipts,
effect reconciliation and stopped-writer evidence before fresh admission. A
cross-node successor needs a NEW attempt-bound base export rooted at the retained
checkpoint, not a relabelled predecessor manifest/old credentials. Early wait
resolution is retained and cannot waive this parking transition. Warm result
delivery and task mail use the existing mailbox owner; no worker supervisor
credentials are introduced.

First meaningful tests must include:

- Early resolution before/during subscribe, response loss and stable install
  retry; no event received, duplicate hints, late lower-sequence commit and rollback.
- Restart at every intent/subscription/observation/outcome boundary; dirty hint
  during held IO remains scheduled; gap/resnapshot failure stays unavailable.
- Real mailbox insert versus registration/head lock, exclusive cursor and exact
  sender/recipient scope; task mail survives attempts, session mail never moves.
- Same operation changed payload/actor, stale generation, scope loss while IO
  held, late stale lease result, self-wait and concurrent A→B/B→A cycle refusal.
- Any/All deterministic selection, retryable unavailable versus evidenced
  failure, bounded backoff/limits, deadline/cancel/extension age preservation.
- One immutable outcome and resume intent under two drivers/restart; outcome
  while parking cannot clear the physical-writer gate or launch another attempt.

Pure tests use deterministic clocks and fake source owners; durable tests use
actual disposable PostgreSQL when separately approved. No scheduler-ready,
hibernated, released-compute or resumed claim is qualified by a fake admission
port. Production register/show/explain/cancel/ack, AQ-08 hold enforcement,
WRK-16 parking and AQ-26 wake remain individually tracked integration gates.
