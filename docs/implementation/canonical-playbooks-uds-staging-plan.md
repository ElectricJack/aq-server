# Proposed canonical Playbooks owner through a private UDS clone

Status: proposal after the successful actual private-UDS AQ37/component0.1.3
proof. No canonical database reads, writes, configuration changes, clone, or
service lifecycle operation were performed to write this plan. Canonical still
uses its existing core database; optional Playbooks remain unavailable there.

The current engine connection loader requires a real protected Unix endpoint.
The canonical core TCP endpoint on55432 cannot be combined with a different
private engine database merely because names match. Core SQLx, bridge and engine
must share one physical cluster/database. Keep the existing core database and
config as the rollback authority until a separately approved migration succeeds.

## Smallest source prerequisite: genuinely empty configured owner

Current configuration rejects an empty deployments array. Runtime bootstrap also
captures a startup definition profile whose existing contract requires1..4items.
An empty current installed catalog is supported by the component itself.

Proposed bounded change:

1. Permit explicit `deployments=[]`, retaining the maximum4, required field,
   private socket/files/key checks, namespace validation and all deployment checks
   for nonempty configuration.
2. Skip the startup-only LoadedDefinitions capture when there are zero configured
   artifacts. Its existing minItems1 contract and parser remain unchanged; that
   historical capability stays unavailable. Current catalog capability remains
   tied to the actual live attached owner and supported query interface.
3. Qualify actual empty-owner startup, namespace/catalog reads, two idle quanta,
   stop/restart and no grants/artifacts/bindings/work created. Existing nonempty
   inert definition and protected current-catalog tests remain regressions.

This requires no dummy policy, artifact, grant, recipient, expiry or activation.
Runtime initialization still creates the real namespace/catalog-profile row,
protected engine state, lease/incarnation and recovery metadata. No workflow is
admitted. Automatic expiry/event collection is joined to accepted effects of this
namespace; the new empty namespace has none and does not process other namespaces'
review effects. Automatic quanta still take shared review-budget/namespace locks;
this plan does not claim zero database activity.

## Staged data and configuration transition

The following steps require root coordination before reading canonical data or
starting another daemon:

1. Record the current canonical binary/assets/config pins, actual database name,
   PostgreSQL version/encoding/collation/extensions/roles, actual Owner identity,
   and enabled project→workspace bindings. Reuse a real existing project and its
   actual owner; do not insert a fixture, mark an unavailable source Verified, or
   create Graph/task/worker eligibility. If no suitable binding exists, leave
   owner configuration unavailable and report the missing actual prerequisite.
2. Take a consistent private canonical database backup through the current
   database's existing client/container tooling. Retain it0600 with checksum and
   source identity. Use a NEW uniquely named database in the already owned private
   UDS PostgreSQL instance; do not reuse the inert proof database or a preexisting
   database. No new PostgreSQL server or network service is required.
3. Restore the complete data/schema/sequence state, preserving owner/privilege
   semantics or explicitly resolving required roles before restore. Do not blindly
   discard ACLs/ownership or clear records to make the optional owner look empty.
   Compare stable table/row/sequence/authority fingerprints against the snapshot,
   distinguishing expected migrations from retained user data. Check capacity
   before using the existing512MiB private PostgreSQL service for the extra DB.
4. Generate a separate0700 stage vault and0600 core/engine connection files to
   this exact new UDS database. Generate new bridge and engine keys once with
   explicit provenance; never copy the inert proof namespace/grant or regenerate
   keys on restart. Use a fresh operator-chosen namespace identity, generation1,
   actual Owner UUID and actual project/workspace IDs from the restored records.
   Namespace name should identify the local development owner accurately.
5. Preserve the canonical config's auth, Graph/Git/source/profile settings and
   adapter identities. Change only stage loopback listener/public-origin values,
   core database pointer and explicit optional-owner section. Use `deployments=[]`.
   No artifact or grant is configured. Serve the immutable624707…e663 bundle with
   the separately qualified default37+empty-owner backend.
6. Start only the stage daemon. Verify ready/source/schema, principal identity,
   actual existing projects/Graph/usage/Work/Results/Settings and preserved data;
   confirm SQLx/bridge/engine clients use the same physical UDS DB. Confirm current
   Definitions/Activations return authoritative empty pages, Runs/admissions are
   empty only for the new namespace, and Decisions reflect actual retained reviews
   rather than assumed emptiness. Exercise GET-only1440/390/320 UI and graceful
   stop/restart. Compare work/effect counters to the restored baseline; never
   assert existing canonical work must be zero.

## Canonical promotion and rollback are separate

A successful stage clone is a point-in-time copy, not a synchronized canonical
replica. Canonical can continue receiving writes while stage is assessed. Before
promotion, root must stop/quiesce every writer to the old database, capture a final
consistent backup, and refresh/requalify the new target so no writes are lost.
Do not claim that the earlier stage snapshot is current. Exact handling of worker
heartbeats, other processes and users with direct DB access must be recorded.

Promote the source-stamped backend, same qualified dashboard, private vault and
both matching UDS pointers together. Preserve old database/config/binary/assets
and record which database is authoritative from the cutover instant. Only one
canonical AQ writer set may resume. Initial browser proof remains read-only.

Rollback before target user writes can stop the new daemon and return to the
retained unchanged old database/config/binary/assets. Once the target accepts new
user writes, switching to the old snapshot would lose them. Freeze writers and
preserve the target for forward repair or a reviewed data reconciliation/export;
there is no automatic safe divergent-database rollback. This plan does not make
that decision or perform promotion.

## Source prerequisite qualified

The [bounded explicit-empty owner change](evidence/empty-playbooks-owner-2baa8e7/README.md)
is now source-reviewed and qualified on composed AQ37. Current empty inspection is
supported without changing the frozen startup1..4 profile. A retained namespace
with `[]` remains populated if actual installed state already exists. Later stage
preparation and any canonical cutover require their own retained data/provenance
record; the initial proposal above is not a promotion claim.
