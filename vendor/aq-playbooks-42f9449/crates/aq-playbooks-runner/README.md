# Durable runner

The runner imports the model and never the compiler. `Engine<S>` verifies installed
artifacts and pins the complete callable closure at root/event admission. Exact
and explicitly authorized compatible catalog selections resolve against one
captured catalog revision. Later activation cannot retarget a child or grandchild.
`Simulation` uses this same evaluator, `MemoryStorage`, an explicit virtual clock
and `FakeHost`; missing mock responses remain unknown.

Every mutation commits through namespace revision CAS. Run snapshots, receipts,
effect intents, waits, timers, children, budgets, accepted events and discovery
indexes share that atomic boundary. Runtime indexes include ready queues, due
timers, effect queues, event correlation postings, subscriptions and receipt/run
pagination. PostgreSQL mirrors discovery and safe inspection projections into
native indexed tables. `drive` consumes bounded work and persists round-robin
cursors across scopes, run/effect classes and recovery participants. Idle polling
uses indexed queries and does not commit a transition. Atomic snapshot writes and
serialization still scale with retained namespace size; indexed discovery does
not remove that cost.

`Engine::new(store, namespace, incarnation)` acquires exclusive ownership. The
incarnation identifies one live runtime; retire the previous process before
replacement. CAS verifies ownership before dispatch, cancellation and recovery.
This prevents concurrent writers without claiming to fence an already-issued
external request in another system.

The host supplies separately authenticated authority and trusted acceptance time.
Event admission checks producer, exact contract fingerprint, schema, scope,
correlation fields and subject/revision requirements. It retains correlated
completions even without trigger subscribers. Trigger destinations, mapped inputs
and the callable-catalog snapshot freeze at acceptance. Bounded `fanout` resumes
without duplication. Missing immutable dependencies produce retained
`destination_blocked` receipts and resume against the original catalog revision;
invalid mapped inputs remain blocked under their original proposal.

`ActivationControls` independently stop new calls, stop event starts or revoke
execution. Actor/scope capability revocations also apply to existing command,
child and recovery starts. Current host authorization is rechecked using the
original caller ceiling. Stopping new event starts does not discard a destination
already accepted under an earlier activation.

Waits use exact invocation correlation and optional minimum revision. The first
eligible durable acceptance wins; processing backlog cannot replace a predeadline
completion with timeout. Expired correlation history refuses a new general wait
instead of silently waiting for a pruned result. Joins capture finite owned child
or operation handles, use committed completion acceptance order and compare
accepted time with their deadline. Named member aliases expose typed heterogeneous
results `{participant,outcome,output}` to downstream expressions. Remaining work is
tracked or requested to cancel. A parent returns through `finishing` until owned
obligations settle. Independent starts retain origin provenance and consume the
same finite allocation ledger without becoming cancellation-owned children.

Effect intents commit input digest, original authority, exact command contract,
stable operation ID and absolute deadline before handoff. Transport recovery
keeps that ID; bounded business retries use new IDs and allowances indexed by the
full loop invocation path. Re-readable/idempotent effects replay only under their
contract, reconcilable effects query authoritative status and unsafe effects wait
for an audited `request_recovery` before status reconciliation. An operator's
request never supplies a manufactured outcome or replaces the caller authority.

An expired unissued effect is refused before dispatch. An expired issued effect
remains unresolved. Cancellation acceptance does not prove stopping;
`Host::cancel` distinguishes accepted, confirmed, refused, too late and unknown.
After interrupted cancellation dispatch, recovery queries authoritative status
before authorizing another same-identity cancellation delivery. Confirmed
cancellation remains final and late authoritative results are retained separately.
Asynchronous admission follows an explicit `accepted` edge and binds `{handle}`;
completion matching remains pinned after that step has continued. Invalid results
block only the affected operation and retain a safe diagnostic.

`EngineConfig` bounds namespace runs, events, queued destinations, receipts and
bytes. Artifacts bound payloads, visits, nested iterations, history, duration,
children, depth and concurrent work. Execution-budget exhaustion settles owned
work and fails once. Quota refusal preserves existing obligations.

`RetentionPolicy` declares event replay, terminal-run and audit horizons plus a
bounded tombstone quota. Bounded GC pages persist their scan cursor, preserve
unsettled cohorts and transitive artifact closures, and record disposal. Replay
identities become tombstones rather than reusable IDs. Explicit monotonic producer
epoch retirement fences old requests and permits bounded tombstone compaction.
Use `admit_event_in_epoch` after retiring legacy epoch zero. Captured replay
horizons cannot be shortened by later policy edits. Root/child audit records and
late result evidence retain their required artifacts until eligible disposal.

Public receipts default to no result fields; explicit public projections and safe
inspection pages omit protected payloads and private host diagnostics. Full state
is a privileged API. Sensitive input/output contracts across the pinned callable
closure require a configured storage protection provider before root or child
inputs are admitted. Registered sensitive event payload fields, protected trigger
inputs and known sensitive asynchronous completion outputs receive the same check
before an event is persisted. Memory/default storage refuses these admissions.
Privacy of undeclared ordinary JSON remains a host contract responsibility.
The PostgreSQL adapter supplies authenticated encryption with an explicit
keyring. Back up encrypted data and its required keys together under host policy.

Format 1 supports serial bounded foreach, nested lexical bindings, parallel owned
async work with bounded joins, all three child modes, event/timer waits, explicit
unknown branches and bounded retries. Parallel foreach, recursion, dynamic calls,
active-run migration, shared/reparented children and arbitrary cross-run policy
state remain explicitly rejected. Hosts retain domain truth, current object
revisions and mutation preconditions.
