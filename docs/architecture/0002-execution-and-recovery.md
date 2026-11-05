# Agent execution and recovery across AQ and workers

**Status:** Required shared contract; foundations are implemented, native-agent integration remains in progress.

**Date:** 2026-10-30

**Reconciled:** 2026-10-31, against daemon state format20 and the overnight component checkpoints.

The daemon decides which execution is authorized. A worker manages local processes and reports observations. Neither a heartbeat timeout nor an agent success message proves that execution stopped, source was retained, or a task is complete. This document is the canonical shared lifecycle contract; the [daemon](../specs/0002-execution-and-agent-access.md) and [worker](../../../aq-worker/docs/specs/0002-execution-and-agent-access.md) specifications define the implementation responsibilities.

## 1. Identities and immutable inputs

| Identity | Meaning |
| --- | --- |
| Task | Desired outcome in aq-graph; may require multiple attempts |
| Attempt | One bounded execution effort; replacement execution gets a new ID |
| Session | One agent conversation; initially one session per attempt |
| Assignment generation | Daemon-owned fencing value binding an attempt to a node and runtime incarnation |
| Node and boot ID | Stable enrolled node and one runtime incarnation |
| Operation/report/directive/message ID | Stable identity for one logical exchange, preserved across transport retries |

At most one assignment generation for an attempt is currently authorized. A confirmed surviving process may be reattached under renewed authority after reconciliation. Relaunching an agent after exit is a new attempt, not a replay of a start request. Initial release excludes migration of live sessions between nodes, persistent shared conversations and automatic resumption inferred from a PID.

The resolved execution specification freezes task/context revision references, outcome contract, project/repository binding, base OID, harness/model configuration, resource limits, folder grants, permitted agent operations and retry/termination budgets. It has a content digest. Credentials are provisioned separately through protected mechanisms; ordinary specification payloads contain references. Messages cannot silently change these fields. Unsupported live changes require stopping and creating a new attempt.

The [profile/session design](0003-agent-profiles-harnesses-and-terminals.md) defines deterministic composition and intelligence selection for those frozen inputs. Every real agent CLI runs in an attempt-owned tmux host within the tested guardian boundary. Remote watch/control has its own session/epoch and controller fencing, does not renew execution authority, and does not enable unqualified live reattachment. Synthetic process acceptance predates and does not qualify these additions.

## 2. Ownership and consistency

AQ stores grants, directive delivery, received observations and result dispositions. The worker stores local action intent, process evidence, retained files and outgoing reports. Graph content lives only in aq-graph; source facts live in aq-git. Each owner acknowledges its own durable boundary, and retries connect those boundaries. No cross-system exactly-once transaction is assumed.

A daemon status view combines authority with the most recent worker observation and its freshness. `unknown` is a valid state. Do not replace uncertainty with failure, success or free capacity. Acknowledged central source retention is different from acknowledged report disposition and from graph completion.

## 3. Assignment handshake

Native profiled execution separates no-launch preparation from execution authority:

1. AQ validates the current registered runtime/incarnation and project policy,
   resolves authorized graph/source inputs and profile/intelligence selection,
   and retains the exact nonsecret input artifacts through their owning services.
   A digest or source observation alone does not establish retention.
2. AQ commits an expiring offer with the exact node/boot, attempt/session,
   immutable launch binding and shared task claim before exposing it to polling.
   An offer is not permission to launch.
3. The worker validates the offer and durably reserves local capacity without
   launching. Its acceptance identifies that reservation, original journal
   evidence and proposed logical launch owner. AQ serializes acceptance against
   expiry, cancellation, current incarnation/configuration and conflicting work.
4. Acceptance commits an immutable PreparationPlan and admitted private-material
   lease records, then returns that original plan. It does not issue a grant.
   The worker fetches assigned retained inputs, prepares the exact workspace and
   installs private material under the qualified adapter. It returns bounded
   receipts tied to the same binding, owner and journal generation. Preparation
   receipt delivery and material availability are not execution authority.
5. AQ verifies the owning-domain preparation evidence and rechecks current
   eligibility, retention, policy/credential revisions, cancellation and deadlines
   after relevant lock waits. It commits a grant before returning authorization.
   Missing or changed evidence refuses rather than manufacturing a receipt.
6. The worker checks the still-live reservation, preparation, exact grant and
   incarnation. It durably records the grant/launch intent before invoking the
   guarded tmux host. Only the actual verified host observation can establish
   Running through Started reporting; grant issuance alone leaves it unknown.
7. Renewal, cancellation and recovery use independent control capacity. Exact
   retries recover the original operation outcome without refreshing its expiry,
   repeating a physical launch or implying the old grant is currently active.

The previously qualified deterministic-fixture protocol remains versioned and
unchanged. Its acceptance/grant shortcut is not the native preparation protocol;
new native producers cannot encode a real agent as a fixture execution merely to
reuse that shortcut. AQ owns the current native transport envelopes while frozen
shared identity and launch contracts retain their original canonical bytes.

Reservation expiry is terminal for that reservation. Delayed plans, material or
grant responses cannot resurrect it. The worker reports its actual non-start or
uncertain-launch disposition and reconciles the existing operation. A central
NeverGranted closure proves that no grant was issued; it does not assert physical
stop and cannot override uncertain external effects. Releasing a granted claim
requires independently verified stop, retention and effect disposition.

A profiled attempt's shared task claim (`holds_task`) is released in exactly
three lawful ways; the claim guards refuse any other true→false transition:

1. **Verified Graph completion release.** An accepted settlement evaluation, its
   owner-verified release record and the confirmed Graph completion receipt and
   outbox entry.
2. **Never-granted closure.** Central proof that no grant was ever issued for the
   attempt (it asserts no physical stop).
3. **Replacement release.** An immutable, insert-only
   `profiled_replacement_releases` row for a failed predecessor. Its trigger
   rechecks current hot authority and refuses the row unless all of these hold:
   - stop is `stopped` or `not_started`;
   - source retention is `retained`;
   - effects are `quiescent`;
   - the stop, retained-source and effect-reconciliation receipt digests in the
     row equal those in hot authority;
   - status is terminal (`finished`, `declined`, `cancelling`) or `unknown`;
   - the attempt has no accepted, stale or needs-reconciliation settlement
     evaluation and no task completion.

   The daemon takes the node lock and locks the attempt and hot-authority rows,
   re-assesses the locked history and checks the predecessor is unchanged before
   inserting. A replacement release is never a Graph completion and cannot
   release an accepted attempt. Its successor is a new attempt, session and
   generation that reuses the predecessor's verified export through the exact
   retained-source receipt (§7); missing release or export evidence refuses the
   offer rather than creating a new export. Retry, lineage-time and
   sandbox-minute budgets are counted across the lineage, so exhaustion blocks
   replacement.

## 4. Lifecycle and observed outcome

For native execution, the logical order is offered/reserved, preparing, granted,
running, stopping, collecting/reporting, acknowledged and retired. These labels
explain the protocol; they do not replace a component's versioned state enum.
Preparation precedes execution authority. Offers may be declined or expire
without launch. Process outcome, stop certainty, source retention, report
disposition and task policy state are separate fields.

An agent may claim success while the runtime reports `stop_uncertain`. Such an observation cannot release a workspace or permit a checkpoint requiring stopped writers. A confirmed stopped agent may free an execution slot while its pending result still occupies protected disk and transfer capacity. An acknowledged rejected result may still contain retained useful work.

The daemon stores each immutable final report and its stable disposition. Reusing an ID with changed bytes is a conflict. Corrections reference prior reports under a new ID. A lost acknowledgement causes resubmission of the same report, not another task completion.

## 5. Authority, disconnect and retry

Heartbeats describe node liveness; grants authorize a particular attempt. Initial proposed defaults remain a 15-second heartbeat, offline classification after 60 seconds, and a 120-second execution grant renewed about every 15 seconds. They are configurable pilot values. Renewal deadlines conservatively account for transport time and use a local clock that advances across suspend. A response received after expiry cannot revive the old grant. Restart or uncertain time requires reconciliation.

Private-material delivery leases authorize bounded retrieval and installation;
they do not independently authorize execution. Initial GrantedLaunch keeps its
existing rule that material delivery expires no earlier than the initial grant.
After verified installation, a running renewal uses a separately versioned
RenewedAuthority reply: the exact grant/binding/reservation/owner, monotonic
authority and grant revisions, a fresh conservative delivery clock, and the
original immutable execution hard deadline. It does not redeliver credentials
or extend the old material lease. The daemon rechecks current machine/agent
scope, registry/profile and the original installed material fingerprint before
renewing; rotation or revocation refuses renewal. The worker rejects stale or
out-of-order authority and cannot reanchor a cached TTL to a later retry.

Expiry of retrieval permission does not invalidate an already admitted private
binding by itself. Installed material remains bound to that execution only and
is removed on confirmed stop or once revocation/hard-deadline stopping has
proved writers stopped. Uncertain writers retain their protected recovery state;
no credential replacement, new process or silent extension follows. This is a
native renewal design requirement, not a claim that the current worker implements
it. The original0.1.6 initial-grant bytes and semantics remain frozen.


The baseline stops execution and preserves work when authority expires. Short disconnections are tolerated within the existing grant. The runtime must not continue indefinitely on cached permission. New agent API mutations are refused once daemon authority is expired, cancelled or superseded. Already admitted operations follow their recorded recovery rules; revocation cannot undo a committed graph write or a dispatched external effect.

Lease expiry does not prove physical termination. Retry policy distinguishes:

| Case | Default |
| --- | --- |
| Same network operation, unknown response | Query/replay the same operation ID and payload |
| Prior process provably survives and current authority can be renewed | Reattach the same attempt, if the adapter supports it |
| Prior process confirmed stopped; effects reconciled | New attempt may use explicitly selected retained inputs |
| Prior process or external effects uncertain | Hold conflicting execution; reconcile or request a scoped recovery decision |

Replacement while old execution remains uncertain requires an explicit policy establishing safe overlap: independent writable workspace, no conflicting external writable resource, and effects that are prevented, idempotent or reconciled. Workspace confinement alone does not prove absence of external effects. The initial default blocks such replacement. A recovery decision records its scope, reason and remaining uncertainty; it does not fabricate a stopped observation.

Retry limits, cost/time budgets and attempt lineage are daemon-owned and cannot reset merely by creating another attempt. Bounded disconnected execution beyond grant expiry is deferred; it requires a distinct capability and policy.

## 6. Cancellation and changing instructions

Cancellation is a durable typed directive. Admission records intent, closes new authority-dependent actions and queues delivery; it does not claim the process stopped. Workers receive it through outbound polling, prioritize it over conversational input, stop owned writers, preserve required work and report actual termination certainty.

AQ serializes cancellation and result eligibility for an attempt. If cancellation is recorded before result acceptance, a later result is retained but cannot automatically qualify as successful task completion. If result acceptance wins first, a later cancellation cannot erase that fact or undo completed effects; it can prevent subsequent undispatched actions under policy. The UI must distinguish accepted execution results from later task completion. Outstanding graph/publication operations need reconciliation according to their own admission boundaries.

The default completion policy (§7.1) enforces this serialization in its acceptance gate. A cancelled or superseded report records `rework` with `not_accepted`, so it can never complete the task. A report without a stop proof is held with `not_stopped`.

Conversational guidance may be delivered during a session but cannot change typed permissions, renew grants, or approve publication. Record `queued`, `received_by_node`, `delivered_to_harness`, `delivery_unknown`, and stale/undeliverable dispositions. Harness delivery is not proof of model comprehension. Ambiguous input injection is not automatically repeated unless the harness supports deduplication.

## 7. Results, source and completion

After writers stop, the worker checkpoints through aq-git, exports/uploads source, obtains central retention, and submits the immutable report. Non-code results follow an equivalent retained-artifact contract. Pending required work remains protected until a retention/release decision; neither elapsed age nor task cancellation authorizes deletion.

AQ may retain a stale attempt's source while rejecting its result for current task settlement. The daemon applies evidence and playbook policy to accepted results and delivers authorized completion changes to aq-graph with expected versions and stable operation IDs. A version conflict requires reconsidering the completion inputs. No automatic read-new-version-and-overwrite loop is allowed.

Replacement attempts start from explicit base/checkpoint references and a bounded handoff summary with provenance. They do not inherit old credentials, mutable directories or provider conversation state implicitly.

The [session operations and waits design](0004-session-operations-waits-and-notifications.md)
adds voluntary suspension and durable continuations. Hibernation releases compute
after stopped-writer proof while required source remains protected until retention
and release. Wake creates a newly authorized attempt from verified dependency
outcome and retained context, potentially on another eligible node. Wait resolution
alone cannot release a workspace, prove stopped writers or settle the task.

### 7.1 Default completion policy and automatic settlement (as implemented, D3)

D3 (`crisp-meadow-36.4`, state format 54, `migrations/0054_auto_settlement.sql`) implements the evidence-and-policy completion described at the start of §7, for one policy. The details are in [D3 implementation notes](../implementation/D3.md).

**Policy body.** The default policy applies when the result review is the policy actor and the retained definition body is exactly:

```json
{"schema_version": 1, "kind": "aq.default-completion.v1", "delivery": "required" | "not_required"}
```

Any other schema version, kind or extra field, or a missing `delivery`, means the policy does not apply.

**Gate order.** The gate (`settlement/default_policy.rs`) is pure and reads server-owned evidence only. The worker's report outcome and conclusion are never evidence. The gates run in this order, and the first one that fails decides the verdict:

1. **Acceptance.** The report is the current head, carries a stop proof, and is neither cancelled nor superseded (see §6).
2. **Retained source.** The source has a confirmed aq-git retention receipt.
3. **Required checks.** Each required check has a row in `profiled_check_evidence` bound to (report, source digest, check id, validator digest).
4. **Delivery.** This gate runs only when `delivery` is `required`. It reads `profiled_delivery_evidence`, and only `published` passes.

**Reason codes.** A `rework` verdict means the evidence shows the work is wrong. A `hold` verdict means evidence is missing or not yet determinate.

| Gate | Code | Verdict |
| --- | --- | --- |
| Acceptance | `not_accepted` (refused, cancelled or superseded report) | rework |
| Acceptance | `not_stopped` (no stop proof) | hold |
| Retained source | `source_not_retained` | hold |
| Required checks | `required_check_failed` | rework |
| Required checks | `required_check_evidence_missing` | hold |
| Delivery | `delivery_unpublished` | rework |
| Delivery | `delivery_indeterminate` | hold |
| Delivery | `delivery_evidence_missing` | hold |

A further hold, `graph_version_conflict`, comes from completion delivery rather than the gate (see below).

**Settlement owner.** `profiled_auto_settlements` is the automatic settlement owner. A trigger requires the owner identity `aq.settlement-owner.auto.1` on every write. `settlement_claim_guard` and `profiled_historical_snapshot_guard` accept a confirmed automatic row as settlement, alongside human settlement and B4 replacement release. After a report is recorded, the server attempts settlement. A settlement error is logged and deferred, and never fails the record. Each attempt takes a per-report advisory lock and the row lock, then applies the verdict:

- **Hold or rework.** The row records the verdict, the reason and the evidence: policy actor, policy, report, acceptance facts, source, each check with its validator digest and verdict, and delivery. Nothing is completed and dependents are not released. A later attempt re-evaluates a held row. A `rework` row is terminal for that report.
- **Complete.** The row is prepared with a single version-checked graph `update_node`:
  - the expected version is the task version retained in the attempt's execution spec;
  - the patch is `task_state=completed`;
  - the idempotency key is stable and derived from the report;
  - the payload is at most 8192 bytes.

  The row is committed as `possible` before the request is sent. A valid graph receipt sets `confirmed` and releases the attempt's task hold. Any other outcome is replayed with the same operation ID and payload, at most 8 times. A 409 holds the row with `graph_version_conflict` and is never re-dispatched. Only a new report can settle the task after a conflict, so no read-new-version-and-overwrite loop exists.

**Limits.**

- Rework is recorded in AQ only. No graph mutation reopens or reroutes the task.
- The gate does not consult the effects ledger.
- `profiled_check_evidence` and `profiled_delivery_evidence` have no production writers until the D6 (checks) and D1 (delivery) seams land. Until then, a policy with required checks or required delivery holds, which fails closed.

## 8. Remote graph access

Agents use an AQ CLI or tool adapter that sends outbound HTTPS directly to the central agent API with a scoped session credential. Graph requests are request/response operations and do not wait for worker polling. The daemon checks current authority, maps permitted operations to aq-graph, and returns confirmed results or an explicit pending disposition. Worker node credentials and graph service credentials are never agent credentials.

There is one authoritative graph database. Workers may hold bounded read-only snapshots of task context and immutable revisions. They must expose revision/freshness and cannot use cached authorization. Offline edits are retained proposals, not committed graph state. Reconnection rechecks authority and expected versions before new mutation admission. Terminal-session recovery of already admitted operations uses an authorized node or operator path without renewing agent write permission.

Read and write scopes are separate. A coding agent may read project context and create findings while a planning agent can propose a wider task subtree. Server-side checks cover search results, context expansion, every batch operation, relationship endpoints, and existing author/protected-record rules. Lifecycle changes such as task completion use AQ policy operations rather than raw graph mutation forwarding.

## 9. Required failure scenarios

| ID | Scenario and required outcome |
| --- | --- |
| X1 | Lost acceptance response: recover the original grant/disposition; no second launch. |
| X2 | Expired local reservation and late grant: refuse launch and reconcile authority. |
| X3 | Crash before/after process spawn: prove process ownership or retain uncertainty; never blind relaunch. |
| X4 | Commit followed by agent crash: preserve source and make it explicitly selectable for a successor. |
| X5 | Disconnect/suspend past expiry: stop/preserve locally and reject stale central writes. |
| X6 | Cancellation/completion race: deterministic recorded disposition; preserve late source and avoid duplicate completion. |
| X7 | Old attempt returns after replacement: retain history, forbid settlement of the current attempt. |
| X8 | Daemon restart: recover offers, grants, directives, delivery intents and receipts; workers reconcile outward. |
| X9 | Worker restart with surviving children: establish process identity and renewed authority before reattachment; block unsafe reuse. |
| X10 | Lost graph response or version conflict: retry identical admitted intent or reconcile conflict, without duplicate history or overwrite. |
| X11 | Credentials expire/revoke: stop new agent actions; protected pending artifacts remain recoverable through a separately authorized path. |
| X12 | Central retention succeeds and worker volume disappears: central policy/integration can proceed from retained artifacts. |
| X13 | Ambiguous harness message delivery: report uncertainty without injecting a duplicate instruction silently. |
| X14 | Heavy graph/log/artifact activity: control requests retain reserved capacity and no false lease-expiry cascade occurs under the accepted load. |

## 10. Scale and acceptance profile

Agent count is not request rate. For a sizing example, 500 agents averaging one graph request every five seconds produce 100 agent graph requests/second, before scheduler, playbook and UI traffic. This is an assumption for load modelling, not measured agent behavior.

Use bounded context reads, pagination, small atomic batches, reused HTTP connections and bounded server database pools. Schedule graph reads centrally rather than having every worker scan ready tasks. Heartbeats, logs and token accounting do not continually mutate task records. Avoid a shared mutable project summary as the destination for every agent's progress. Isolate control, graph and bulk-transfer concurrency budgets; apply fair admission and jittered retry across nodes/projects.

The initial product acceptance workload uses 500 simulated agents across 50 logical workers through the real agent API and worker protocol, with actual graph service, at least 100,000 records/500,000 relationships, and declared server resources. Logical workers can share load-generator hosts; this is not evidence of 50 independent host/network failure domains. Run at least two hours with 100 aggregate agent graph requests/second plus declared measured background traffic, and 1,000 graph requests/second offered for ten-second bursts. Use a documented read/write mix, hot-record conflicts, large-but-bounded contexts, coordinated starts, reconnect storms and background maintenance. Test daemon/worker restart and authority correctness separately within the run; genuine outages are reported separately from connected steady-state SLOs.

Initial proposed end-to-end connected-load budgets are graph read p95 <=250 ms (search <=500 ms), confirmed small mutation p95 <=500 ms, p99 <=1 second, and cancellation recognition within an active poll interval plus one second. Declare test network latency, ingress/authentication and payload sizes; these budgets are not guarantees for arbitrary WAN conditions. Reserved control traffic must stay within grant renewal safety margins. Pending mutation acceptance is measured separately and cannot count as confirmed-write latency. At the accepted sustained and burst rates require no dropped arrivals, unexpected errors/timeouts or capacity refusals; deliberately invalid/conflicting requests remain separately counted. Above those rates, test bounded overload refusal and recovery rather than promising unlimited admission. Report retry amplification, request/lock/pool waits and retained disk growth. No acknowledged data loss, stale settlement or duplicate logical effects are permitted.

Current [schema-8 graph evidence](../../../aq-graph/docs/performance/schema8-load-first/README.md) covers 100,000 records and 500,000 relationships on shared desktop hardware: 200 requests/second for 30 seconds and 1,000/second for ten seconds, with about 11/15 ms all-attempt client p95 and expected conflict/validation responses. Fixed 200-client saturation reached about 300 ms p95. This excludes the AQ gateway and WAN and does not replace the product profile. Hardware and final release SLOs must be selected before capacity acceptance; do not infer production sizing from these short measurements.

## 11. Implementation and verification boundaries

The worker uses a fake coordinator, fake clock and deterministic harness for core crash/replay tests. AQ uses simulated workers and a fake graph adapter for pure state-machine tests, then real PostgreSQL/aq-graph for delivery and concurrency tests. Published protocol fixtures bind both implementations. Real process-tree, tmux, container and harness compatibility checks remain separate required adapter evidence. See the [ecosystem design](0001-ecosystem-and-releases.md) for release and test ownership.

## Current implementation and next acceptance boundary

The [2026-10-31 checkpoint](../plans/2026-10-31-overnight-checkpoint.md) records
qualified foundation/recovery, shared authority, graph access, source handoff,
profile selection, native registration, Started reporting, and assigned material/
artifact delivery components. The complete native assignment producer, worker
consumer/harness adapter and result-settlement path still require implementation
and assembled qualification. The [three-area completion map](../plans/0002-execution-sessions-policy-completion.md)
identifies that work. No component receipt or test count closes GATE-04 by itself.
