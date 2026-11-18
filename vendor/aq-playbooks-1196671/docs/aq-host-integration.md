# AQ host integration boundary

The Rust engine is independently buildable. Its production host is AQ, which must bind the twelve contracts to authoritative domain services and drive the embedded runner. A passing fake-host test is not evidence that these production services exist.

## Current dependency and implementation status

This boundary is reconciled on2026-10-31 against committed daemon
`0551d84c0ba9653da8310d0596a833182fe73341` (state20), rather than the old
foundation-only inventory. AQ now has durable agent graph operations/recovery,
task/session messaging with distinct node/harness dispositions, revisioned
profile/project bindings and intelligence selection, human notification
transport/unknown recovery, and session/observation/transport services. Those are
owning services to reuse; their existence does not mean the twelve playbook
adapters or central supervisor authority are already implemented.

| Existing AQ owner | Reuse and remaining boundary |
| --- | --- |
| `agent_access::mutations` | Finite scoped graph intents, guarded versions and retained operation recovery; not arbitrary work creation, task completion or a central-supervisor credential |
| `agent_access::messaging` | Immutable task/session mail, scoped visibility and node/harness delivery; central supervisor/playbook authority adapter still required |
| `profile_catalog`, `project_bindings`, `intelligence`, `profiled_execution` | Existing catalog, selection, policy/input/Started ownership; no second selector or fabricated worker assignment inside policy |
| `notifications` | Durable bounded human delivery and unknown recovery; human acknowledgment/decision, episode and report-window policy remain separate |
| `observations`, `terminal` | Typed finite inspection/control seams; native source/runtime adoption and full waits/hibernation remain separately qualified |

Current code registration is in [daemon lib.rs](../../aq-server/crates/aq-server/src/lib.rs).
The detailed staged [AQ-12-through-17 plan](../../aq-server/docs/implementation/AQ-12-through-17-plan.md)
maps exact commands, authority, outbox/recovery and qualification gates. The
shared [ecosystem ownership](../../aq-server/docs/architecture/0001-ecosystem-and-releases.md)
still applies: AQ owns product/domain integration; the runner never reads AQ's
private domain tables or recreates their state owners.

The component publication profile is a pinned source archive/inventory and
commit, not a registry-publication claim. Current playbooks source
`42f944996eafad5af07d9ec1c72d3e3f2630c03b` retains public package identities0.1.0;
its four library src/manifests/migrations are byte-identical to qualified
PB04 stageA`7e669e179e6cc726185ac80627a6bdca146944f1`. Later differences are
unlinked experiment examples/scripts. AQ must vendor/verify exact committed
source closure without sibling live paths; it must not mutate same-version
published artifacts or link the CLI's synthetic catalog/fake host as adapters.
PB04 still misses its25ms budget and has no qualified normal workload envelope.
Bounded embedding/isolation is not performance closure or a500-agent claim.

## Host responsibilities

| Boundary | Required AQ implementation |
| --- | --- |
| Registration | Capture exact typed command/event fingerprints, finite context views and work-change variants; never register every daemon method automatically |
| Authority | Authenticate caller/producer separately from payload fields; retain actor, exact scope and capability ceiling; recheck current grants and object revisions; stamp envelope subject/revision consistently with authoritative payloads |
| Work and judgments | Durable work admission and terminal results, independent reviewer/disposition checks, bounded model assessments, truthful operation handles and cancellation |
| Decisions and checks | Authorized revision-bound human decisions and executed check evidence; neither can be replaced by an agent-completed document |
| Candidates | Use the integration owner's public services and aq-git's immutable candidate/conditional publication contract; independently enforce evidence and destination preconditions |
| Observations and findings | Frozen finite complete observations, pre-scan episode revisions, trusted evaluations, atomic finding changes and notification intents |
| Messaging | Stable identities, scoped visibility, idempotent enqueue, separate acceptance/delivery/acknowledgment |
| Delivery | Commit domain transition and event outbox together; retry stable producer/event/epoch identities until durable runner admission; the engine drops old replay identities by itself (the most recent `replay_keep` keys within their window), so retiring an epoch is not needed to free storage (it still refuses everything from older epochs) |
| Runtime | Own one engine namespace, drive bounded ready/timer/effect queues, publish the scheduler's durable calendar occurrences, reconcile uncertain operations, and provide backups and configured protection keys; export process metrics to a retained telemetry backend |
| Release | Pin released library and contract versions; run assembled-product acceptance without path dependencies on sibling source checkouts |

The engine exposes `Host::authorize`, `dispatch`, `reconcile`, and `cancel`. Implementations dispatch only the exact registered command contract, preserve `Intent.id`, input digest and original authority, and return truthful typed outcomes. `Storage` provides atomic CAS, indexed discovery and bounded safe inspection. Readback/recovery remains host-authoritative even when an operator records an evidence-backed recovery request through the CLI.

The synthetic contract catalog of `aq-playbooks-cli` and the stateful fake host in its test support (`crates/aq-playbooks-cli/tests/support`) are executable integration fixtures. Their checks cover the pilot workflows (the routing pilot was retired in favour of authored routing), source revisions, findings completeness, notification recovery, visibility, publication conflicts and late outcomes. They are not adapters for existing daemon endpoints.

## Live acceptance still required

### Session operations, waits and human delivery extension

The required [AQ operations/waits design](../../aq-server/docs/architecture/0004-session-operations-waits-and-notifications.md)
adds typed session/usage/wait/delivery context and events through versioned host
catalogs. It preserves the twelve ordinary commands. `message.send` and
`findings.reconcile` route through AQ's durable delivery services;
`decision.request` keeps authenticated human responses separate from transport
acceptance. Digest/escalation policy consumes those same episode and schedule
identities, including late author and uncertain external-send recovery.

Engine-native waits suspend workflow execution. An agent's self-suspension,
checkpoint, source retention and wake are AQ execution-domain operations with
their own scoped agent API; the runner does not own a parked process or duplicate
the continuation registry. Registered operation results/events expose their
truthful outcomes. A dependency notification does not itself prove source success.
Stall mechanics remain in AQ, with policy events available to workflows rather
than an unrestricted session-control command family.

AQ-27 qualifies these production services using the unchanged runner primitives;
INT-03 adds actual external delivery and cross-node continuation evidence. These
are planned host extensions; current AQ messaging/notification/observation owners
are reused through explicit adapters, not inferred from synthetic pilots.

### Existing pilot acceptance

After AQ binds the existing owners and implements the remaining services, exercise the actual coding/integration, routing, findings-watcher, and supervisor-report pilots through the embedded runner. Include daemon restart between domain commit and event delivery, lost effect responses, revoked authority, changed candidate/base revisions, notification retry, late author completion, and protected database backup/restore with the correct keyring. Record the exact AQ/library/contract versions tested.

## Revision-pinned implementation evidence

The [bounded v1 reference inventory](../../aq-server/docs/research/2026-10-30-v1-implementation-reference-map.md) records implementation symbols, selected test obligations and per-file hashes at revision `9a9b8e8ef992d1ff699755a9383ed3c91e2844ea`. It distinguishes preserved behavior from remote-authority and retention adaptations. This evidence informs the owning v2 tasks; replacement coverage and product qualification remain pending.

## Reconciled AQ host catalog 1.0.0 (D4)

The authoritative [schema artifact](../contracts/aq-host/registry.json),
[manifest](../contracts/aq-host/manifest.json) and
[fixtures](../contracts/aq-host/fixtures.json) are published together. The CLI,
fake host and all four pilots load that artifact. Version `1.0.0` has fingerprint
`sha256:627125cc393d8274bfaa9ffc785c338e566cc69af362ac027a3daa355c3509e1`.
The fingerprint covers canonical JSON of `[version, registry]`; command/event
fingerprints cover their individual canonical definitions. See the
[vendor instructions](../contracts/aq-host/README.md). A consumer must pin both
the source revision and this manifest, and deliberately rebind changed contracts.
There are twelve commands and seven events within the daemon's 262,144-byte
registry bound. The five context views, nine work kinds and six work.change
variants are unchanged and listed in the manifest.

### Recovery and outcomes

Every mutation except publication uses an immutable operation identity and input
binding: repeating the identical identity/input/authority must return the retained
admission or terminal result, without another domain effect. A conflicting reuse
refuses. This is the host's keyed idempotency obligation; registration alone does
not establish that the production adapter meets it. Observations may be re-read
with a new invocation identity, retaining each observation. Publication must query
the integration owner's authoritative receipt before deciding on resubmission.
No command requires naturally idempotent or unsafe recovery in this version.

| Command | Recovery | Exact named outcomes |
| --- | --- | --- |
| `context.read` | Observation | observed, incomplete, expired, refused |
| `work.ensure` | KeyedIdempotent | accepted, ensured, completed, refused |
| `work.change` | KeyedIdempotent | changed, conflict, refused |
| `judgment.request` | KeyedIdempotent | assessed, refused |
| `decision.request` | KeyedIdempotent | accepted, approved, rejected, expired, cancelled, refused |
| `check.request` | KeyedIdempotent | passed, failed, infrastructure_failure, refused |
| `candidate.prepare` | KeyedIdempotent | prepared, conflict, refused |
| `candidate.publish` | Reconcilable | published, conflict, refused |
| `message.send` | KeyedIdempotent | queued, refused |
| `findings.reconcile` | KeyedIdempotent | reconciled, conflict, refused |
| `operation.get` | Observation | running, completed, failed, cancelled, indeterminate, refused |
| `operation.cancel` | KeyedIdempotent | accepted, refused, too_late, cancelled, indeterminate |

`accepted` on an asynchronous request carries only `{handle}` and promises
admission. `work.ensure` returns `ensured` for independent work admission and
`completed` for a retained owned-work result. The latter contains a separate
business `disposition` (accepted, changes_requested, failed). `message.send`'s
`queued` records an identified enqueue; delivery and human acknowledgment remain
separate host facts. `operation.cancel`'s `accepted` is cancellation admission,
`cancelled` confirms termination, and `too_late` reports an already terminal
operation. Transport rejection and uncertain transport responses remain
`DispatchResult::Rejected` and `DispatchResult::Unknown`, respectively.

`decision.request` is asynchronous. It binds a question, declared choices,
eligible responders and expiry to an exact subject revision, admits a durable
handle, then completes with approved/rejected/expired/cancelled/refused. The host
authenticates the human responder separately from the workflow's JSON and checks
eligibility, current revision and deadline. No configured approval on the request
can complete it. Pending `operation.get` returns `running`; terminal readback
returns `completed` with `{outcome, output}`, preserving the original work or
decision business result. This replaces the ambiguous `succeeded` readback.
Cancellation or indeterminate state can be observed separately. Reconciliation
of a lost dispatch response must retain the same identity and authority.

The 1.0.0 judgment adapter is synchronous (`assessed`), and 1.0.0 `check.request`
names a suite rather than a registered check. Both are frozen as published; the
asynchronous provider judgment and registered check execution that D6 needs are
host catalog 2.0.0 (below). This retains the routing pilot's 1.0.0 behavior and
does not imply execution of an unimplemented production model adapter.

### Event names, producers and schemas

All seven events have semantic version `1.0.0`, authenticated producer
`aq.host.bridge@1`, scope `project`, payload bound 65,536 bytes, and retention
86,400,000 ms. Each envelope requires the correlation map field `operation`, whose
value is the authoritative operation handle (for domain notifications without
an admitted async handle, the originating domain operation identity). The handle
is opaque and preserved byte-for-byte; it is not inferred from subject ID. The
producer must be authenticated by the host bridge; a payload field claiming the
producer confers no authority.

Let `Subject = {id: bounded string, revision: nonnegative integer}` and
`Result = {reference: bounded string, subject: Subject}`. Strings are at most
4,096 bytes unless the artifact specifies a smaller bound. Objects are closed:
extra keys refuse. The machine schemas in registry.json are authoritative.

| Event | Authoritative source behind bridge | Payload | Envelope subject/revision paths |
| --- | --- | --- | --- |
| `aq.work.changed` | Work/graph owner | Result plus outcome: ensured/changed/completed/cancelled | subject.id, subject.revision |
| `aq.artifact.recorded` | Evidence owner | Result plus outcome: recorded | subject.id, subject.revision |
| `aq.operation.updated` | Owned work result owner | `{outcome: completed/refused, output: Result plus disposition: accepted/changes_requested/failed}` | output.subject.id, output.subject.revision |
| `aq.check.completed` | Executed check owner | Result plus outcome: passed/failed/infrastructure_failure/refused | subject.id, subject.revision |
| `aq.decision.completed` | Human decision owner | `{outcome: approved/rejected/expired/cancelled/refused, output: Result}` | output.subject.id, output.subject.revision |
| `aq.integration.changed` | Integration/publication owner | Result plus outcome: prepared/published/conflict/refused | subject.id, subject.revision |
| `aq.signal.reported` | Authorized signal owner | Result plus outcome: reported | subject.id, subject.revision |

The bridge publishes stable producer/event identities and causation references,
with occurrence and observation times. Subject and revision in the envelope must
match the specified payload paths. Owned-work and decision completions carry
`correlation.operation = accepted.handle`. A duplicate event with the same identity
and content is a replay; conflicting content refuses. The engine recognises a
duplicate only while it still remembers the identity (spec 0003: the most recent
`replay_keep` keys within their window); a later re-send is refused as stale only
when it carries a request time older than the window, and is otherwise admitted
as new. Events do not grant the
workflow new authority. Domain state and the outgoing event must commit atomically
in the owning host service (D3 outbox), and delivery may retry until admitted.

The old synthetic names map as follows: work.changed → aq.work.changed,
artifact.recorded → aq.artifact.recorded, operation.changed → aq.operation.updated,
check.completed → aq.check.completed, decision.resolved → aq.decision.completed,
integration.changed → aq.integration.changed, signal.reported → aq.signal.reported.
Old producers and fingerprints are incompatible; do not alias them silently.
The daemon's old `aq.operation.updated` queued-message payload is also incompatible
with this work-completion schema despite sharing its name. Message enqueue is
represented by the `queued` command receipt; message delivery events need a new
versioned extension if required.

### Production adoption boundary

D5 must vendor this bundle by exact Git revision, relax the blanket
Reconcilable-only registry guard to accept the published per-command classes,
and replace independent command/event definitions with the exact artifact.
The existing daemon message/decision inputs, result fields, version strings,
producer mappings and operation lookup are also different; changing just the
outcome names cannot satisfy fingerprint equality. Production adapters must
translate to their owning services while enforcing this public schema and current
authority. D3's outbox and D8's triggers must use the event table above.
Production fixture/replay and assembled-pilot checks remain D5–D8/D11 work; D4's
synthetic results do not claim those adapters are already bound or qualified.

## AQ host catalog 2.0.0 (D6 judgment and check adapters)

The [2.0.0 bundle](../contracts/aq-host-2.0.0/README.md) is published beside
1.0.0 and does not replace it. Version `2.0.0` has fingerprint
`sha256:e2ce7dc8ff4b37538f8d27f175a4e365ff95cf14508ae7074beea04026b143db`
over `["2.0.0", registry]`. The manifest records the 1.0.0 version and
fingerprint it supersedes.

### Compatibility decision

- **1.0.0 is frozen.** Its bytes, fingerprint, fixtures, the CLI surface, the
  fake host and all four pilots are unchanged. Existing namespaces, grants and
  captured artifacts stay bound to 1.0.0 contract fingerprints. A 1.0.0 host
  refuses 2.0.0 contracts, and a 1.0.0 grant does not authorize them.
- **Nine commands and all seven 1.0.0 events are byte-identical in 2.0.0.**
  Their per-contract fingerprints are equal. `fixtures.json#compatibility` pins
  them, along with the 1.0.0 registry file SHA-256.
- **Three commands change.**
  - `judgment.request` and `check.request` move to command version 2.0.0, are
    asynchronous, and require the new capabilities `judgment.request.v2` and
    `check.request.v2`. An inherited 1.0.0 capability never authorizes provider
    spending or sandbox execution.
  - `operation.get` moves to 2.0.0 because its `completed` union also reconciles
    both new completions. Its capability is unchanged.
- **Two events are new, at event version 1.0.0:** `aq.judgment.completed` and
  `aq.check.receipted`. 1.0.0 `aq.check.completed` keeps its 1.0.0 meaning and is
  not the 2.0.0 completion event.
- **Consumers opt in with a new pin.** aq-server vendors the 2.0.0 directory as a
  new catalog profile and namespace generation, pinned to the exact aq-playbooks
  revision and this manifest fingerprint. It must not patch the vendored 1.0.0
  fingerprint. Rebinding 1.0.0 captured artifacts is a deliberate, separate act.

### judgment.request 2.0.0

This is a bounded provider request, not an agent session, and has no tool loop.

**Input** (closed object):
- the subject;
- a question;
- explicit context: inline text of at most 32,768 bytes plus up to 32 immutable
  context `ArtifactRef`s (`{id, version, digest}`);
- a declared output shape: up to 64 choice labels and a rationale byte bound;
- budgets: `max_input_tokens`, `max_output_tokens`, `max_cost_microunits` and
  `deadline_ms` (1,000–600,000).

There is no credential-profile or tool field. The host chooses a permitted
provider credential profile.

**Admission:** `accepted {handle}`, or a synchronous `refused`. The operation is
not cancellable.

**Completion** arrives on `aq.judgment.completed` and through `operation.get` as
`{outcome: assessed|failed|refused, output}`:
- `assessed` carries a declared `choice`, a bounded `rationale`, `provenance`,
  `reference` and `subject`.
- `provenance` holds provider, model, the credential profile reference (never the
  secret), prompt and context digests, an output digest, and usage (input and
  output tokens, cost in microunits).
- A refusal is typed: budget_exceeded, deadline_exceeded, invalid_output,
  invalid_request, no_permitted_profile or stale_subject.
- A failure is typed: provider_error, provider_unavailable or indeterminate.

A judgment never substitutes for a human decision or for executed check evidence.

### check.request 2.0.0

**Input** is `{subject, check: {id, validator: ArtifactRef}, source: {format: sha1|sha256, hex}}`:
- The check ID and validator must already be in the pinned outcome's
  `required_checks`, which holds at most 32.
- `source` is the retained immutable source object ID.
- There is no shell, URL, path or caller-asserted result field.

**Admission:** `accepted {handle}`, or a synchronous `refused` with reason
execution_limit, invalid_request, not_required, stale_subject or unavailable. The
operation is cancellable through 1.0.0 `operation.cancel`.

**Completion** arrives on `aq.check.receipted` and through `operation.get`. The
receipt binds a receipt digest, executor version, inputs digest, up to 32 result
artifacts, and stop/exit evidence. The schema enforces the D6 disposition mapping:

| Disposition | Catalog outcome | Receipt |
| --- | --- | --- |
| passed | passed | Required; stop `exited` with code 0 |
| failed | failed | Required; stop `exited` |
| cancelled, timed_out, unknown | infrastructure_failure | Nullable; any stop (cancelled/exited/lost/signalled/timed_out) |
| stale (or null at admission) | refused | None; typed reason |

A lost or uncertain stop therefore cannot validate as `passed`.

### Bounds left to the host

These are host semantics, not schema:
- the 64 KiB request bound (`max_payload_bytes`);
- digest and OID formats (lowercase hex, `sha256:` prefix);
- membership in `required_checks`;
- 8 executions per subject and check, 256 operations per lineage, and 10,000
  retained;
- that `choice` is one of the requested choices.

The fake host models 2.0.0 admission and replay only, returning a retained
handle. Completion is the production host's event.

## AQ host catalog 2.1.0 (prepared candidate source)

The [2.1.0 bundle](../contracts/aq-host-2.1.0/README.md) is published beside
1.0.0 and 2.0.0 and replaces neither. Version `2.1.0` has fingerprint
`sha256:24407a41481dba1bc9618e4c9239e0282083cca2ef57316a42d5f61df32d1716`
over `["2.1.0", registry]`. The manifest records the 2.0.0 version and
fingerprint it supersedes.

### Why

In 2.0.0, `check.request` takes `source`, the immutable object ID it executes,
but `candidate.prepare` does not return one. A policy can only bind `source`
to a caller input, which need not match the candidate the host prepared. 2.1.0
returns the source from the host that prepared it. A policy binds
`check.request.source` to the prepare step's `source` (for example,
`candidate.source`). A compile against 2.0.0 refuses that binding
(`expression_type`: unknown field source).

### Compatibility decision

- **1.0.0 and 2.0.0 are frozen.** Their bytes, fingerprints and fixtures are
  unchanged. The fake host still returns no source for an earlier
  `candidate.prepare` contract.
- **Eleven commands and all nine 2.0.0 events are byte-identical in 2.1.0.**
  `fixtures.json#compatibility` pins their fingerprints and the 2.0.0 registry
  file SHA-256.
- **`candidate.prepare` moves to command version 2.1.0.** Its `prepared` outcome
  requires `source: {format: sha1|sha256, hex}`, with the same schema as
  `check.request` 2.0.0 `input.source`. `source` is a public result field.
  `conflict` and `refused` are unchanged. The capability stays
  `candidate.prepare`: the effect and its authority are the same, and only the
  result gains a field.
- **Consumers opt in with a new pin.** aq-server vendors the directory as a new
  catalog profile (Host8), pinned to the exact aq-playbooks revision and this
  manifest fingerprint, beside the untouched 2.0.0 (Host7) pin. Its
  `candidate.prepare` adapter emits the prepared candidate OID as
  `output.source`. A host-side guard that substitutes or checks the prepared OID
  for a 2.0.0 `check.request` becomes either unnecessary for 2.1.0 policies or a
  consistency check (refuse when `input.source` differs from the prepared OID).

The host still owns the OID format bound: lowercase hex, 40 characters for
`sha1` and 64 for `sha256`.

## AQ host catalog 2.2.0 (routing: task facts, routes, structured judgments)

The [2.2.0 bundle](../contracts/aq-host-2.2.0/README.md) is published beside
1.0.0, 2.0.0 and 2.1.0 and replaces none of them. Version `2.2.0` has
fingerprint
`sha256:ca3c3d68959d760cc761d0c39b8db8740a27b3e414ae54e76d64df944f9b677a`
over `["2.2.0", registry]`. The manifest records the 2.1.0 version and
fingerprint it supersedes, and lists `commands_added` and `events_added`.

### Why

Before 2.2.0, a playbook could not route:
- reading a task returned no facts about it;
- no command set a route;
- no event started a run when a task needed one;
- a judgment returned a single label.

Spec 0013 §6 words the fix as changes to `context.read`, `work.change` and
`judgment.request`. Every Host8 coding artifact pins those three
fingerprints, so 2.2.0 adds new contracts instead.

### Compatibility decision

- **Additive only.** All twelve commands and nine events of 2.1.0 are
  byte-identical. `fixtures.json#compatibility` pins:
  - their fingerprints;
  - the 2.1.0 registry file SHA-256;
  - the fingerprints of the six additions.

  An artifact sealed on 2.1.0 validates against 2.2.0 unchanged.
- **New commands:** `work.read`, `work.route`, `work.unroutable` and
  `judgment.assess`.
  - Each requires the capability of the same name.
  - `judgment.assess` declares `operation.get` as its reconciliation command.
- **New events:** `aq.judgment.assessed`, the completion of
  `judgment.assess`, and `aq.work.route_needed`, whose payload is
  `{subject}` only.
- **Consumers opt in with a new pin:** aq-server's Host9 profile, beside the
  untouched Host8 (2.1.0) pin.

### Bounds left to the host

These are host semantics, not schema.

- **The request bound:** 64 KiB.
- **`judgment.assess` `assessed`:**
  - one label per requested field, in field order;
  - each label one of the field's declared choices;
  - flags only from the declared flags, each once.

  Otherwise the result is `failed` with `indeterminate`.
- **Exactly one completion:** one final `aq.judgment.assessed` for every
  accepted judgment, including at the host's own deadline.
- **`work.route`:**
  - It intersects `harnesses`, `providers` and `prefer` with what Settings
    offers at the level, drops duplicates and drops `prefer` entries that are
    not allowed. It reports what remained in `routed`.
  - It refuses `not_offered` only when the level has no profile, or when a
    non-empty list has nothing left. It then records the refused request in
    the route history.
  - It never replaces an owner route (`owner_route`).
  - It answers `stale` when the task's revision moved since
    `subject.revision`.
- **`hold`:** pins the task to its lists, for sentences that tell work to
  wait for an agent. A held task is routed: the unrouted alarm does not apply,
  and the task view shows it held, with its reason and how long it has
  waited. "Nothing available right now" is a wait for every task, never a
  failure; the "no candidate" failure means none in Settings.
- **One trigger per cause for "nothing could ever take it":**
  - Settings: `work.route` refused `not_offered`. The host raises the task
    itself, once. The playbook only ends.
  - The document: `work.unroutable {subject, reasons}`. The host clears a
    router route the task may have (never an owner route: `owner_route`),
    leaves the task unrouted and visible, shows the reasons, raises it, and
    does not re-emit `aq.work.route_needed` until the task or the active
    playbook changes.

  A playbook's entry outcome is never read as this signal.
- **`work.read` facts:**
  - They never include priority or provider load.
  - `level_hint` is always present (`""` for none).
  - `charged_failures` counts only failures charged to the task.
- **`aq.work.route_needed`:**
  - It is not emitted again for a task version while a run started for that
    version is unfinished.
  - Once a task is raised as unroutable it is not emitted again until the
    task or the active playbook changes, or, after `not_offered`, Settings
    change.

`operation.get` 2.0.0 is unchanged, so its `completed` union does not include
the `judgment.assess` outputs. A host reconciles a `judgment.assess` intent
from its own record of the operation. A playbook reads the answer through the
join, never through `operation.get`.

## AQ host catalog 2.3.0 (delivery by playbook, Epic 0)

The [2.3.0 bundle](../contracts/aq-host-2.3.0/README.md) is published beside
the earlier ones and replaces none of them. It is compiled in only with the
cargo feature `delivery_playbook` (`catalog_v2_3`). It adds the event
`aq.work.result_ready`, the delivery commands (`work.complete`, `work.return`,
`work.hold`, `supervisor.notify`, `main.sync`, `branch.publish`,
`pull_request.open`, `pull_request.await_checks`, `pull_request.merge`,
`pull_request.close`) and their completion event `aq.delivery.completed`, and
extends `work.read` (now 2.3.0) with the facts of the task's latest result.
Every other 2.2.0 contract is byte-identical. The host adapter for these
contracts is not part of this change.
