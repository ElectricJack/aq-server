# Session operations, durable waits and human notification delivery

**Status:** Required v2 design; observation/accounting and notification components exist. Native integration, stall policy and waits/continuations remain incomplete.

**Date:** 2026-10-30

**Reconciled:** 2026-10-31, with current component evidence and explicit remaining scope.

This extends [profiles and tmux terminals](0003-agent-profiles-harnesses-and-terminals.md)
and [execution recovery](0002-execution-and-recovery.md). Live terminals alone do
not provide session health, usage accounting, human delivery or resumable waits.
These are required product capabilities, including agents suspending themselves
until a dependency resolves. The [task graph](../plans/0001-v2-milestones.md)
assigns implementation and acceptance; existing execution evidence does not
qualify these additions.

## 1. Ownership and deployment

| Owner | Responsibility |
| --- | --- |
| agent-q | Session projections and operator CLI/API, usage ledger, stall policy, wait registry, continuation scheduling, notification outbox, human decisions and escalation/digest policy |
| aq-worker | Harness transcript readers, normalized observations, safe message/nudge delivery, stop/checkpoint/handoff and continuation startup; published worker protocol |
| aq-playbooks | Existing bounded wait/join and twelve host commands; consumes domain events and services without owning agent process lifetime or notification transport |
| aq-dashboard | Session timeline, transcript/usage views, stall actions, waiting/hibernated explanations and external delivery status |
| aq-graph / aq-git | Authoritative task/dependency outcomes / retained source facts through their public interfaces |

Use independently tested Rust modules in the existing repositories. Reuse AQ's
PostgreSQL, worker journal, artifact retention and HTTPS exchanges. No new broker,
metrics cluster, notification service, worker listener or database deployment is
required. Discord is the first external human transport; it is configurable and
not required to start AQ or run core tests. Product qualification must nevertheless
demonstrate an actual configured external destination. Slack/email and interactive
Discord bot conversations are subsequent adapters, not initial dependencies.

The central database owns execution observations, wait/continuation records,
notification deliveries and accounting facts. These do not duplicate graph task
state or Git metadata as editable sources of truth. Views join references and
freshness. Local parser cursors/journals and central delivery receipts own their
respective recovery boundaries, with no distributed transaction assumption.

## 2. Session observability and interaction

### 2.1 Product surface

The following CLI shapes are planned contracts, with equivalent authenticated
API handlers. Agent tools expose only their scoped subset.

| Surface | Meaning |
| --- | --- |
| `aq session show` / `peek` | Bounded current/last-observed state and pane snapshot, with age, epoch, coverage and truncation; never claims liveness from stale text |
| `aq session attach` | Watch an existing session through AQ's terminal relay; explicit control acquisition for input, no direct worker SSH or socket access |
| `aq session nudge` | Durable scoped guidance through the mailbox and safe adapter boundary; queued/delivered/unknown/unsupported are distinct |
| `aq session logs` / `usage` | Paginated transcript events and accounting with source provenance, gaps, pricing version and completeness |
| `aq wait register` / `checkpoint` / `show` / `ack` | Register an owned condition, cooperate with suspension, inspect and acknowledge an exact continuation generation |
| Operator wait/session actions | Scoped, revision-checked cancel-wait, resume/hold, stop and quarantine recovery; cannot forge dependency success or stopped-writer proof |

Attach, peek and nudge are different operations. Attach defaults to watch and
reuses the controller lease/input uncertainty rules of design 0003. A nudge cannot
type over a human composer or race exclusive terminal control. Defer injection
while human control is active unless that controller explicitly yields. AQ's
inbox is the declared fallback; never press Enter at an unknown prompt. A queued
nudge is not proof of delivery or model comprehension. Bulk/repeated nudges are
bounded and deduplicated by action identity, not merely by message text.

Each session view separates authority, process observation, provider activity,
semantic progress, mailbox delivery, active wait and source retention. Capture
timestamps and provenance for each. A heartbeat, terminal repaint, transcript
mtime, AQ-injected reminder or increasing tokens alone does not prove useful
progress. Agent-authored progress is labelled as a claim. Tool completions and
source observations can support policy, but only the owning domain can settle a
task or verify a dependency. Unknown evidence is visible rather than fabricated
as stalled, productive or complete.

### 2.2 Transcripts and usage

Worker adapter readers follow the exact launched session's native transcript or
supported structured event source; never scan every user's provider home for
plausible files. Bind source identity, session/attempt, source generation, record
identity or byte range, parser version and digest. Normalize bounded event types:
turn boundary, assistant/user text, tool start/result, question, provider error,
usage and parser gap. Preserve provenance and label inferred states. Unsupported
formats or partial records report a gap/error, not an empty successful transcript.

Reader tests cover partial writes, rotation/truncation, duplicated records,
cumulative counters, counter resets, native resume, mixed models, malformed large
records and version drift. Checkpoint a parser cursor with its durable outgoing
events; central ingestion deduplicates by stable source-event identity. Do not
advance past unrecorded accounting facts. Bounded raw artifacts permit reparsing
when retained; a parser upgrade creates a versioned reconciliation/correction,
not a second bill for the same provider call. If evidence is irretrievably lost,
mark the affected interval incomplete. Never guess token counts from terminal
characters or equate transcript silence to process death.

AQ stores an append-only usage ledger with correction references and derived
aggregates by task lineage, attempt, project, provider/account and model. Record
input/output/cache/reasoning categories only with their declared semantics;
providers may report overlapping categories, so do not sum all fields blindly.
Keep reported zero distinct from unknown. Deduplicate transcript/API sources
through a declared call identity or precedence rule; do not add two observations
of the same invocation. Cumulative counters produce deltas only inside their
source generation. Historical wake/retry sessions cannot reset the task budget
or recount imported conversation history.

Costs distinguish provider-reported charge, estimate at a pinned rate-card
revision/currency, and unknown. Flat subscriptions/local models need an explicit
cost basis; token-price estimates are not invoices. Late facts/corrections update
projections with an audit trail. Raw transcripts may expire independently of
minimal accounting facts under a declared retention policy. Access and erasure
rules apply to retained content; credentials are never intentionally collected,
and redaction does not guarantee arbitrary agent output contains no secrets.

Accounting is not automatically a hard spending limit. Soft ceilings emit policy
events; hard ceilings require a qualified enforcement boundary and conservative
handling of in-flight/unreported usage. Refuse a requested hard guarantee that
the selected harness cannot enforce. AQ-20's selection/budget interface consumes
this ledger when available, without becoming a second ledger.

All four harness adapters need tested transcript observation and an explicit
usage capability matrix at pinned versions. Codex CLI and Claude Code must
qualify native token accounting in the selected product configurations. For
Gemini CLI/OpenCode, parse all available native usage; any unavailable category
or configuration remains visibly unsupported/unknown and ineligible for policy
requiring complete accounting. Native conversation resume stays optional.

### 2.3 Stall ladder

The daemon runs a deterministic, persisted policy state machine per session/task
episode. Versioned execution policy sets evidence windows, grace periods, nudge
limits/backoff, replacement ceiling and escalation destination. Playbooks consume
its findings/events; a model does not infer authority to kill or replace a writer.

1. **Suspected stall:** expose evidence and freshness. Provider backoff, a registered
   wait, a known long-running check, human control and unavailable observations
   are distinct explanations. Unknown observations raise a monitoring incident;
   they do not justify destructive recovery on their own.
2. **Nudge:** send bounded durable guidance only through a supported safe boundary.
   Record attempted, deferred, delivered and unknown separately. AQ's own nudge
   never resets the semantic progress clock. Repeated deferrals escalate visibly
   rather than looping forever or pretending a reminder reached the agent.
3. **Stop and resume via replacement:** after the configured evidence/backoff
   policy, use typed stop, confirm stopped writers, retain work and reconcile
   effects, then schedule a new attempt from a bounded handoff. Native provider
   resume is optional and version-qualified; no blind same-process restart.
4. **Quarantine and escalate:** exhausted recovery or uncertain stop prevents
   conflicting scheduling, records the exact held resources and notifies the
   configured supervisor/human policy. Quarantine targets the session/lineage;
   do not disable an entire worker/provider account without evidence for that scope.

Progress starts a new evidence interval but cannot reset lineage-wide retry/cost
ceilings. Cancellation, explicit task holds, hibernation and terminal ownership
are checked again at each action admission. Recovery actions have stable IDs,
expected revisions and effect receipts across daemon restart. Quarantine release
is audited and cannot substitute for stop proof or missing retained source.

## 3. Notification transport and human lifecycle

`message.send` and findings outboxes submit logical messages to AQ messaging.
A separate dispatcher resolves authorized recipient routes and durably records
per-destination delivery intents with message revision, routing revision,
redacted payload digest and stable delivery ID. Agents choose permitted logical
recipients, never arbitrary webhook URLs or destination credentials. Secrets stay
in central protected configuration. Route access and project visibility are
rechecked before dispatch; already sent external content cannot be recalled by
local revocation. Destinations are explicitly operator-configured.

Initial transport: a Discord channel webhook adapter that posts bounded summaries
and authenticated AQ links. The exact API/version, limits and receipt/readback
behavior are qualification inputs, not assumptions. No Discord inbound bot or
worker credentials are needed. Disable unintended mentions, bound formatting and
attachments, and do not export transcripts or private task context by default.
Dashboard inbox delivery remains available when an external route is unconfigured;
the message exposes `no_external_route` rather than claiming external delivery.

The transport interface supports send, optional receipt reconciliation and
classified retry. States include queued, dispatching, accepted_by_transport,
delivery_unknown, retry_scheduled, permanently_failed and suppressed. Transient
failures use persisted bounded backoff/jitter and provider-directed rate limits;
permanent failures enter an inspectable retry/dead-letter queue. One destination
cannot block other routes or execution control. Transport acceptance is neither
human read nor human acknowledgment.

Before every external call, persist its identity and dispatch intent. A timeout
after remote acceptance may be ambiguous: reconcile only if the adapter supports
reliable receipt lookup. Otherwise retain `delivery_unknown`. The route's explicit
policy may authorize a labelled retry with duplicate risk; it must not claim
exactly-once external delivery. Internal intent deduplication is required even
when the provider cannot deduplicate. Crash tests exercise both sides of send
and receipt persistence. Retry retention and tombstones cover the documented
replay window; old replay cannot silently create another digest delivery.

### 3.1 Escalations and digests

An escalation episode binds finding/wait/decision, subject revision, eligible
audience, severity, ownership, reminder deadline and notification references.
Open, acknowledged, resolved and expired are separate from transport state.
Acknowledgment stops/escalates reminders according to policy; it does not settle
the task or answer a decision. Decision responses require authenticated AQ action
against the exact question revision. Initial Discord links open that action in AQ;
Discord reactions or quoted replies are not approvals or wake evidence.

Digest occurrences have a durable schedule/window identity, timezone/misfire
policy, bounded fact snapshot and recipient scope. Automated authoring may enrich
the facts; a deterministic fallback handles timeout. Exactly one winning logical
message revision is selected before delivery, so late author completion cannot
send a second report. Urgent escalation routes bypass digest batching. Findings
deduplication, reminders and resolution use AQ-16's episode/outbox service; do not
add another suppression ledger in the runner or transport. Delivery-health faults
are visible internally without recursively generating notifications about every
failed notification.

## 4. Dependency-aware suspension and hibernation

### 4.1 Wait contract and ownership

Agents may register a scoped condition and yield instead of polling. AQ decides
whether a short warm wait is economical or hibernation should release compute;
an agent may explicitly request hibernation. Denial/pending transition is visible,
never silently interpreted as parked. Human/indefinite waits default to hibernation.
A warm wait retains the process, slot and ordinary expiring execution authority;
it is bounded by policy and converts to hibernation or an explicit recovery hold.
Hibernation has no live agent, active execution grant or reserved execution slot.

| Initial source | Verified condition |
| --- | --- |
| Task/dependency | Exact task/edge identity and permitted source revision/generation; distinguish success from settled failure/cancellation |
| Message | Exact authorized thread and exclusive message cursor; sender scope/identity constraints |
| Human decision | Exact AQ decision/subject revision and authenticated eligible response |
| Timer | Persisted due instant, independent of an agent keeping a timer alive |
| Operation/playbook | Owning operation/run identity and authoritative terminal result |
| Resource availability | Typed scheduler/provider/workspace eligibility observation, rechecked at admission; observation is not a reservation |

CI/PR/managed-job adapters register later with their owning integrations. Unknown
kinds refuse; neither arbitrary event names nor agent prose can satisfy a wait.
Support bounded all/any combinations, per-member outcomes and deterministic
selection; no unbounded predicate language. Detect self-waits and cycles among
known task/wait dependencies, and surface cases whose external dependency cycles
cannot be proven absent. Waiting alone does not mark a graph task failed or done.

The wait record binds caller/task/attempt, continuation generation, canonical
predicate digest, source references/revisions/cursors, policy revision, creation
time/deadline and immutable outcome. The idempotency key conflicts on changed
bytes. Re-registering cannot reset age or budgets. Sources are checked under the
caller's scope; unavailable/forbidden data never becomes a success verdict.
Initial outcomes include satisfied, source_failed, cancelled, deadline_reached
and source_unavailable, with verified result references and precise reason.
Temporary provider/network failures remain pending/backoff with freshness visible.

Register and install the central scheduling hold durably before returning success.
For each source adapter, subscribe/persist the cursor and reobserve authoritative
state so an event committed before or during registration cannot be lost. The
adapter must prove its snapshot/replay boundary; a maximum allocated sequence
alone is not a committed watermark. Events prompt verification; they do not
supply truth. Bounded periodic reconciliation recovers missed notifications and
source-outbox gaps. Cross-service graph observations carry revisions and pending
delivery, never a fake AQ+graph atomic commit.

Central admission always checks the continuation hold even if graph projection
updates are pending. Graph dependency/outcome content stays in aq-graph. Source
failure, reopen/revision change, deleted source and task cancellation have typed
policies. A wake does not imply its producer succeeded, nor does it authorize a
publication against an obsolete dependency revision.

### 4.2 Suspension protocol

1. Persist wait, central hold and continuation identity; if already resolved,
   return the verified outcome without forcing an unnecessary suspension.
2. For hibernation, record a fenced suspension intent and request a bounded
   checkpoint summary: goal, work completed, next action, constraints, uncertainties,
   relevant context references and pending effect IDs. Agent summary is a claim;
   source and process facts are recorded by their owners. Missing/cooperative
   failure enters recovery with any available artifacts, not an invented summary.
3. After checkpoint acceptance, close new ordinary agent mutations and terminal
   input, preserving narrow status/recovery paths. Stop all owned writers through
   the guardian; reconcile effects admitted before suspension. Agent turn-end or
   `checkpoint` returning successfully does not prove quiescence.
4. With stop proof, snapshot source through aq-git and upload required source,
   summary and result artifacts through the existing retention protocol. Persist
   exact receipts and stop observations with stable IDs. If the dependency resolves
   during parking, retain that outcome and finish the same transition; never start
   a successor while the old writer remains uncertain.
5. A confirmed stopped process can release compute independently of pending
   protected disk. Mark the continuation safely hibernated/migratable only after
   required central retention and effect reconciliation. Local workspace release
   requires the existing explicit retention/release decision. Failed upload,
   checkpoint or stop holds the corresponding resources with bounded recovery.

Suspension has a distinct attempt disposition. A cooperative harness exit during
parking is neither task success nor failure; the ordinary result-settlement path
must not close the graph task or spend a failure retry solely for hibernation.
Actual failed suspension/resume attempts follow their recorded recovery budgets.

No live task claim/worker lease must persist merely to remember a sleeping agent.
The central continuation owns future admission, not a parked process. Retained
source consumes an explicit project/tenant storage budget; hibernating thousands
of agents must not reserve thousands of hot worktrees. Dirty/untracked content
follows aq-git's supported snapshot contract; unsupported or oversized required
content refuses safe parking until resolved. Timed garbage collection cannot
discard an active continuation's required artifacts.

### 4.3 Wake, migration and cancellation

Resolving a wait commits its immutable outcome and an idempotent resume intent in
the same AQ transaction. A scheduler-ready projection is derived from that intent.
Wait outcome, physical parking, capacity availability and delivery/acknowledgment
are independent states; `resume_pending` is not running. Events can resolve the
wait while the node is offline, but cannot authorize overlapping writers.

Wake uses ordinary project/node eligibility, profile selection, fairness, grants
and resource admission. It may land on a different compatible worker after
central retention; never requires the original container. Create a new attempt,
new session and fresh scoped credentials with lineage linking to the predecessor.
Each dispatch generation has one durable winning attempt; retries query that
attempt instead of launching a duplicate. Re-resolve allowed current profiles
and context, record differences from the predecessor and carry forward retry,
time and cost budgets. Restoring private provider conversation state is optional;
correctness depends on retained source plus bounded context/summary/outcome.

The bootstrap context contains wait identity/generation, verified outcome and
result references, retained source receipt, summary, unresolved effects and held
mail. The successor acknowledges the exact continuation delivery before ordinary
task mutations; bootstrap allows only scoped reads/ack/recovery. Duplicate ack
returns its original receipt. A stale successor cannot consume the new generation
or clear its hold. Native resume must not re-import old credentials or silently
execute buffered commands. Failed launch/ack has persisted bounded recovery and
eventual escalation, not a stranded permanent `resume_pending` loop.

Task-addressed mail waits durably for the successor; old session-addressed mail
does not silently retarget. Normal mail does not wake a dependency wait unless
its predicate or explicit policy allows it. An in-place warm wait receives its
verified outcome through the durable inbox and acknowledges it without relaunch.
Central supervisors can use the same own-session wait semantics under their
central authority adapter; worker nodes never inherit supervisor credentials.

Cancellation/completion/supersession serializes against wake admission and
retires the current continuation, preserving source/history. Cancelling only the
wait returns a cancelled outcome for policy-directed continuation; cancelling
the task prevents automatic resume. An operator can request an explicit early
resume, but the context says `manual_override` and preserves the unresolved
predicate; it cannot manufacture dependency success. Deadline policy can notify,
hold, or resume with a non-success outcome; extending a deadline does not reset
the original age or total ceiling. Resource wake bursts are coalesced, paginated,
fairly scheduled and subject to revalidation/backoff, never an agent polling loop.

## 5. Playbooks and bounded APIs

Keep the twelve ordinary policy commands. Agent self-waits/checkpoints/acks are
scoped execution APIs, not new general playbook commands. Engine-native wait/join
still suspends only the workflow; requesting an agent suspension uses the host's
typed domain path and does not keep a workflow thread or agent running. Publish
finite context views for sessions, usage, waits and delivery. Domain producers
publish versioned session/wait/notification events or typed `operation.changed`
results with stable identities and permissions.

`message.send` routes through messaging/notification delivery, `decision.request`
through authenticated human decisions, and `findings.reconcile` through episodes
and their outbox. `operation.get/cancel` use only server-owned registered handles;
do not smuggle arbitrary session commands through string operation names.
Stall mechanics and wait lifetime stay in AQ; playbooks express escalation,
reminder and digest policy using these services. No runner fork or duplicate
transport/wait store is required.

### 5.1 Playbook catalog profiles (as implemented)

The embedded host registers a fixed, numbered catalog profile for each
namespace. The playbooks config field `profile` selects it. A profile only ever
adds contracts. An existing profile's registry and stored grants are never
widened in place, so new contracts need a new profile number.

| # | Config value | Registry |
| --- | --- | --- |
| 1 | — (not selectable in config) | Mail1: the `message.send` command and the `aq.operation.updated` v1 event (producer `aq.host.bridge@1`) |
| 2 | `human_review_v2` | Mail1 plus the `decision.request` and `operation.get` commands and the `aq.decision.completed` v1 event (producer `aq.human.decisions@1`) |
| 3 | `graph_events_v3` | HumanReview2 plus the `aq.graph.operation.confirmed` v1 event contract (D3) |
| 4 | `settlement_events_v4` | GraphEvents3 plus the `task.completed` v1 event contract for confirmed reviewed-task settlement |

The `aq.graph.operation.confirmed` v1 contract
(`playbooks/review_domain.rs::graph_operation_event`) has:

- trusted producer `aq.graph.operations@1`;
- scope `project`;
- correlation field `operation`;
- payload `{operation_id, task_id, attempt_id}`, each a UUID string.

Profiles 2, 3 and 4 accept the same grant capabilities: `message.send`,
`decision.request` and `operation.get`.

**Graph-event collection (profile 3).** Each driver quantum seals at most two
graph events into the engine's `playbook_outbox`. A row of `agent_graph_outbox`
is eligible when it is undelivered, its operation is `confirmed`, and its
launch binding matches the namespace's project and workspace. Rows already
sealed for the namespace are skipped. Rows are taken in `operation_seq` order.
Each event uses epoch 0, and its `event_id` is the graph outbox `event_id`.

The driver collects only after an artifact that declares this exact contract
is installed, because the engine refuses events whose contract no installed
artifact declares. Runtime startup installs the configured deployments, so
production sets this condition at boot.

`agent_graph_outbox.delivered=true` is set only by `acknowledge_event`, in the
same transaction that records the engine's durable acceptance (the engine ACK).
A crash between the engine commit and the AQ ACK replays the same sealed event.
The engine then answers `duplicate` for the same key,
digest(producer, 0, event_id), so the workflow sees each event once.

Two limits apply:

- **Delivery is per event, not per namespace.** With two `graph_events_v3`
  namespaces on one project, the first ACK marks the row delivered, and the
  second namespace no longer collects it.
- **A refused event stops the outbox page loop.** This is existing driver
  behaviour.

See the [D3 implementation notes](../implementation/D3.md).

**Settlement-event collection (profile 4).** A namespace configured with
`profile: "settlement_events_v4"` collects at most two confirmed reviewed-task
completion events per quantum after an artifact declaring the exact
`task.completed` contract is installed. Existing namespaces keep their catalog
identity; use a new namespace for profile 4. The producer is
`aq.task.settlement@1`, with project scope and `operation` correlation. The
completion operation ID is the stable event ID. Its payload retains the attempt,
evaluation, report artifact, verified release and confirmed Graph receipt.

Collection requires the launch's project and workspace to match the namespace.
Prepared or uncertain completions remain pending. The source completion's
`outbox_acknowledged` flag changes only in the transaction recording engine
acceptance. The sealed event survives a restart, and replay after an omitted AQ
ACK returns the engine's existing acceptance. Delivery is per event, as for graph
events above; it is not broadcast to every namespace.

### 5.2 Owner artifact install and binding activation (as implemented, D9)

The namespace owner installs sealed playbook artifacts and chooses which
artifact a binding runs through five routes (`playbooks/api.rs`, implemented in
`playbooks/artifacts.rs`). There is no generic command route and no route that
registers a synthetic host. The CLI is separate work (F7).

| Route | Body | Effect |
| --- | --- | --- |
| `POST /api/playbooks/artifacts/validate` | `{artifact}` | Validates only and returns `{digest, binding, commands, events, dependencies}`. Writes nothing. |
| `POST /api/playbooks/artifacts` | `{op_id, artifact}` | Validates and installs the artifact. Needs no grant. |
| `POST /api/playbooks/bindings/{binding}/activate` | `{op_id, artifact, grant_id, grant_revision}` | Makes the artifact digest the binding's active artifact. |
| `POST /api/playbooks/bindings/{binding}/rollback` | `{op_id, grant_id, grant_revision}` | Re-activates the artifact that was active before the binding's latest activation. |
| `POST /api/playbooks/coding/default` | `{op_id, policy, grant_id, grant_revision}` | Builds the `aq.coding` artifacts from `policy`, installs them and activates `aq.coding` (see [spec 0002 §7.1](../specs/0002-execution-and-agent-access.md#71-default-coding-binding-aqcoding-as-implemented-d9)). |

**Access and bounds.** Every route is owner-only: the caller must be the
namespace's `owner_user_id`, and any other user gets `404`. Bodies are capped at
2 MiB and reject unknown fields. Operation ids and binding names are 1–128
characters with no control characters. For `coding/default` the `op_id` limit
is 112, because the route derives sub-operation ids from it. Errors map to
`400` (invalid), `409` (conflict) and `503` (engine unavailable).

**Validation.** The artifact arrives as JSON. The route re-encodes it
canonically and decodes it with `Artifact::from_canonical_bytes`, which checks
canonical form, structure and digest. The encoded artifact must be at most
1 MiB. Every command and event contract the artifact declares must exactly equal
the contract of the same name in the namespace's pinned catalog profile (§5.1).
A contract that is missing from the catalog, or differs from it, is refused.
Install is content-addressed, so installing identical bytes again changes nothing.

**Operation-id idempotency.** Each install, activate and rollback carries a
caller `op_id`. The ledger records each `op_id` once per namespace, and the
first writer wins. Responses are `{op_id, kind, binding, artifact, previous,
duplicate}`.

- Replaying an `op_id` with the same request (same kind and binding, and the
  same artifact for install and activate) returns the recorded result with
  `duplicate: true`. It does not touch the engine again.
- Reusing an `op_id` for a different request returns `409`.
- A crash after the engine commit and before the ledger write is safe to
  replay. On replay, the previous artifact is read from the ledger first, so
  the replay records the same `previous`.

`coding/default` derives `{op_id}/install/{i}` for each artifact and
`{op_id}/activate` for the activation, so replaying it is idempotent at every
step.

**Activation stack and authority.** Activation and rollback run under an active
grant. `grant_id` and `grant_revision` must name a current, unexpired grant
(`Service::active_authority`), and the engine activates with that authority.
Activate pushes onto the binding's activation stack. Rollback pops the stack
and re-activates the entry below. The bottom of the stack is the artifact that
was active before the first ledgered activation, read through
`Driver::binding_artifact`. Rollback with nothing below the current entry
refuses with `409`. It never falls back to an empty binding.

**Activation is pinned per run at admission.** The engine fixes each run's
artifact when it admits the run. Activation and rollback change only which
artifact *new* runs admit. A run already admitted finishes on the artifact it
was admitted with (`activation_switches_future_runs_only`).

**Ledger.** `PgLedger` stores records in `playbook_activations`
(`migrations/0085_playbook_activations.sql`), keyed by `(namespace_id, op_id)`
and ordered by `seq`. Install rows are informational, because install is
content-addressed in the engine. The ledger does not serialize concurrent
activations of one binding. Only the engine orders those; hardening is a filed
follow-up. See the [D9 implementation notes](../implementation/D9.md).

## 6. Bounds and verification

Publish negotiated record/batch sizes, per-session/per-node spool limits,
transcript retention, parser CPU budget, central ingress budgets, wait member and
subscription limits, and dispatcher concurrency before implementation acceptance.
Suggested fixture profile: 64 KiB/event, 256 KiB/batch, 16 MiB telemetry spool/node,
32 predicate members/wait and one active suspension wait/attempt. These are test
inputs, not measured capacity. Preserve stop/source/control capacity independently
of transcript, accounting and delivery traffic. Under overload, explicit gaps and
incomplete accounting are permissible only when policy permits loss; a configured
no-loss requirement must trigger admission/backpressure or bounded stop before
storage exhaustion, not block the guardian or claim complete accounting.

Core tests use fake clocks, source owners, provider transcripts, Discord transport
and workers. No paid model calls or real human notifications in unit tests.
Explicit product qualification uses pinned harnesses, actual tmux, two eligible
workers, the browser/CLI and a configured Discord test destination. It exercises:

- Peek/attach/nudge and a safe deferred composer; stall nudge/replacement/quarantine
  with lost acknowledgments, real progress, intentional wait and human control.
- Transcript rotation/replay/parser change, unknown usage, cumulative counters,
  corrected cost estimates and budgets across replacement/hibernation.
- Send-before-receipt crash, rate limit, permanent/ambiguous failure, route revocation,
  digest fallback versus late completion, and authenticated decision versus transport ack.
- Dependency resolving before registration, during checkpoint, after daemon restart,
  or while the original node is lost; same and different worker wake without duplicate
  launch, stale ack, lost source, timeout, cancellation/wake race and failed resume.
- A sleeping continuation releases compute, retained work survives local cleanup,
  wake storms preserve fairness and control traffic, and no resume resets budgets.

INT-03 records exact artifacts and declared load/storage/retention parameters.
PERF-01 includes 500 simulated active agents plus 10,000 dormant continuations and
a burst of 1,000 resolutions, bounded by normal admission; actual harness/terminal
concurrency remains separately declared. Test dormant records rather than keeping
10,000 idle provider processes. No acceptance by only showing a dashboard badge.

## 7. Traceable requirements and delivery

| ID | Requirement |
| --- | --- |
| S1 | Scoped CLI/API peek, attach, nudge, logs and usage distinguish freshness, watch/control and uncertain delivery for remote and central sessions. |
| S2 | Versioned per-harness transcript readers normalize bounded events with durable cursors, provenance, replay deduplication and explicit gaps. |
| S3 | Usage/cost ledger distinguishes reported, estimated and unknown values, reconciles corrections and preserves lineage budgets across retry/wake. |
| S4 | Persisted evidence-based stall ladder supports safe nudge, stopped-and-retained replacement and scoped quarantine with bounded escalation. |
| S5 | Intentional waits, human control, missing observations and AQ-injected traffic cannot produce false progress or unsafe stall recovery. |
| N1 | Logical messages route through a durable scoped central outbox to a qualified Discord adapter without worker secrets or another mandatory service. |
| N2 | Retry, rate limit, dead-letter and ambiguous external delivery are explicit; transport acceptance is distinct from human acknowledgment/decision. |
| N3 | Escalations and digests use durable episode/window identities, bounded reminders and one winning logical report, with authenticated revision-bound decisions. |
| N4 | Destination policy, visibility, payload bounds and secret protection apply at dispatch; delivery failure cannot starve execution or recursively flood notifications. |
| C1 | Agents register typed verified dependency waits without polling; registration/reobservation and replay prevent lost wakeups. |
| C2 | Suspension checkpoints, stops writers and retains source/effects before safe hibernation; compute and protected disk have separate release rules. |
| C3 | Durable continuation outcomes and fenced wake admission resume on an eligible node with a new attempt, retained context and fresh authority. |
| C4 | Wake acknowledgment, held mail, cancellation/supersession/deadlines and concurrent outcomes cannot duplicate launch or settle stale work. |
| C5 | Waits are bounded, cycle-checked where knowable, fair under wake bursts and preserve age, artifact retention and retry/cost budgets. |
| C6 | Central supervisors and remote workers share wait semantics through separate authority adapters; playbook waits do not duplicate execution lifetime. |

CON-03 publishes observation/continuation contracts. WRK-15 and AQ-22 implement
readers/accounting; AQ-23 adds the stall ladder. AQ-24 adds external delivery and
AQ-27 integrates escalation/digest policy. AQ-25, WRK-16 and AQ-26 implement wait
verification, physical parking and continuation scheduling. UI-04 and INT-03
complete operator/product acceptance. These are explicit M5/M6 prerequisites,
not optional parity follow-ups.

V1 references used for retained behavior: [verified waits](../../../agent-queue2/docs/guides/agent-waits.md),
[source contracts](../../../agent-queue2/docs/guides/wait-source-contract.md),
[session runtime](../../../agent-queue2/docs/specs/design/session-runtime.md),
[usage accounting](../../../agent-queue2/docs/reference/usage-accounting.md) and
[messaging](../../../agent-queue2/docs/specs/systems/messaging-and-human-asks.md).
These inform semantics, not v2 implementation dependencies or a full v1 port.

## Revision-pinned implementation evidence

The [bounded v1 reference inventory](../research/2026-10-30-v1-implementation-reference-map.md) records implementation symbols, selected test obligations and per-file hashes at revision `9a9b8e8ef992d1ff699755a9383ed3c91e2844ea`. It distinguishes preserved behavior from remote-authority and retention adaptations. This evidence informs the owning v2 tasks; replacement coverage and product qualification remain pending.

## Current implementation boundary (2026-10-31)

CON-03 contracts, the central observation owner/accounting ledger and notification
dispatcher have retained component evidence. Worker normalizers, transport
ownership and the bounded store are independently tested. The format5 store
reserves logical control capacity without deleting history; physical-full
progress and in-place migration remain unqualified. It is not yet adopted by
the production worker. Selected native source/parser qualification, raw content
retention and rate-card/lineage-budget integration remain open.

AQ-24's transport implementation does not establish actual external destination
delivery, authenticated human decisions or complete escalation/digest workflows.
Those need their own configured integration evidence. The central supervisor,
real harness nudge, persisted stall ladder and production playbook adapters also
remain open. No synthetic host can be registered as a production adapter merely
because it passes runner tests.

The [current completion scope](../plans/0002-execution-sessions-policy-completion.md)
prioritizes real execution, session operations and supervisor/policy integration.
AQ-25/26 and WRK-16 hibernation remain separate work. Missing wait support must
remain explicit in intermediate capabilities; it cannot be treated as an empty
wait set authorizing destructive recovery. AQ-27's wait-source portions, GATE-05
and INT-03 retain all their original continuation prerequisites.
