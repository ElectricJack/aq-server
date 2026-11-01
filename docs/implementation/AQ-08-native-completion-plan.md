# AQ-08 native producer completion plan

Current delivery baseline: `1b7e4f5`, state format 33. The ordinary daemon now
owns phase offer/preparation/Grant and qualified Started/Stopped/closure reports.
Actual separate-worker/Git/tmux tests cover those owners; this does not qualify a
real model or make the production worker command use the native path. The worker
CLI still exposes foundation `Run` and protocol 2 synthetic `RunExecution`; its
native physical adapter remains test-support-only. See the
[native runner delivery delta](AQ-08-native-runner-delivery.md) for the next bounded
increment. The older checkpoint narratives below remain historical, not a claim
that current handlers are absent.

This task-level implementation plan is subordinate to architecture 0003/0004 and
the [three-area completion scope](../plans/0002-execution-sessions-policy-completion.md).

## Historical producer increments

The qualified context projection/counting slice is5510d3d; byte-verifiable native
startup delivery is c3f49fd with single-descriptor correction70e98fe. Their facts
and contracts are prerequisites, not an installed native producer or real model
qualification. The source I/O owner9fa3428/bad2cb9 verifies actual retained Git
exports/chunks before any attempt; source owner022 adds durable quota and exact
reconciliation/completion/linkage. See [the022 storage review](AQ-08-native-storage-022.md).
Migration022 does not add poll/accept/prepare/grant/renew handlers. The ordinary
first native worker consumer and final startup-byte assembly/counting remain
next required increments; missing actual CLI/model facts continue to refuse.

## Ownership and immutable contracts

Reuse the frozen transport/execution-spec/Started DTOs through current
`aq-admission`0.1.6 and pinned session/lifecycle identities. Additive stopped
and outcome owners are installed through migration021; the0.1.6 startup profile
and report-recording acknowledgment have exact qualified archives. The native
producer's022 candidate is functional pre-offer source ownership only; later
phase storage needs a separately coordinated migration.
No existing migration or publication is edited in place. Any missing typed
reconcile reply or genuinely incompatible envelope is coordinated/versioned
before use; generic undocumented JSON is not a substitute.

The server owns task/source observation, project-policy scheduling principal,
selection, retained inline artifacts, actual source-owner mapping, private
material leases, central authority and operation receipts. The assigned qualified
worker owns its durable no-launch reservation, artifact/material installation,
physical host journal and process. Root's stop/result owner verifies delegated
host evidence; source/effect owners independently verify their receipts. Session
and supervisor/playbook lanes consume authority instead of creating parallel
grants or fictitious worker identities.

## Required context/model facts and acyclic sealing

1. Fetch an actual ready task with exact graph CAS and content revision. Fetch
   bounded context through the configured public graph reader and preserve raw
   read provenance centrally. Required truncated/unknown context refuses.
2. Project only authorized API01 `ContextSnapshot` fields/records. Task records
   require canonical `task.read` for this project; knowledge records require
   `knowledge.read`; repository writes name the actual repository. Neither raw
   daemon graph responses nor unapproved metadata enter model startup context.
3. Compile the instruction/permission draft from the installed immutable catalog,
   real project policy and exact projected assignment. Derive ordered-instruction
   and tool-policy digests, seal the typed preselection graph policy against the
   actual complete context, then freeze schema2 composition. No model/profile
   digest is required to construct these instruction bytes.
4. Resolve intelligence through `resolve_for_project_policy`, never a borrowed
   human actor. Actual installed harness/credential/availability facts, lineage
   and budgets retain exact provenance. Native selection refuses schema1
   compositions; legacy inspection/replay remains explicit and separate.
5. Require an operator-owned, versioned qualification entry for the exact
   provider/model/native reasoning/configuration, harness adapter/version,
   environment and qualification. It must name the actual model capacity or an
   explicitly qualified conservative planning ceiling and a qualified counting
   algorithm/bound, its overhead allowance and evidence provenance/validity.
   Missing, expired, mismatched or unqualified facts refuse. Unknown token/window
   values are never represented as0. A counting bound is not an exact tokenizer
   claim. Worker advertisements cannot create this evidence. Actual production
   combinations remain closed until their real configuration/adapter qualifies.
6. Count the actual ordered startup bytes plus qualified harness overhead before
   producing a numeric ContextPlan. Require input upper bound plus selected
   reserved output to fit the qualified ceiling and retain the counting evidence.
   Synthetic test facts identify a synthetic harness/model explicitly.
7. Derive the actual API01 resolved profile from that policy and the genuine
   selected model/configuration. Bind its actual digest in GraphBinding and the
   manifest/spec; bind original policy digest in schema2 composition. This order
   avoids both model/profile and composition/instruction digest cycles.

## Genuine source and retained artifacts

`source::observe` already performs finite public Git reference/commit/retention
reads for an exact configured project source policy. It preserves original age
and initially supports only exact retained SHA1 commits. Before an offer becomes
visible, an actual source owner must export/verify the immutable source bundle
and retain its real manifest/bytes/receipt. Existing post-attempt source-base
provisioning must gain a narrow trusted pre-offer preparation seam; the central
producer cannot invent a source descriptor or retention receipt to break that
ordering dependency. Stable source operation IDs and actual public operation
reconciliation handle lost replies. Bounded source quota reservation precedes
I/O; no public source-owner call runs while attempt/project admission locks are
held. Attach the actual prepared source mapping atomically with the offer.

Use the existing inline owner for actual nonsecret catalog/definition, graph
policy/profile, composition, selection, authorized context and typed-spec bytes.
Retain in standalone bounded owner transactions before final admission, then link
only verified same-project records under the offer transaction. Source bundles
and provider secrets never use this inline owner. The outcome artifact is a real
owner-produced expected-outcome definition bound to the graph acceptance
criteria/version and reporting policy, not an invented completed-outcome receipt.

## Complete native state/transport flow

| Step | Owner action and durable result |
| --- | --- |
| Register/inventory | Existing current-credential/config/runtime admission; exact boot and immutable replay. Inventory records installed support or daemon-derived preparation readiness, not runnable authority. |
| Observe/place | Bounded project round-robin/priority and node resource-fit proposals. Cache/warmth hints do not create eligibility or account identity. Existing held reservations count conservatively against initial finite capacity. |
| Seal offer | Fresh genuine graph/source observations and retained artifacts precede a SERIALIZABLE transaction. Recheck exact current project/catalog/node/selection/policy/resource/budget/history, claim the common task index, insert profiled immutable launch/selection/source/artifact links and Offered hot state together. |
| Poll | Authenticate/recheck the exact current native boot; return bounded actual assigned offers/cancellations and retimed remaining TTL, with no grant implied. Correlation IDs do not grant capacity. A side-effect-free empty poll must not exhaust an unbounded durable receipt log. Any mutating poll/admission operation needs explicit bounded durable idempotency. |
| Accept | Validate exact offer/binding, qualified runtime, acceptance digests and worker's durable no-launch reservation/logical guardian/launch journal. Read actual material bytes/fingerprint outside SQL locks, then recheck owner facts. Atomically persist the original accepted PreparationPlan, one exact admitted model-authentication lease initially, material mapping, Preparing owner and `record_reserved`. |
| Prepare | Worker fetches actual linked artifacts and exact private material, verifies bytes/digests, installs an owner-only attempt binding and records the prepared journal before submitting exact artifact/material receipts. Delivery HTTP acknowledgment alone is not installation evidence. |
| Grant | Reobserve graph readiness/CAS/content/context and exact source; revalidate current same concrete choice, model counting/capacity, registry/availability/project/catalog and remaining lineage budgets. Verify every required retained descriptor and original material installation receipt. Persist real grant/hard deadline, original prepare receipt and `record_prepared` atomically. A changed candidate needs a new offer, not mutation of this manifest. |
| Launch/Started | Worker accepts grant under independent monotonic host expiry, launches through its qualified guarded host, then reports the actual durable Running observation. Existing Started owner atomically records original receipt, advances Prepared→Started and sets Running; grant/renew never infer it. |
| Renew | Exact owner/grant, monotonic journal/operation identity, current auth/config/boot/policy/credential/material facts and fresh task/source/eligibility gates. Extend only within the original immutable execution hard deadline and supported material lifetime; never silently rewrite leases or install receipts. Replays shrink remaining TTL and do not extend authority. |
| Recover/cancel | Reconcile exact retained owner/journal/assignment records; missing or contradictory state is Unknown and launch/control remains fenced. Cancellation becomes durable before delivery; guardian stop/recovery never restarts an uncertain writer. A new incarnation cannot replace one with held claims. |
| Result/stop/settle | Report process outcome separately from graph task disposition. Root's qualified stop owner, actual source retention and effect reconciliation must establish their own receipts before claim release/replacement. NeverGranted uses the existing separate no-grant proof. Process exit, observations, notifications and terminal actions do not complete tasks or renew execution. |

Preparation deadline is the original accepted plan's absolute clock, independent
of old material history and the original offer expiry after acceptance. The
initial single-lease profile uses exactly equal plan/material expiry. Clamp the
plan to the worker's remaining durable reservation lifetime; a response/retry
never creates a fresh reservation. Grant TTL is separately bounded by current
lease support and original execution deadline. Workers subtract the entire
request interval from server-provided remaining TTL and keep local monotonic
hard ceilings. Clock rollback or unknowable freshness refuses extension.

## Final fences, replay and grant-time selection refresh

No external graph/Git request or private-file read occurs under final SQL locks.
Retain actual observation times and expected revisions, then refresh time after
all waits and immediately before commit/response. Canonical lock order is current
node credential → global configuration owner → exact attempt/hot when present →
project → catalog activation → intelligence registry/snapshots/availability →
installed policy principal → deterministic lease/artifact/journal/receipt child
rows. Match existing owner lock order; standalone policy installation uses
configuration→project→policy and is called after binding commits, not under a
held project write lock. Inline/source quota reservation is outside this chain.

Mutations retain original operation/body digest and semantic result under their
exact owner namespace. Changed content under one operation conflicts. Replay
rechecks current machine authorization and never reinstalls an old boot, changes
original grant/manifest/stop identity or resurrects released authority. Delivery
clocks are recomputed from the same absolute expiry; retained historical receipt
readback is distinct from permission to act.

The original AQ20 decision expires quickly and its task budget snapshot precedes
the new offer reservation. Grant must not call the new-attempt resolver with a
caller-chosen discount or blindly replay that expired decision. Add a trusted
reserved-attempt refresh: only the exact still-held never-granted reservation can
be discounted from started-attempt charging, under its node/attempt locks, while
all history/generation and NeverGranted retry bounds remain charged. Recompute
current eligibility for the exact immutable concrete choice; any fallback/change
refuses this grant and requires terminal NeverGranted closure/new admission.
Retain the fresh validation provenance separately from the original immutable
selection/manifest. Graph context comparison ignores only its observation-time
wrapper, not changed records/edges/CAS/content; source must still resolve to the
same sealed object and active retention. Freshness is not distributed locking:
unsupported dependency-sensitive automatic graph settlement refuses.

## Storage and bounded recovery additions

Review exact migration022 before registration. Expected additions are bounded
native admission/prepare/grant revalidation operation receipts, stable source
preparation/quota/operation ownership before an attempt exists, and immutable
fresh grant-validation provenance. Existing common attempt kind/identity/hot
rows remain sole authority. Do not add a shadow task lifecycle or parallel grant.
Review every legacy writer and common authority consumer affected by new fields;
keep old serialized fixture requests/digests unchanged. Receipt/artifact retention
must have explicit finite count/byte budgets, exact replay at capacity, and safe
cleanup only after all referenced ownership obligations are satisfied. Do not
make an empty polling loop permanently consume one retained mutation row per tick.

## Delivery increments and qualification gates

1. Add the missing qualified context/counting facts and authorized projected
   context/policy/manifest assembly. Test missing/expired/wrong-model facts,
   Unicode/overflow/overhead, mixed scopes, two-model same-harness behavior and
   real-byte descriptor derivation. No runnable placeholder manifest is emitted.
2. Implement actual pre-offer source preparation through the existing Git owner,
   retained artifact assembly, SERIALIZABLE offer publication and native poll.
   Test actual pinned public Git/Graph plus deterministic synthetic qualified
   harness data, mixed-kind claim races, lock-wait expiry, quota/replay and bounded
   fairness/resource fit. Real source/material/retention receipts are mandatory.
3. Implement accept→original plan/admitted lease→verified prepare→separate grant
   with reserved-choice refresh. Test lost replies, conflicting replays, changed
   task/context/source/model/credentials, insufficient counting limits, lease
   rotation/expiry and races with NeverGranted closure. Use existing material and
   artifact endpoints; do not duplicate owners.
4. Wire renew/reconcile/cancel/result/stop ownership to actual worker driver
   records and the existing Started owner. Qualify real no-launch/prepare/launch
   journals, fresh boot recovery, offline expiry and unknown-start behavior. The
   worker consumer/material installation/host adapter is a required WRK08 owner
   interface; server-only synthetic HTTP tests cannot close this gate.
5. Run a meaningful selected native CLI task through the ordinary daemon/worker,
   capture genuine transcript/structured failure/usage provenance and retained
   source/effect disposition. Actual CLI qualification uses a selected configured test target and a declared
   bounded budget. Missing concrete facts, credentials or a target are reported
   as those blockers; they do not block ordinary component implementation. This
   initial component stage makes no paid provider calls.
   Root owns combined final workspace/component acceptance and canonical status.

Each increment separates code from frozen source/log/archive evidence. Focused
checks precede the combined root gate. Session/control, supervisor/playbook and
central-authority changes remain in their assigned lanes.

## Qualified context input increment

`context_projection` constructs an API01-only projection from the actual bounded
ready/context reader result. It verifies original response bytes/digest, root CAS
and content revision, workspace, freshness, typed permission coverage, exact edge
endpoints/pins and complete required context. It omits raw graph metadata,
provenance and actor fields. Original observation time and source digest remain
available centrally; canonical projected bytes are the model input artifact.

`context_budget` accepts only explicit daemon-owned
`ProfiledConfig.context_qualifications`. The optional empty vector is omitted
from serialization, preserving old configuration digests. Each bounded fact
names exact runtime/qualification, harness CLI/adapter/environment/qualification,
provider/model/reasoning/configuration, credential profile/revision/account and
endpoint. Facts name counting and capacity evidence, validity and a qualified
UTF8-bound/rendering profile. The current renderer joins startup instructions
with two literal newlines; qualified fixed overhead is charged explicitly. It is
not a universal tokenizer claim. Conservative planning capacity is labeled as
such instead of being advertised as an exact native window.

The result binds actual rendered bytes, selection/composition and evidence and
includes per-startup-instruction upper bounds plus selected reserved output.
Deferred context has no invented zero-token entry; any later manifest/consumer
must obtain its own supported bound or refuse that mode. The helper never grants
execution or replaces final configuration/registry/project/authority fences.
Changed facts alter the content-addressed definition and installed global
configuration digest; a stale daemon cannot keep using an old capacity policy.
Synthetic tests do not install any real provider/model qualification.

## Delivery lease versus installed execution binding

The source022 owner is separate from execution authority. Delivery lease expiry
ends retrieval permission for private material; it does not itself erase an
already verified admitted installation or renew execution. Initial Grant0.1.6
keeps its lease-covered expiry. Native control0.1.7 adds explicit renewed
authority for the same binding/grant/logical owner and immutable hard deadline,
with current machine/agent/registry/profile/fingerprint fences. No changed secret
or fallback is silently adopted. The worker stops authorized writers before
private binding deletion on stop/revocation/hard deadline. A historical delivery
receipt never supplies fresh execution permission. See the [native control reply
specification](AQ-08-native-control-017.md) for fresh per-transmission delivery,
replay/current-state separation and finite page bounds.

Migration025 adds the typed inline Source-role observation owner required by
frozen NativeOffer validation. It retains the actual canonical public Git
observation under `aq.source-admission-observation/{digest}@1`, validates exact
project/repository/generation/base and original retention evidence, and checks
freshness after owner locks before a new receipt. Exact historical replay keeps
the original receipt. Existing Context proposal receipts remain Context;020 and
published contracts stay unchanged. The large retained Git bundle stays separate.
No placeholder converts missing owners or unknown token facts into a manifest.

## Native source retrieval checkpoint (025)

Current-credential worker reads use `aq-source-api-version: 1`:

- `GET /v1/worker/profiled/source/{attempt}` returns the exact immutable
  `TransferManifest`; its transfer/artifact ID equals the original export
  operation ID.
- `GET /v1/worker/profiled/source/{attempt}/{manifest|bundle}?offset=N&limit=L`
  returns at most 262144 bytes, `application/octet-stream`, `no-store`, an exact
  `aq-source-offset`, and base64url-unpadded `aq-source-transfer` JSON (<=4096
  encoded bytes). Unknown fields/member names and invalid ranges refuse.
- Failures use bounded 0.1.7 `ControlReply::Failure`; version1 headers identify
  this source API independently of the frozen admission transport.

The first status result must be journaled by the consumer. Later status/chunk
metadata must compare equal; the consumer verifies the full pinned manifest and
bundle digests before hydration. Final source reads recheck current machine
credentials, registered runtime/global configuration, held exact native
assignment, project revision/repository generation/node eligibility, cancellation
and phase expiry after external Git/file I/O. Offered uses the original offer
expiry; Preparing requires its actual original accepted PreparationPlan and
owner; Granted uses the current grant and immutable hard deadline. No read grants
execution, renews retention, creates a journal or supplies an installation receipt.
Files are opened through the bounded nofollow source owner. Git retention checks
and chunk reads occur outside SQL locks; fresh final authority checks prevent an
in-flight credential rotation from authorizing the response.

The first producer mode reserves two of Prepare's 128 receipt slots: at most 126
inline artifacts plus `aq.source-manifest/{export_operation_id}@1` and
`aq.source-bundle/{export_operation_id}@1`. Each receipt binds exact verified
bytes/digest; initial mode requires a positive retained bundle. Generic zero-byte
bundles have no bundle digest and cannot be represented as a positive-byte
ArtifactReceipt. No source receipt authorizes launch by itself.

A framework restart creates a new logical boot. Initial recovery never accepts,
prepares or renews an old boot's live authority. It preserves original owner
journals and may report historical Started/Stopped/results under current machine
credentials, then waits for genuine settlement/release before new-boot
registration. Ever-granted/noStarted remains held until a separately qualified
no-start recovery owner exists.

This checkpoint does not provide the scheduler/offer/accept/prepare/grant/renew
producer. Those handlers must compose these real owners, count every final startup
byte using qualified facts, and preserve the acyclic schema2 policy/selection
order. Production harness eligibility remains closed without real definition,
model-capacity/counting and adapter qualification bodies.

## Public definition bodies for producer assembly

An ArtifactRef alone cannot supply the definition/evidence bytes required by a
manifest. Optional `ProfiledConfig.definition_bodies` supplies daemon-owned exact
public UTF8 bodies with ArtifactKind Definition and a matching content digest.
It defaults to empty and changes no old startup behavior or serialized defaults.
Missing required bodies refuse construction; retaining arbitrary text does not
qualify a model, credential, adapter, tokenizer or environment. The configured
qualification owner remains responsible for truthful public evidence and must
exclude credentials and private material from these public bodies. They are
never interpreted as executable hooks, paths or commands.

The catalog is bounded to 64 definitions, 2MiB per body and 8MiB total. Definition
fields are immutable after checked construction/deserialization. Configuration
serde retains exact bodies for roundtrip, while `configuration_digest()` uses an
explicit compact projection with exact references/kinds/byte counts. Each
reference digest already binds its constructor-verified bytes. Empty catalogs
preserve the prior configuration digest. Installer, policy-record and current
configuration fences all use this same projection; hot authority requests do not
rehash multi-MiB definition bodies.

The actual inline artifact owner supplies receipts for the required definitions.
All required references are checked before retaining any; individual receipts
are durable per-artifact operations, so a later storage failure can leave charged
retained artifacts but grants no execution. Migration027 makes definition
identity/version content immutable across projects and configuration revisions,
serialized under the existing inline-artifact quota lock. The schema owner
registered026→027 separately. This lane’s original explicit027 component tests
are not ordered migration qualification. Final producer offer/grant transactions must fence the same
current configuration after external work and waits.

## Actual acceptance/preparation owner

Frozen checkpoint `57584b8` mounts raw `transport::Accept` at
`POST /v1/worker/profiled/accept` on the machine listener with a 64KiB body ceiling
and one nonnil `aq-delivery-nonce` per physical send. It returns the frozen 0.1.7
`ControlReply::Preparation` envelope. This is preparation permission only, never
an execution grant, GraphActive credential or Running observation.

Acceptance consumes an existing exact sealed native offer and an actual retained
selector Decision. The service requires operator-qualified and registered
artifact/material/durable-journal features and qualified capabilities covering the
manifest/harness requirements. Extra known worker capability claims do not
manufacture qualification. A bounded private-file read and fresh retained-export
inspection run outside SQL locks. Current machine credentials, native boot/runtime
and installed global configuration, exact held attempt/hot identity, current
project/catalog/registry/node/availability/budget facts and project-policy
principal are fenced again before commit.

Final transactions are SERIALIZABLE. Lock order is current node → installed
configuration → exact attempt/hot/sealed records → project → catalog → registry
and inventory/availability/policy → preparation quota → lease/mapping → Reserved
journal → original receipt. Existing acquisition routines may recheck already-held
locks; no external I/O runs inside the transaction. The original decision's
pre-offer counts discount exactly one DB-verified current ungranted Offered or
Preparing reservation. Actual durable history/generation is unchanged. Prior
Started/Granted/legacy/closed history and finite NeverGranted retries remain
charged; issued grants cannot use this preparation fence.

Preparing, the single admitted lease/mapping, exact original PreparationPlan and
Reserved launch-owner journal commit atomically. A failed journal/storage write
rolls everything back. Initial scope is server-material preparation only, with
lease expiry equal to the plan deadline and capped by the original offer,
selection and reported remaining reservation interval. Existing delivery never
extends that deadline. Exact original-ID replay returns the immutable plan with a
fresh nonce-bound shrinking clock and current SQL-derived revisions; expired or
changed authority refuses. Concurrent serialization/capacity failures require
explicit original-ID reconciliation, with no hidden HTTP retry or new operation.

New preparations are bounded to 4096 retained leases globally / 256 per node,
64MiB globally / 4MiB per node of conservative uncompressed lease/accept-response
charges, and 8KiB lease / 16KiB response frames. These are logical storage limits,
not measured PostgreSQL or RSS ceilings. Quota checks include incoming bytes.
Missing definition027 protection keeps the separate definition retention helper
closed until that schema is installed.

Component tests use an actual resolver/policy/registry/file owner and exact scoped
public Git/Graph peers, with explicitly synthetic sealed offers and qualification
facts. The test-only fixture cannot substitute for the missing real offer
assembler or a real model/harness/counting qualification. No process is started,
provider called, source/effect receipt invented, grant issued or task completed.
Next required code remains actual ready-task → authorized projection/composition
→ selection → retained artifacts/source → atomic offer publication, then
Prepare→Grant, Renew and Reconcile handlers plus the worker/host vertical gate.


## Bounded first native offer publication

The initial producer reads actual public ready/context owners, checks exact task
CAS/content revisions and decoded record equality, compiles the authorized
ContextSnapshot into startup before selection, and uses current service-policy
selection plus explicitly qualified model capacity/counting. Missing facts refuse;
unknown token counts are never encoded as zero. Actual source export intent is
reserved before Git I/O and reconciled by its original operation identity. After
export, ready/context/eligibility/selection and source retention are observed again.
All Git/Graph/file I/O precedes the final SQL transaction.

The final SERIALIZABLE transaction locks/rechecks the authenticated current node,
registered runtime/configuration and actual project/catalog/registry/policy facts,
then publishes the shared unsettled-task claim, immutable launch/spec/selection,
Offered hot authority, owner-linked retained artifacts/source and an internal
protected API01 launch binding. No credential, material lease, grant or Running
observation is minted by Poll. Independently retained artifacts/export intents may
remain charged after a later failure, and are not silently pruned.

The immutable launch row stores a versioned complete original NativeOffer and
separate central read provenance. Historical loading verifies exact binding,
manifest/spec digests, readiness/context expiry ceilings and linked owner bodies;
missing older wrappers refuse. The final renderer payload is verified against
its descriptor and exact constituent spans. Fixed project-policy labeling is part
of the counted compiler input, using a derived content-addressed context source;
the original policy Definition remains separately pinned. This avoids ambiguous
same-byte Definition/Context receipts under the frozen Prepare digest-uniqueness
rule. The worker receives one rendered payload and adds no framing.

This first profile selects at most one offer, one worker slot, the highest-priority
enabled project and its first ready task. Empty/ineligible first projects and
already-held first tasks can starve later work. It requires one allowed agent
recipe, immutable SHA1 source, positive complete bundle and startup-only delivery.
It refuses inline descriptor counts above126, logical inline bodies above16MiB,
duplicate receipt digests (including Git members), and original wrappers above
500000 compact bytes. These are conservative supported-profile limits, not claims
of full public-envelope or fleet capacity support.

Original offer expiry is at most30s and no later than all exact ready/context,
source, selection and counting facts. Original preparation/material expiry cannot
exceed it. Cold hydration/install/Prepare may consequently time out for slow or
large workspaces. Delivery retries shorten remaining TTL; they never rewrite the
original clock. Later prewarming or exact-input freshness revalidation must have
its own owner semantics rather than extending stale originals.

The initial integration test uses synthetic Graph/Git/model/runtime qualification
bodies and real daemon HTTP, retained owners, compiler/selector and PostgreSQL.
It exercises Poll→Offered→redelivery→Accept→Preparing with no grant. Real CLI/model
qualification, worker execution, Prepare→Grant/Renew/Reconcile, cancellation and
bounded expiry dispatch remain distinct gates. A previously started attempt is
never discounted by reserved-choice refresh; verified NeverGranted history still
advances generations and consumes the separate finite retry budget.
