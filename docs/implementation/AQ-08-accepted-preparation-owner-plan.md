# Accepted preparation v2: first server owner slice

Status: proposed implementation scope; default inactive. This follows the pure
[phase design](AQ-08-accepted-preparation-v2-plan.md). It does not activate execution,
renewal, closure or a worker driver. Migration 030 is reserved; 029 belongs to the
scoped-presence owner and ordered registration is coordinated separately.

## Owned boundary

Add a dedicated accepted-preparation owner, not a clock adjustment to the legacy
PreparationPlan. A protected public Definition body declares the exact phase
contract, runtime/qualification, bounded PhasePolicy and nine owner identities.
Its immutable digest and actual runtime qualification are checked on every live
request. Absence of this policy refuses all v2 routes. No default feature inference
or legacy fallback exists. Existing worker authentication, current node/boot,
project eligibility and protected configuration owners remain authoritative.

A bounded authenticated proposal request names only identity/binding and exact
negotiation. The server reconstructs the proposal from the actual retained
NativeOffer, sealed execution spec, original selection decision, original Graph
read/context provenance, linked source export and retained original artifact
receipts. Choice/policy/owner pins are not accepted from the worker. The immutable
proposal body and digest are durably retained. All original proposal/source clocks
remain unchanged.

Timely Accept links that exact proposal to the original reservation/resource hold
and immutable operation ID/request digest. Under node → attempt/hot → project and
selection owners → shared quota suffix locks, first admission verifies the current
original offer window. Source retention and protected material reads happen
outside SQL locks, followed by fresh identity/configuration/time revalidation.
The new material lease is issued by the actual material owner; its private bytes
are never journaled. One atomic transaction stores the material lease, launch
material link, Reserved journal, hot Preparing owner and v2 accepted receipt.
Receipt IDs and expiries are created once. Its independent no-launch deadline is
bounded by policy, all original inline retention and the actual material lease.
No source observation or proposal is retimed or replaced.

A dedicated table retains immutable v2 proposal and accepted request/response
bytes/digests. Global and node count/JSONB-byte quotas share the existing preparation
quota lock and include both legacy and v2 retained state. Exact replay bypasses
new-admission quota but verifies actual stored identical state; missing or changed
rows refuse without reconstruction. Locks/statement deadlines and two owned
external-I/O permits bound outstanding work; no I/O runs under authority locks.

## Retrieval and historical provenance

Introduce a typed internal preparation authority view: LegacyPlan or AcceptedV2.
Each keeps original receipt kind, operation and canonical digest. The v2 variant
contains actual retained proposal/request/receipt, never a manufactured legacy
plan. Artifact, material and source owners use this view only for current no-launch
retrieval of exact original bytes during the accepted lease, after fresh runtime,
node/boot, project, source, cancellation/revocation and time checks. Source access
continues to require actual configured Git retention observations and same linked
manifest/object/policy; inline access requires actual retained descriptor/bytes.
An expired historical source observation is not fresh eligibility or new ref
resolution. Material reads verify original immutable file fingerprint and lease.

Legacy initial Grant must refuse v2 preparation: no legacy accept receipt exists.
The first slice exposes no new Prepare→execution route. OriginalGrant/renewal and
terminal closure must later consume an explicit phase-aware provenance view with
strict fresh confirmation; no fabricated InitialGrantDelivery is permitted.

Expiry stops retrieval and replay delivery; it does not assert physical stop or
free unknown resources. The current NeverGranted owner requires a legacy accept
receipt and therefore refuses v2 attempts. First-slice v2 expired attempts remain
held until separately qualified phase-aware cleanup/reconciliation exists. No
fallback expiry, replacement ID, implicit release, or automatic external retry.

## Independent qualification

Use disposable PostgreSQL, synthetic protected files/Graph peers, and actual
configured Git where meaningful. Prove timely admission and original artifact,
material and source retrieval beyond original proposal expiry but within accepted
expiry; immutable replay after restart; changed bytes/boot/runtime/project/source
and cancelled/revoked refusal; deadline recheck after waits; exact retention cap;
rollback/quota refusal without partial lease; no duplicate replay; legacy Grant
refusal and expired/unknown held state. Retain all synthetic/actual boundaries.
No provider, notification, process launch, deployment, PB04 or cold-workload closure
claim follows from these tests.
