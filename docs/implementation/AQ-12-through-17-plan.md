# AQ-12 through AQ-17: embedded policy and central supervision

This plan is pinned to daemon0551d84c0ba9653da8310d0596a833182fe73341/state20 and playbooks42f944996eafad5af07d9ec1c72d3e3f2630c03b. It supplements the [completion scope](../plans/0002-execution-sessions-policy-completion.md); canonical dependencies/status stay in the task graph. Runtime adoption, adapters and pilot acceptance have separate gates.

## Component pin and ownership

Consume the existing public source-archive publication profile, with model/compiler/runner/Postgres package identities0.1.0. Vendor exact committed source bytes and workspace metadata, inventory/hash the closure and qualify it independently; no dependency may point at a live sibling checkout. Do not mutate an old same-version package. Current42 production src/manifests/migrations are identical to qualified PB04 stageA7e669e179e6cc726185ac80627a6bdca146944f1; only experiment examples/scripts changed afterward. The CLI's synthetic catalog/fake host are not runtime dependencies or production registrations. Preserve the frozen source/contract fingerprints on activation and intent receipts.

AQ owns one explicitly configured namespace and unique runtime incarnation. The engine's `aq_playbooks` schema and storage adapter own workflow state, receipts, artifacts, waits, indexes and namespace advisory lock; AQ does not introduce another editable workflow-state database. AQ-owned effect authorization, domain receipts and event outbox are separate domain records. Their schemas/migrations belong to AQ, never private-table imports into the engine. An installation starts disabled unless its namespace, bounded configuration and protection policy are explicit.

## AQ-12: bounded embedding foundation

1. Add an isolated `aq-playbook-host` crate consuming exact source pins. Put every synchronous storage/engine/host call on a dedicated owned thread, not a Tokio request/control worker and not the general blocking-pool queue. A bounded request channel refuses overload without a hidden unbounded waiter list; reserve a separate control channel for cancellation/shutdown. Async callers await bounded acknowledgements and can time out without assuming the underlying operation stopped.
2. Own the namespace for that thread's lifetime. Acquisition/startup failure is visible. A timed-out or stuck shutdown leaves ownership unknown and prohibits replacement until the old worker is known retired; do not detach an old owner and automatically start another. Database session loss fences subsequent CAS. SQL connect/lock/statement deadlines are separately configured; no storage/host transaction is held across external service effects.
3. Require configured storage protection for protected artifacts/namespace use. Keys come from private no-follow bounded files/keyrings; no plaintext/key fallback, key bytes in status, startup log or workflow data. Use the existing storage protection/rotation API. Production remote PostgreSQL uses a host-configured TLS client rather than pretending `NoTls` is transport protection.
4. Expose bounded install/activation/event admission/query/cancel/tick operations. Validate payload bytes and finite count/step budgets before enqueue. A driver quantum uses the engine's public bounded transition API; recovery has its own bounded quantum (the current public `recover` scans up to64 effects). Full snapshot/storage costs remain explicit, with no25ms or500-agent claim. Shutdown/control is checked between quanta; a currently blocked synchronous call cannot be forcibly interrupted safely.
5. Provide daemon lifecycle/config wiring only after foundation qualification. Default registration is empty and unsupported adapters refuse, so development fixtures cannot run as production commands. The daemon's current control admission/separate execution pool remains independent. Prove actual heartbeat/lease/cancel progress while the playbook worker's storage and host are held. This is isolation evidence, not a hard deadline for an uninterruptible playbook operation.

Initial foundation profile is finite and conservative (one namespace/thread, bounded queued requests, per-request JSON cap, bounded transitions/tick and namespace quotas). Exact tested values and overload behavior belong to implementation evidence. PB04 still fails the25ms persistence budget; bounded embedding does not close that performance gate.

## Durable domain bridge and event outbox

Before a domain effect, persist one AQ intent keyed by namespace/incarnation binding, runner Intent.id, exact registered command fingerprint, canonical payload digest, original authenticated principal/scope/grant revision, target revisions and bounded deadline/retry budget. Same identity with different bytes conflicts. Never trust actor/capability fields inside workflow input. Domain admission and its event-outbox entry commit together in the owning AQ transaction. The engine and AQ domain transaction are not one distributed transaction: a stable event/producer/epoch identity retries admission after crash, and only durable engine acceptance acknowledges the outbox row. Bounded paging, per-producer cursors, restart/late ACK and explicit epoch retirement prevent loss or duplicate root runs. No second event history replaces engine storage.

Each registered adapter implements authorize/dispatch/reconcile/cancel:

- Authorize resolves the current authenticated principal and exact contract capabilities/scope; it rechecks object versions and grant expiry after lock waits. It does not grant capabilities from graph-read permission or a requested command string.
- Dispatch admits only the retained exact payload/operation identity into its owning service, returning a durable handle or a truthful completed/refused/unknown result. Acceptance is not final work completion, harness delivery, human acknowledgment or publication.
- Reconcile looks up the retained owner operation and original bindings. It never invents a new payload/operation after unknown response. Existing-operation recovery uses a separately scoped owner recovery principal, including after ordinary execution authority expires; that principal cannot admit a new business effect. Effects should use reconcilable contracts so recovery does not silently turn unknown writes into fresh dispatch. The runner's authorization gate must be driven in explicit dispatch versus recovery modes; the domain adapter still validates the full Intent on lookup.
- Cancel uses the engine's stable cancellation key, persists request before effect, and distinguishes accepted/refused/too-late/confirmed/unknown. Confirmation comes from the domain owner. Cancellation cannot erase a possibly completed effect, manufacture stopped writers or reopen a task.

The bridge rejects unavailable/unregistered commands rather than registering a wildcard, synthetic fallback or generic endpoint/SQL proxy. Installation/activation records the exact finite registry and dependency hashes; no registration of all daemon methods.

## Exactly twelve command mappings

| Command | AQ owner and finite implementation boundary | Recovery/cancel evidence |
| --- | --- | --- |
| context.read | AQ-13 finite work/project/execution-options/evidence/health views; public graph adapter, project bindings, selection and retained execution/observation views | Exact snapshot/revision/completeness; no arbitrary URL/SQL; current visibility |
| work.ensure | AQ-13 AQ-owned bounded work admission using public graph identities and validated proposed graphs; selection/execution constraints reuse AQ18/20/AQ08 | One stable operation/task identity; admission distinct from execution and AQ10 completion |
| work.change | AQ-13 closed requirements/priority/dependency/constraints/owned-hold variants; existing graph CAS/ownership policies and task-domain service | Expected versions; no lifecycle patch or completion shortcut |
| judgment.request | AQ-14 bounded assessment via AQ17 central session owner and existing selection | Durable handle; no model output as human approval; stop/late result distinct |
| decision.request | AQ-14 authenticated eligible human decision owner; scoped AQ24 notification only carries a link | Exact subject/revision/choices/expiry; transport acceptance is not a decision |
| check.request | AQ-14 registered executed check against immutable source/context | Retain execution/result evidence; cancellation and unknown process/source effects explicit |
| candidate.prepare | AQ-15 central AQ09/aq-git public immutable candidate/source/base APIs | Stable operation and conflict result; no worker publication |
| candidate.publish | AQ-15 conditional destination update through same central integration owner | Applicable review/check/decision evidence plus current destination; lost reply reconciles original operation |
| message.send | AQ-13 durable AQ11 task/session mailbox or AQ24 human-message intent, with explicit typed recipient/visibility | Stable message identity, accepted/node/harness/unknown/human-ACK distinctions; no Discord call in runner |
| findings.reconcile | AQ-16 sole AQ finding-episode/pre-scan revision/reminder/suppression owner; atomic AQ24 notification intents | Incomplete scans cannot clear absent findings; same episode/message replay |
| operation.get | AQ-13 finite tagged operation-owner handles, scoped retained status/results | No arbitrary service name; unknown/stale remain explicit |
| operation.cancel | AQ-13 delegates to tagged owner; AQ14/15 services implement corresponding cancellation semantics | Stable separate cancel identity; acceptance never means stopped/completed |

AQ07 currently supplies finite agent graph intents/recovery, not a universal central work service or permission to complete tasks. AQ11 currently supplies task/session mail and node delivery, not an already implemented supervisor authority adapter. AQ18/20 provide actual catalog/selection ownership, not another selector inside playbooks. AQ24 supplies bounded human transport/unknown recovery, not decisions, finding episodes or report-window policy. The host must extend these owners through explicit public service interfaces; duplicating their tables/policies inside the engine is prohibited.

## Sequenced implementation and proof

| Stage | Deliverable | Qualification gate |
| --- | --- | --- |
| AQ12 foundation | Exact vendored source pins, owned bounded thread, protected namespace backend, empty production registry, bounded public engine operations | Real runner/storage ownership and restart, changed artifacts, blocked-storage/host control isolation, overload/shutdown uncertainty; protected storage tests |
| AQ13 | Context/work/change/message/operation service interfaces, current authority adapter, AQ domain intents and outbox dispatch/replay | Real selected graph and disposable PostgreSQL; expiry/revocation/revision/changed-ID conflicts, crash between owner commit and engine event ACK; no synthetic production registration |
| AQ17 first central adapter | Central expiring session authority and private credentials using qualified pinned harness/terminal/agent API packages | Depends on genuine native execution/Started and WRK08/10; central identity is not a fake node; stop/restart uncertainty and remote scoped messaging |
| AQ14 | Bounded assessments, revision-bound genuine human decisions, executed check evidence | Separate machine/model/human authority, late results, cancellation and check/source pins |
| AQ15 | Candidate prepare/publication policy | Disposable graph/Git pilot; source/base/destination CAS, new evidence after conflict repair, lost publication reply |
| AQ16 | Complete finite observations, finding episodes, durable calendar/window/report cursors | Partial scan refusal, reminder/retry identity, timezone/misfire, fallback/late author no double-send; reuse AQ24 |
| Applicable AQ27 policy | Escalation acknowledgment/reminder/resolution and digest/report delivery | Existing12 commands, actual configured fake external peer; no false human ACK or transport finality |

Each stage freezes source, service/contract/archive hashes, exact commands, disposable fixtures and negative evidence before claims. The coding/review/check/publication, routing, findings-watcher and supervisor-report pilots run against the actual assembled owners only when their prerequisites exist. Fake-host engine pilots remain useful component evidence, never AQ integration evidence.

Full wait-source integration, agent hibernation/continuation, AQ27's wait portions, GATE05 and INT03 remain blocked on their full owning interfaces. They are not closed by omitting waits. No provider/Discord/production publication/deployment is authorized by this plan. PB04's unchanged25ms gate and missing normal workload envelope remain open separately.

## First implementation slice for review

Add only the exact vendored source closure and `aq-playbook-host` foundation,
with daemon dependency/test wiring through narrow coordinated Cargo edits. The
public foundation uses an empty production command catalog and a refusing host;
command-bearing artifacts cannot be activated as production policy. Internal
unit fixtures may exercise held Host calls but never register them in the daemon.
Pure workflows, immutable install/activation, bounded admission/inspection and
namespace ownership use the actual public runner/storage interfaces. No work,
message, graph, decision, check or finding owner is recreated in this slice.

A dedicated actor owns engine and storage; two bounded queues separate work from
control and check control before each bounded driver quantum. Acknowledgment
timeouts are explicit unknown/pending, not safe retry permits. A stuck call does
not consume Tokio control threads/pools, and shutdown does not certify a stopped
owner until its actual thread exits. Tests include bounded overload, cancellation
queue priority, real runner immutable/restart semantics, exclusive PostgreSQL
namespace ownership/protection, and an actual AQ control listener progressing
while the policy actor is blocked. Production service config/startup mounting is
not implicit in adding the crate and is coordinated separately.
