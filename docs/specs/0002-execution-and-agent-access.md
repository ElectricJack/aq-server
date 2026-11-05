# Daemon execution coordination and remote agent access

**Status:** Current implementation contract; foundations and scoped services exist, native execution/result integration remains open.

**Date:** 2026-10-30

This specification defines AQ's implementation of the shared [execution and recovery contract](../architecture/0002-execution-and-recovery.md). AQ owns scheduling and authority, durable protocol admission, agent-facing graph access, result eligibility and task completion policy. Local execution mechanisms belong to aq-worker. The [ecosystem design](../architecture/0001-ecosystem-and-releases.md) defines dependency and release ownership.

## 1. Scope and requirements

| ID | Requirement |
| --- | --- |
| D1 | Persist assignment authority and resolved inputs; never infer authority from graph readiness or heartbeat liveness. |
| D2 | Make offer, acceptance, cancellation, result and directive operations replayable with stable IDs and explicit conflicts. |
| D3 | Preserve uncertain execution and late evidence without allowing stale task settlement. |
| D4 | Expose scoped remote agent reads/mutations through AQ; no worker graph credentials or database replica. |
| D5 | Persist authorized graph delivery intent, reconcile uncertain responses and respect optimistic versions. |
| D6 | Keep graph content authoritative in aq-graph and task completion subject to central policy. |
| D7 | Reserve capacity for renewals/cancellation and bound other request classes independently. |
| D8 | Support independently tested protocol versions and staged worker extraction. |
| D9 | Expose freshness, pending operations and reasons through the same domain API used by CLI and dashboard. |

No new generic workflow language, second graph store, offline merge engine or direct worker RPC is introduced here. Scheduling optimization remains in the worker framework's placement contract; this spec supplies its durable authority boundary.

The [profile, intelligence and terminal design](../architecture/0003-agent-profiles-harnesses-and-terminals.md) supplies additional requirements P1–P8, H1–H4 and R1–R6. AQ-18/19/20 implement catalog composition, project execution bindings and intelligence selection before real scheduling. AQ-21 supplies authorized remote/central terminal relay; worker directives still arrive only through responses to worker-initiated requests. API-01 access profiles remain separate authorization objects.

The [session operations, waits and notifications design](../architecture/0004-session-operations-waits-and-notifications.md)
adds S1–S5, N1–N4 and C1–C6. AQ-22/23 own normalized observations, accounting and
stall recovery; AQ-24/27 own external delivery and escalation/digest policy;
AQ-25/26 own verified waits and continuation scheduling. Their CLI/API handlers
share the daemon's domain admission paths. Sleeping continuations retain no live
execution grant; their wake uses fresh attempt authority and the original lineage
budgets. Notifications, transcripts and source events cannot manufacture task
completion, human decisions or stopped-writer evidence.

## 2. Internal modules and storage

Implement narrow modules for placement, execution authority, worker exchanges, agent access, graph delivery, artifact handoff and result policy. Modules may be crates inside `agent-q` when useful; they do not each require a repository or service. Runtime/harness code is not linked into these modules.

Durable AQ records include:

| Record | Required contents |
| --- | --- |
| Node incarnation | Node ID, active boot ID, negotiated capabilities, last observation and restrictions |
| Attempt | Task ID, immutable input/evidence references, session, lineage, budgets and resolved spec digest |
| Offer/reservation | Target node/boot, expiry, spec digest and central allocation identity |
| Grant | Attempt generation, node/boot, sequence, validity and revocation/admission state |
| Observation | Attempt/generation, worker sequence, observed phase/process evidence and received/observed times |
| Directive/message delivery | Stable ID, scope, payload digest, ordered delivery cursor and acknowledgements |
| Final report/disposition | Immutable report bytes/digest, source receipts, stop certainty and acceptance decision |
| Domain operation | Caller attribution, scoped request digest, expected versions, dispatch/recovery state and result |
| Event outbox | Stable authoritative event ID and delivery disposition to registered consumers |

Use the daemon's durable PostgreSQL ownership boundary for central state. Graph records remain outside it. Constraints and compare-and-set transitions prevent duplicate acceptance or settlement under concurrent requests. Transport handlers must not rely on a process-local mutex for correctness across restarts. Scheduling indexes may cache graph-ready candidates, but execution admission fetches/rechecks relevant current inputs; readiness is not a graph execution claim.

A graph edit and execution admission cannot share an atomic transaction. Record the input revisions admitted for each attempt and reconcile later graph changes through AQ policy. An invalidating requirements/dependency change can close further admissions and cancel or supersede affected attempts; it does not silently rewrite a running agent's specification. Result acceptance and completion recheck the relevant current conditions, so a concurrent graph change cannot be bypassed merely because execution already began.

## 3. Worker request admission

Authenticate node credentials before inspecting assignment payloads. Enforce active incarnation, endpoint scope, negotiated protocol, bounded payload size and monotonically valid generations. A cloned identity cannot register two active runtimes without an explicit replacement/recovery decision.

Persist the offer before publishing it through polling. Acceptance serializes against offer expiry, node restrictions, cancellation and any conflicting current assignment. The recorded result binds the exact request digest; reusing an ID with different inputs fails. Only a committed grant can authorize execution.

For native profiled execution, acceptance records the no-launch reservation and
original PreparationPlan, including the exact admitted private-material leases.
The plan authorizes only scoped preparation. Grant issuance is a subsequent
transaction after verified source/material/journal receipts and current
eligibility checks. Keep the qualified fixture protocol unchanged rather than
using its acceptance/grant shortcut for a real harness. The updated
[shared handshake](../architecture/0002-execution-and-recovery.md#3-assignment-handshake)
defines the native ordering.

Current native authority comes from the adopted compact hot row; immutable launch
and selection rows remain sealed. Consumers use the common AuthorityView and
recheck authority after relevant waits. Producer-generated artifact links define
the assigned artifact allowlist, so the producer must validate typed semantics,
project scope and nonsecret content before linking. Inline retention verifies
bytes/receipt integrity, not arbitrary content classification. Provider secrets
use the separate private-material owner; Git bundle retention uses aq-git.


Progress writes are idempotent by attempt, generation and sequence. Older observations remain historical and cannot regress the current projection. Directive acknowledgements cannot skip undelivered gaps; polling supports bounded ordered replay. Acknowledging receipt is distinct from confirming a directive's physical effect.

Renewals use monotonic grant sequences and the shared timing rules. Refuse stale node boots or generations and return structured reasons. The independent admission budget for heartbeats/renewals cannot be consumed by graph queries or uploads. Node offline classification affects new placement and presentation; it does not mark an attempt stopped.

### 3.1 Multi-project fair share

The native offer producer implements the central ordering in the worker
framework's [placement order](../../../aq-worker/docs/specs/0001-worker-framework.md#82-placement-order)
(§8.2, steps 1 and 5) across enabled projects. Code:
`profiled_execution/producer/fairness.rs` and `producer/workflow.rs`.

- **Effective priority.** Configured project priority plus 1 point per second
  waited since that project's last recorded service.
- **New projects.** A never-served project counts as having waited exactly the
  starvation bound. It is not starved, and it does not jump ahead of projects
  that are already starving.
- **Starvation bound.** A hard 10 minutes. Any project waiting longer goes ahead
  of the priority order, oldest first.
- **Ties.** Broken on the oldest last service, then project ID.
- **Per-poll limit.** Each poll tries at most 4 projects in that order and stops
  at the first eligible task.
- **Charging.** A project's service is recorded before its Graph ready read. A
  project with no ready work or a failing owner read therefore cannot hold the
  head of the order and starve the others.

The only durable input is `profiled_project_schedule`, which stores the last
service time per project. It never moves backwards. Ordering itself is pure
server code. Fairness applies only between projects; task-level priority within a
project and per-project budget configuration are separate follow-ups.

### 3.2 Sandbox placement policy

C4 adds an optional `placement` table to each `[[profiled_execution.projects]]`
entry (`ProjectPolicy`). It orders backends and states the capabilities the
project requires on B1-registered sandbox nodes:

```toml
[profiled_execution.projects.placement]
preference = ["docker-local", "boat", "cloudflare-containers"]
required_capabilities = ["vm-isolation"]
```

- **`preference`.** Distinct backend kinds from the closed set `docker-local`,
  `boat` and `cloudflare-containers`, at most 3 of them.
  - An empty or omitted list uses the default shown above.
  - Kinds the list leaves out rank last.
- **`required_capabilities`.** At most 4 B1 capability names (`vm-isolation`,
  `nested-docker`, `suspend-snapshot`, `persistent-disk-during-attempt`).
- **Validation.** Unknown fields, duplicate kinds and lists over these bounds
  refuse the configuration.
- **Digest stability.** An absent `placement` is not serialized, so existing
  project configuration digests do not change.

The policy feeds selection-time placement facts. The rules for the bounded
preference hold, node ceilings and typed refusal reasons are in
[intelligence selection](0004-intelligence-selection.md#sandbox-placement-facts-c4).
They are also summarized in
[architecture 0005 §3.2](../architecture/0005-sandboxed-execution-and-deployment-topology.md#32-scheduling-across-workers).
Host-native nodes, which have no B1 registration, ignore this policy.

## 4. Agent-facing API and credential provisioning

Publish a separately versioned agent API with schemas and CLI/tool bindings. Agents make direct outbound HTTPS requests to AQ. No inbound node listener or tunnel is required. Reuse typed domain service handlers for CLI, dashboard and playbook commands, with explicit caller-context differences; do not implement authorization independently in each frontend.

The daemon provisions a per-session credential to the authenticated node through a protected response/channel after authorization. It binds session, attempt/generation, subject identity, project/workspace scopes, permitted operations, expiry and credential version. The worker supplies a private token file or equivalent harness-supported secret binding. Tokens do not appear in instruction text, command arguments, logs, artifact manifests or graph records. Store credential verifiers centrally and support rotation/revocation without changing node identity.

Authorization checks current central assignment state, not merely token signature/expiry. Short credential lifetime does not replace online revocation checks. Scoped read permission, ordinary mutation permission, and lifecycle control are independent. New writes from expired, cancelled or superseded sessions are refused even if the process is still running.

Admission of an operation is the authorization boundary. An operation durably admitted before cancellation may already have reached aq-graph. Reconciliation of that exact intent can continue under its recorded authority, including after session expiry; it cannot broaden its payload or create a new action. An undispatched operation may be cancelled only when the delivery state proves it was not sent. Expose these distinctions to the caller.

Finished or revoked execution sessions do not receive fresh graph write credentials to recover an unknown response. An authenticated node recovery endpoint can retrieve the dispositions of its already-associated operation IDs; it cannot submit arbitrary agent actions or read unrelated graph content. Operators have a separately authorized recovery interface.

## 5. Read scopes and context

Read operations cover task/knowledge retrieval, bounded context, search, revision/history and permitted relationships. Reuse aq-graph's public schemas where appropriate; AQ supplies execution-aware access policy and a bounded projection. Avoid a generic forward-any-route proxy.

Every response must satisfy the caller's read scope. Context traversal, search, pagination and relationship endpoints must not disclose inaccessible records, titles, counts or snippets. Limits apply before unbounded expansion; an empty filtered page is not evidence that the graph has no matching records. Return explicit truncation/coverage and continuation semantics. Do not declare an observation complete when an authorization filter or scan budget left coverage unknown.

Assignment context captures exact references where possible, with observation time and truncation. Mutable current-record reads identify their version. Worker/browser caches are disposable, scoped to the authorized principal and revision, and invalidated on credential/scope changes. Cached content cannot authorize a later write. AQ should not persist full graph replicas to speed ordinary reads.

## 6. Mutation policy and graph delivery

| Operation class | AQ admission policy |
| --- | --- |
| Findings/knowledge creation | Explicit project scope, bounded content, provenance and permitted authorship |
| Authorized record correction | Narrow field allowlist, expected version, author/protection rules |
| Follow-up/decomposition | Typed proposal or permitted draft creation; validate all records/edges atomically where supported |
| Requirements/dependencies/priority | Explicit role capability and impact/version checks; no implicit running-session reconfiguration |
| Complete/cancel/reopen task or approve evidence | Domain lifecycle operation with applicable policy and decision authority |
| Assignment, worker or publication control | AQ-specific typed operation; never arbitrary graph field mutation |

The graph adapter authenticates as an AQ service principal scoped to the relevant workspace and privilege class. Session/task attribution is provenance, not impersonation of a different aq-graph actor. The daemon enforces any narrower per-agent author policy before dispatch, and aq-graph still enforces its own service-principal permissions and invariants. Do not use an administrator token for ordinary worker requests. Privileged domain actions use separately scoped adapters when required.

Before dispatch, persist a domain operation containing its ID, canonical payload digest, expected versions, service-principal mapping, agent provenance, authorized scope and policy/input references. The graph request carries a stable derived idempotency key. Reads may be synchronous; mutations expose confirmed, pending, conflict, rejected and failed/uncertain dispositions with a queryable operation ID. A pending response is not mutation success.

Use the following delivery rules:

1. Commit authorized intent before sending the request. Serialize dispatch/cancel state transitions so cancellation cannot falsely prove that a potentially dispatched request was unsent.
2. On success, persist the exact graph receipt and versions, then publish the confirmed domain event through an AQ outbox in the same local transaction.
3. On timeout or crash, query or replay the identical request/key. AQ and aq-graph do not share a transaction.
4. On version conflict, stop that intent and return the conflict. Fetching a newer version is context for a new deliberate decision, not permission to overwrite.
5. On authentication, invariant or permanent validation failure, record the reason without retrying indefinitely. Temporary failures use bounded backoff and an inspectable recovery queue.

The graph's receipt retention contract bounds safe replay. Preserve the service identity and payload required to recover admitted requests. Credential rotation/revocation must have a recovery plan for pending operations. If the service cannot prove prior application after receipt retention expires or identity changes, do not invent a fresh key; retain uncertainty for reconciliation.

Atomic graph batches are used for bounded related changes within one graph workspace. They do not atomically commit AQ assignment state, Git effects or playbook state. Return the actual graph result before projecting confirmed graph lifecycle. The domain event consumer deduplicates by event ID so outbox retries do not create repeated workflow effects.

## 7. Results and task completion

Authenticate report and upload scope against retained assignment identity. A stale attempt may be permitted to transfer its own bounded recovery artifacts through node authorization; this does not restore its agent write authority. Bind upload role, project/repository, size limits and destination to server-owned metadata. Never forward worker-chosen Git commands, paths or remote URLs.

Import required source centrally and obtain aq-git's retained receipt before claiming source durability. Validate report identity, generation, spec digest and stop evidence. Record separate accepted, rejected/stale or needs-reconciliation dispositions. Result acceptance means eligible evidence, not automatic task completion.

Task completion evaluates the pinned outcome contract and current policy against applicable source/evidence versions. Playbooks may request reviews, checks or decisions through typed host commands. The authorized completion action then uses the graph delivery mechanism. If graph state changed, policy reevaluates; it does not force completion. A successful harness exit alone never performs this mutation.

Cancellation and result acceptance follow the shared serialized ordering rule. Existing completion/publication intents are shown explicitly when a later cancellation cannot stop them. Pending operations and stale source remain inspectable and have explicit retention owners.

### 7.1 Default coding binding `aq.coding` (as implemented, D9)

`aq.coding` is the default playbook binding for coding work. `CodingPolicy` (`playbooks/coding.rs`) builds it, and the owner installs and activates it through `POST /api/playbooks/coding/default` ([architecture 0004 §5.2](../architecture/0004-session-operations-waits-and-notifications.md#52-owner-artifact-install-and-binding-activation-as-implemented-d9)). The artifacts use only the pinned Host7 catalog commands `work.ensure`, `check.request`, `candidate.prepare`, `candidate.publish` and `operation.get`, plus `decision.request` when a human decision is configured. Each artifact's registry is pruned to the contracts it uses. D3's `aq.default-completion.v1` remains the automatic-settlement policy ([architecture 0002 §7.1](../architecture/0002-execution-and-recovery.md)); `aq.coding` does not reroute settlement.

**Policy.** `CodingPolicy { implementer_family, reviewer_family, checks, max_repairs, human_decision }`.

- `checks` is a list of required checks. Each is pinned to an exact validator id, version and digest.
- `CodingPolicy::project_default(checks)` uses an `anthropic` implementer, an `openai` reviewer, 2 repairs and no human decision.

`build()` refuses a policy when:

- either family is blank, or the two families are equal after trimming and case folding;
- `max_repairs` is more than 8, there are more than 32 checks, or two checks share an id;
- `human_decision` is set and the grant cannot hold `decision.request`. Host7 grants cannot, so `coding/default` currently refuses such a policy (`NotGranted`) rather than silently dropping the step. `build_for(granted)` builds it for a grant that includes the capability.

**Artifacts.**

| Artifact | Behaviour |
| --- | --- |
| `aq.coding.work` | One `work.ensure`, completed synchronously or through `accepted` followed by `aq.operation.updated`. Returns `accepted`, `rejected` or `escalated`. |
| `aq.coding.check` | One `check.request`, completed synchronously or through `accepted` followed by `aq.check.receipted`. Returns `passed`, `failed` or `escalated`. |
| `aq.coding.decision` | Present only with `human_decision`. One `decision.request` on the reviewed candidate, with choices `approved` / `rejected`, completed synchronously or through `aq.decision.completed`. Refused, expired or cancelled requests are not a human verdict and escalate. |
| `aq.coding.attempt` | One attempt, in this order: work (implementation or repair), then `candidate.prepare`, then every required check, then the cross-family review (`work.ensure kind=review`), then the optional human decision, then `candidate.publish`. Returns `published`, `rework{reference}` or `escalated`. |
| `aq.coding` (root) | Attempt 0 is the implementation. It is followed by up to `max_repairs` repair attempts, each given the previous attempt's failing `reference` as its `context`. Returns `published`, `exhausted{reference}` or `escalated`. |

**Per-attempt evidence rules.**

1. **Evidence belongs to one prepared candidate.** Every attempt prepares a new candidate. Every required check and the review run on that candidate's subject. Each evidence entry carries the candidate's revision. A subject change therefore invalidates all earlier evidence, and nothing is carried over between attempts.
2. **Publish carries only this attempt's evidence.** The `candidate.publish` evidence is the attempt's check receipts in policy order, then its review reference, then its human decision reference if configured. The host's publication guard verifies each entry in its owning store.
3. **Checks run before review, and review never substitutes for a check.** A failed check ends the attempt with `rework` naming that check receipt. Review is not requested for a candidate that failed a check.
4. **Review is cross-family.** The review context names both families. The reviewer family must differ from the implementer family.
5. **A human decision follows review and never replaces it.** The decision is requested only after the review accepts. `approved` publishes. `rejected` ends the attempt with `rework` naming the decision. Nothing auto-approves.
6. **Every refusal is explicit.** These outcomes are `rework`: a rejected work result, a `candidate.prepare` conflict, a failed check, a rejected review, a rejected decision and a `candidate.publish` conflict. Each `rework` names the failing evidence as `reference`, which becomes the next repair's context. Every unexpected outcome is `escalated`. If the last permitted attempt also returns `rework`, the run returns `exhausted` with that reference and does not publish.
7. **Retries are idempotent.** `CodingPolicy::input` gives each attempt `i` the logical keys `{key}/a{i}/work` and `{key}/a{i}/review`. A replayed attempt therefore ensures the same work and review operations instead of creating new ones.

**Current limits.** The check `source` comes from the run input, because `candidate.prepare` does not return a source OID. Real Host7 `work.ensure` returns `ensured` without a completion, so runs escalate until the D5–D8 runtime emits `aq.operation.updated` / `completed`. Details are in the [D9 implementation notes](../implementation/D9.md).

## 8. Recovery and explanation API

On daemon restart, reconstruct pending offers, grants, directives, graph deliveries and result dispositions before issuing conflicting new work. Use persisted deadlines and current time conservatively. A restart cannot extend a grant by replaying its previous response. Worker reconciliation reports process evidence and journaled operations; daemon responses select continue-under-new-authority, stop, retain, or retry-known-report actions.

Expose task/attempt explanations containing: current authorization, latest worker observation/time, input revisions, active holds, pending delivery IDs, retained evidence, retry budget, disposition reasons and the action/authority needed for progress. Resource observations and complete domain facts are labelled separately. API clients display pending operations and stale observations without inventing success.

The first implemented browser read profile is `dashboard-read-1`, with capability
version `1`, published under [protocol/dashboard-read/v1](../../protocol/dashboard-read/v1/README.md).
Authenticated bootstrap, bounded session list/detail, owner-only configured
projects and a partial overview reuse current human identity and exact-session
inspection ACLs. Existing status/worker reads retain their own routes and ACLs.
Unknown process activity and aggregate counts remain unknown; unsupported task,
decision, search, supervisor, quota and terminal views are explicitly unavailable.
Optional verified static assets are served only by the human listener. This
subset does not satisfy the broader explanation/action contracts above or close
API-02/UI-01; exact source and current qualification are recorded in the
[dashboard integration checkpoint](../implementation/UI-01-live-dashboard.md).

Logs carry correlation IDs but exclude secrets and unauthorized content. Detailed protected reports require scoped reads. Bulk telemetry retention is bounded and cannot prevent critical control admission.

## 9. Testing and implementation order

Start with pure authority transitions and a fake clock/store boundary, then add transactional persistence and a fake worker. Implement a fake graph adapter that can commit before losing its response. Add real aq-graph contract tests before connecting live agents. Preserve current foundation routes and node credential behavior while adding a negotiated execution API.

| Test | Requirements and shared scenarios |
| --- | --- |
| Concurrent accept/cancel/expiry and changed-payload replay | D1–D2; X1, X2, X6 |
| Reconcile stale node boots, stopped/uncertain attempts and retry budgets | D3; X3–X9 |
| Agent permission matrix across context, search, batch edges and protected fields | D4; X5, X11 |
| Commit-before-response-loss, receipt replay and completion conflict | D5–D6; X10 |
| Credential expiry during an admitted operation and node-only recovery | D4–D5; X11 |
| Retained source independent of worker and stale result exclusion | D3, D6; X7, X12 |
| Real outbox replay to a deduplicating playbook host | D2, D6; X8, X10 |
| Control admission while graph and transfer budgets saturate | D7; X14 |
| Current/previous supported protocol and foundation worker compatibility | D8 |
| CLI/dashboard explanations show pending, stale and unknown accurately | D9 |

Component-level tests do not require real model providers or production worker containers. Product tests and the shared 500-agent profile exercise the actual adapters and retained artifacts. Exact API route names and generated schemas are required implementation deliverables, not claimed as shipped interfaces by this design.
