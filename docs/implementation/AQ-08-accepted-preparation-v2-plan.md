# AQ-08 accepted preparation v2: minimal cross-component plan

**Status: design plus pure-contract implementation lane; no owner or runtime
activation.** Preserve admission 0.1.7/0.1.8 archives and every old clock/fixture.
The worker [phase proposal](../../../aq-worker/docs/implementation/WRK-08-preparation-phase-separation.md)
records the authority distinction. Worker smoke source `532ec8c` with optimized
binary `b5ae2e3e8b7d2ca8166a30df75e88f40d7999733a96f3040f3e81a707c1f88ee`
reached actual initial Grant in one tiny-fixture run, but the same binary earlier
received server Prepare 200 and rejected usable delivery. Other frozen binaries
expired earlier. This is load-sensitive evidence, not cold-preparation closure.

## 1. Additive public contract, proposed publication 0.1.9

Append `aq_admission::preparation_v2` with its own exact contract artifact and
phase feature enumeration; do not alter frozen session Capability or legacy
transport Feature/DTO definitions. A bounded negotiation selects that exact
artifact and all mandatory features or refuses. No automatic old-profile fallback
is permitted after any v2 admission. Publish only after the independently owned
scoped-presence module is frozen into the joint 0.1.9 package; retain old archives.

The pure DTOs cover the original NativeOffer/spec/choice pins, timely Accept and
exact resource hold, immutable accepted no-launch preparation receipt and sampled
delivery, exact Prepare input claims, mandatory fresh owner observation set,
immutable execution receipt and independent delivery clock. Validators check
shape, unique mandatory categories, owner/binding/choice/source associations,
canonical request/receipt hashes, bounded vectors/bytes and deadline order.
They cannot certify authentication, owner truth, actual resources or private
files. No boolean or DTO presence is public live authorization. Owner adapters
must resolve the actual retained records before constructing live local permits.

Execution issuance must occur while the exact accepted lease is open, with
expected revisions and the original task/session/attempt/node/boot/generation.
Its new execution lease and hard deadline are independently bounded; local
sampling charges the complete execution response interval rather than inheriting
an expired proposal/preparation cap. Exact replay cannot change grant identity,
hard expiry or revive an already expired local execution sample. Historical
receipt inspection remains distinct from a usable delivery.

Qualify generated schemas/fixtures/drift checks, independent canonical vectors,
fake-peer association refusals and extracted-only exact dependency archives.
Compare all old module/artifact bytes against 0.1.8, allowing only package metadata
and an appended module export. No registry/network publication is authorized.

## 2. Central accepted-preparation owner, separate later slice

Coordinate the next migration number; this plan reserves none. Add an immutable
accepted-preparation record keyed by attempt and admission operation with exact
request digest, accepted receipt/digest, original proposal/source clocks, resource
hold and policy references, owner revisions, bounded deadline and terminal state.
Use the existing shared native quota lock and actual JSONB byte/count budgets;
no pruning of replayable unknown operations. New owner transactions use the
existing node → attempt/hot → project/catalog → registry/principal lock order.

New explicit v2 routes/profile dispatch are separate from legacy admission
handlers. Timely Accept verifies current proposal, principal, boot/runtime,
project/task CAS, exact sealed choice and qualified resource policy, then records
accepted permission for original bounded input/material/source operations only.
Deadline expiry stops new work but never frees holds. Exact receipt replay keeps
its original expiry; a fresh delivery stamp only subtracts elapsed time.

Inventory every shared writer before mounting v2. New profile/runtime generations
must fail closed through old routes. Qualify a DB-enforced writer-version fence
for v2-owned state, including already-open old connections, or explicitly exclude
mixed writers with a proven deployment fence. A new sidecar column alone is not
an old-writer fence. Control cancellation/revocation remains bounded/responsive;
no general mutation bypass or expensive external I/O under authority locks.

## 3. Strict fresh initial execution admission

Reuse renewal's pinned/current/external-observations split, not its permissive
Unknown continuation rule. Retain original sealed spec/context/choice/source and
actual installation receipts. Collect genuinely new bounded Graph task/context,
project/catalog, principal, inventory, provider availability, budget, Git retention
and central private-file observations outside authority locks. Recheck exact pins
and fresh times after lock/quota waits before atomically recording confirmation,
execution grant, material receipt and quota charge.

Initial admission requires fresh known budget/provider eligibility. Newer missing
inventory, cooldown, registry/principal/config revision change, different context
or absent retention refuses. Current source confirmation must name the same
original retained object/base and policy; no new ref resolution or old observation
retiming. The material claim remains bound to original private installation and
current backing fingerprint. Scoped same-grant presence is not global new-placement
availability and cannot substitute for these facts.

Audit original artifact/context/source/material expiry policies. Choose a bounded
qualified cold window (proposed initial maximum 30 seconds, shorter when retained
owner policy requires); larger workloads require prewarming or a separate bounded
continuation design. Do not relabel the old 2-second decision as that authority.
The public old plan caps and all old journal deadlines stay intact.

## 4. Worker owner and actual assembly, separate later slice

Vendor the exact joint 0.1.9 archive without sibling checkout dependencies. Add an
explicit v2 profile/client and new journal format; refuse unsupported existing
stores unchanged. Preserve old proposal/acceptance clocks and store the separately
admitted lease, exact hold and original receipt. Compute its conservative local
CLOCK_BOOTTIME deadline from actual authenticated delivery. No receipt or reopened
JSON manufactures input bytes, source proof or a live permit.

Reuse the bounded two-transaction inline batch, actual private installation pins,
streamed whole-file verification and finite Git import/create intents. Recheck
actual source/materials and full context before sending the exact durable Prepare.
Persist the actual execution receipt/delivery before any physical host hook;
launch intent must precede every adapter/host preparation callback. No host or
provider activation belongs to the current pure-contract slice.

Keep current grant-unknown recovery explicit: server commit/HTTP 200 plus rejected
or lost delivery is ever-issued authority with held task/source resources, not
NeverGranted. Reconcile exact historical operations without refreshing permission.
NeverGranted closure must serialize against issuance; granted unknown needs its
own qualified closure/physical suppression facts. Unknown import/create writers
block cleanup until actual Git owner recovery/retire/release proof. No automatic
recursive deletion, replacement IDs, account rotation or task completion follows
preparation expiry.

## 5. Completion evidence

Test proposal expiry versus timely acceptance; cold work beyond proposal but
inside accepted lease; immutable replay/lost response; eight relevant phase/fsync
cuts; old format/feature refusal; strict stale/Unknown/current-revision refusals;
actual control under held transfer/storage; Grant/closure races; and committed
Grant with unusable delivery. Qualify actual daemon + independent worker process
+ actual Git, with physical bounds and cold/warm timing reported separately.
A tiny optimized success does not close arbitrary cold workloads or PB04 latency.

## Explicit historical owner boundary before activation

The current OriginalGrant, renewal and closure loaders consume the legacy stored
`prepare` receipt. The new owner path must retain phase-aware provenance: either
an explicit typed historical variant or a trusted normalized internal view that
keeps the original receipt kind and canonical digest. It must never manufacture a
legacy InitialGrantDelivery or retime an old preparation plan to reuse validators.
Same-grant renewal and terminal closure must consume and test that explicit
boundary before any phase-v2 activation. This is a later owner integration gate,
not authority provided by the pure package.
