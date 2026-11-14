# AQ-08 authority bridge review draft

**2026-11-13:** aligned with the rolling-records principle ([architecture 0006](../architecture/0006-records-are-rolling.md)); implementation pending. Only the retention bounds paragraph changed.

Root approved this bridge after stage 1 (`3520832`). Migration 0010 and the
kind-dispatched view are now implemented as a qualification checkpoint. No real
profiled grant/lifecycle routes are enabled. AQ21 consumes the same locked view
from its isolated branch and reserves migration 0011; AQ24 reserves 0012. The
actual SQL is [0010_profiled_authority.sql](../../crates/aq-server/migrations/0010_profiled_authority.sql).
The proposal below records the reviewed design; the linked migration is the exact
implementation authority. Overall AQ-08 scheduling/sealing remains in progress.

## Common identity, separate state decoders

Keep `execution_attempts` as the common central attempt identity/claim owner.
Its existing task/node/offer/session columns, foreign keys and unique unsettled
`task_id WHERE holds_task` index remain shared across kinds. Add `kind` with
`legacy_fixture` default and `profiled_v1`; do not construct a fake legacy Offer
or ExecutionSpec for a real agent. Existing `state` JSONB stays byte-equivalent
for legacy rows; profiled rows use an explicit separate versioned decoder.

The profiled immutable offer lives in `profiled_launches`. Its mutable `state`
contains status, acceptance/prepare references, exact grant, cancellation,
classified observations, verified stop/retention/effect references and result
reconciliation. No caller-provided boolean establishes stop or retention. The
common `holds_task` projection becomes false only after kind-specific verified
nonstart/stop plus required retention and unresolved-effect checks. Expiry never
clears it. The same unique index prevents mixed-kind double admission. Stored common state
is capped at 4 MiB of uncompressed JSON text. Revisions are always derived from
OLD values plus detected state changes; supplied revision writes are ignored and
no-op/exact replay cannot advance or decrease them. Common reads project only the
resolved selection from retained decisions, excluding startup input bodies.

`execution_attempts` central ownership is not a second graph task-state owner.
It records authorization and retained attempt evidence, not task readiness,
completion or graph lifecycle. A graph-ready observation remains non-claiming.

## Proposed migration 0010 SQL

The following is a concrete schema proposal, not an installed SQL file. Root
review may split independent terminal ledger work into its own later migration.
All UUID/digest and JSON bounds are also validated by typed services; SQL CHECK
expressions do not authenticate evidence or validate process stop.

```sql
ALTER TABLE execution_attempts
 ADD COLUMN kind TEXT NOT NULL DEFAULT 'legacy_fixture'
   CHECK (kind IN ('legacy_fixture','profiled_v1')),
 ADD COLUMN record_revision BIGINT NOT NULL DEFAULT 1 CHECK(record_revision>0),
 ADD COLUMN authority_revision BIGINT NOT NULL DEFAULT 1 CHECK(authority_revision>0),
 ADD COLUMN grant_revision BIGINT NOT NULL DEFAULT 0 CHECK(grant_revision>=0),
 ADD COLUMN execution_deadline_ms BIGINT CHECK(execution_deadline_ms>0);
ALTER TABLE execution_attempts ADD CONSTRAINT execution_profiled_state_shape
 CHECK (kind='legacy_fixture' OR
   (jsonb_typeof(state)='object' AND state ? 'schema_version'
    AND state->>'schema_version'='1' AND state ? 'status'));
CREATE INDEX execution_kind_node ON execution_attempts(kind,node_id,attempt_id);

CREATE TABLE admission_source_policies (
 project_id UUID PRIMARY KEY REFERENCES project_execution_bindings(project_id),
 revision BIGINT NOT NULL CHECK(revision>0),
 policy_digest TEXT NOT NULL CHECK(policy_digest ~ '^[0-9a-f]{64}$'),
 policy JSONB NOT NULL CHECK(octet_length(policy::text)<=65536)
);
CREATE TABLE admission_source_observations (
 observation_id UUID PRIMARY KEY,
 project_id UUID NOT NULL REFERENCES project_execution_bindings(project_id),
 source_policy_revision BIGINT NOT NULL CHECK(source_policy_revision>0),
 observation JSONB NOT NULL CHECK(octet_length(observation::text)<=65536),
 observed_at_ms BIGINT NOT NULL CHECK(observed_at_ms>0),
 expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>observed_at_ms)
);

CREATE TABLE admission_artifacts (
 artifact_id UUID PRIMARY KEY,
 descriptor JSONB NOT NULL CHECK(octet_length(descriptor::text)<=65536),
 content_digest TEXT NOT NULL CHECK(content_digest ~ '^[0-9a-f]{64}$'),
 byte_length BIGINT NOT NULL CHECK(byte_length>0 AND byte_length<=1073741824),
 owner_kind TEXT NOT NULL CHECK(owner_kind IN
   ('catalog','graph_snapshot','composition','selection','outcome','source_retention')),
 owner_key TEXT NOT NULL CHECK(octet_length(owner_key) BETWEEN 1 AND 512),
 -- Actual central bytes only for bounded nonsecret snapshots. Large Git source
 -- remains with the existing source owner and its real retention receipt.
 body BYTEA CHECK(octet_length(body) BETWEEN 1 AND 2097152),
 created_at_ms BIGINT NOT NULL CHECK(created_at_ms>0),
 retain_until_ms BIGINT NOT NULL CHECK(retain_until_ms>created_at_ms)
);
CREATE INDEX admission_artifact_digest ON admission_artifacts(content_digest);

CREATE TABLE admission_material_leases (
 lease_id UUID PRIMARY KEY,
 operation_id UUID NOT NULL,
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
 node_id UUID NOT NULL REFERENCES nodes(id),
 boot_id UUID NOT NULL,
 prospective_attempt_id UUID NOT NULL,
 admitted_attempt_id UUID REFERENCES execution_attempts(attempt_id),
 offer_id UUID NOT NULL,
 credential_identity TEXT NOT NULL CHECK(octet_length(credential_identity) BETWEEN 1 AND 512),
 credential_revision BIGINT NOT NULL CHECK(credential_revision>0),
 purpose TEXT NOT NULL CHECK(octet_length(purpose) BETWEEN 1 AND 512),
 lease JSONB NOT NULL CHECK(octet_length(lease::text)<=65536),
 verifier TEXT NOT NULL UNIQUE CHECK(octet_length(verifier)=64 AND verifier ~ '^[0-9a-f]{64}$'),
 state TEXT NOT NULL CHECK(state IN ('pending','admitted','revoked','expired')),
 issued_at_ms BIGINT NOT NULL CHECK(issued_at_ms>0),
 expires_at_ms BIGINT NOT NULL CHECK(expires_at_ms>issued_at_ms
   AND expires_at_ms-issued_at_ms<=120000),
 CHECK(admitted_attempt_id IS NULL OR admitted_attempt_id=prospective_attempt_id),
 UNIQUE(node_id,operation_id)
);
CREATE UNIQUE INDEX admission_one_live_material_purpose
 ON admission_material_leases(offer_id,credential_identity,purpose)
 WHERE state IN ('pending','admitted');
CREATE TABLE admission_material_receipts (
 lease_id UUID PRIMARY KEY REFERENCES admission_material_leases(lease_id),
 operation_id UUID NOT NULL,
 receipt_digest TEXT NOT NULL CHECK(receipt_digest ~ '^[0-9a-f]{64}$'),
 receipt JSONB NOT NULL CHECK(octet_length(receipt::text)<=65536)
);

CREATE TABLE profiled_launches (
 attempt_id UUID PRIMARY KEY REFERENCES execution_attempts(attempt_id),
 decision_id UUID NOT NULL REFERENCES intelligence_decisions(decision_id),
 source_observation_id UUID NOT NULL REFERENCES admission_source_observations(observation_id),
 launch_binding JSONB NOT NULL CHECK(octet_length(launch_binding::text)<=65536),
 launch_manifest JSONB NOT NULL CHECK(octet_length(launch_manifest::text)<=2097152),
 manifest_digest TEXT NOT NULL CHECK(manifest_digest ~ '^[0-9a-f]{64}$'),
 execution_spec JSONB NOT NULL CHECK(octet_length(execution_spec::text)<=2097152),
 execution_spec_digest TEXT NOT NULL CHECK(execution_spec_digest ~ '^[0-9a-f]{64}$'),
 graph_task_observation JSONB NOT NULL CHECK(octet_length(graph_task_observation::text)<=1048576),
 source_binding JSONB NOT NULL CHECK(octet_length(source_binding::text)<=65536),
 offer_expires_at_ms BIGINT NOT NULL CHECK(offer_expires_at_ms>0)
);
CREATE TABLE profiled_launch_artifacts (
 attempt_id UUID NOT NULL REFERENCES profiled_launches(attempt_id),
 artifact_id UUID NOT NULL REFERENCES admission_artifacts(artifact_id),
 PRIMARY KEY(attempt_id,artifact_id)
);
CREATE TABLE profiled_launch_materials (
 attempt_id UUID NOT NULL REFERENCES profiled_launches(attempt_id),
 lease_id UUID NOT NULL REFERENCES admission_material_leases(lease_id),
 PRIMARY KEY(attempt_id,lease_id)
);
CREATE TABLE profiled_execution_receipts (
 node_id UUID NOT NULL REFERENCES nodes(id),
 operation_id UUID NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN
  ('accept','prepare','renew','observation','stop','result','reconcile','cancel')),
 request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
 response JSONB NOT NULL CHECK(octet_length(response::text)<=262144),
 PRIMARY KEY(node_id,operation_id)
);
CREATE TABLE scheduler_projects (
 project_id UUID PRIMARY KEY REFERENCES project_execution_bindings(project_id),
 policy_revision BIGINT NOT NULL CHECK(policy_revision>0),
 policy JSONB NOT NULL CHECK(octet_length(policy::text)<=65536),
 next_graph_cursor TEXT CHECK(octet_length(next_graph_cursor)<=4096)
);
CREATE TABLE scheduler_cursor (
 singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK(singleton),
 revision BIGINT NOT NULL CHECK(revision>0),
 after_project UUID REFERENCES project_execution_bindings(project_id)
);
INSERT INTO scheduler_cursor(singleton,revision) VALUES(TRUE,1);
-- meta state_format_version=10 and release metadata only after approval.
```

Migration constraints must make `kind` and common attempt identity immutable
via a trigger, and must refuse execution_deadline_ms extension once set. The
trigger proposal checks OLD/NEW task_id,node_id,offer_id,session_id,kind equality;
allows NULL -> first actual granted deadline; thereafter requires exact equality.
No trigger decodes or rewrites legacy JSON. Full body ownership and revision
increments remain in typed state transitions, with expected record_revision CAS.

Pending material lease rows intentionally have no FK to an attempt that does not
yet exist. They are non-deliverable prospective bindings. Admission inserts the
actual profiled attempt and atomically links `admitted_attempt_id`; delivery
requires this link and current offered/preparing/granted authority. No pending
lease can create an execution claim or grant. Expired pending rows retain stable
operation receipts; a new lease uses a new operation ID. Rotation/revocation is
checked again before delivery and grant.

Raw uncompressed JSON text lengths and bytea lengths bound storage; no quota
uses compressed `pg_column_size` as a proxy for canonical raw content. Material
verifiers are constrained to exactly 64 lowercase hexadecimal bytes.

The existing `intelligence_attempt_selections` and `agent_launch_bindings` FK
rows are inserted in the same admitted-offer transaction, not populated by an
owner route on a fixture attempt. No foreign keys need to be moved off the common
attempt table. Scoped graph/API01 profile/context digests stay independent from
execution spec, composed context and native credential fingerprints.

Initial service bounds count live rows only: 1,024 pending/live material leases
and 16 MiB metadata, 4,096 profiled operation receipts/16 MiB per node, 1,024
source observations/16 MiB, 1,024 central artifact bodies/64 MiB and 128
descriptors per launch. Rows of settled runs do not count. Include incoming
uncompressed JSONB/bytea lengths in these checks before insert. Live/referenced
rows are pinned: nothing deletes replay or stop proof while its run is live or
its task is open. Settled history is deleted under
[spec 0008](../specs/0008-sealed-history-retention.md) phase B, following
[architecture 0006](../architecture/0006-records-are-rolling.md); history growth
never refuses new work (2026-11-13: aligned with the rolling-records principle;
implementation pending). Artifact fetch resolves only typed owner_kind/owner_key through fixed
adapters; it accepts no caller filesystem path or URL.

## AuthorityView exact read contract

A kind-dispatched locked read yields this internal view, not a wire grant:

```text
AuthorityView {
 kind: LegacyFixture | ProfiledV1,
 session: SessionIdentity, task_id, workspace_id?, project_id,
 record_revision, authority_revision, grant_revision,
 status: Offered | Preparing | Granted | Cancelling | Finished | Declined | Unknown,
 grant_id?, grant_expires_at_ms?, execution_deadline_ms?,
 execution_spec_digest, launch_manifest_digest?,
 graph_access_profile_digest?, graph_context_digest?, graph_authority_revision?,
 source_binding?, selection_digest?,
 cancellation_id?, cancellation_revision?, revoked: bool,
 stop: VerifiedNotStarted | VerifiedStopped(receipt) | Running | Unknown,
 retention: Verified(receipt) | RequiredPending | NotRequired | Unknown,
 effects: Quiescent(receipt) | Pending(count) | Unknown
}
```

`record_revision` advances on every actual state write. `authority_revision`
advances on identity/grant replacement, cancellation, revocation, unsafe/unknown
transition and permanent end of execution; it stays stable during same-grant TTL
renewal. `grant_revision` advances for accepted renewals. Exact replay never
advances any revision. Graph credential authority has its existing separate
revision; graph credential revocation does not silently redefine terminal ACLs.

A profiled execution hard deadline is fixed at its actual first grant from
qualified wall/lineage budgets and cannot extend. Pregrant deadline is absent.
Legacy state has no invented hard deadline or manifest/selection: absent fields
remain absent. The implementation preserves typed profiled stop/retention/effect
projections separately from legacy flags; they are not verified by shape alone.
TerminalActive additionally refuses unknown profiled host stop/running evidence. It is not eligible for profiled terminal/material behavior merely
because its legacy grant exists. Expiry/status are sampled again after lock waits.
`stop`, `retention` and `effects` only reflect actual verified owning-domain
receipts; no caller assertion or time elapse upgrades them.

Graph access requires exact current node/boot/attempt/session/generation,
Granted/unexpired/noncancelled state and independent graph binding/profile scope.
Source transport deliberately supports late expired/cancelled owned attempts for
retention, while freezing exact source/spec/attempt and never renewing execution.
These are separate policies over the same identity view, not a single `active`
boolean shared by every consumer.

## Decoder and query inventory

Line references describe the reviewed source baseline; re-run rg before edits.
Every direct legacy parse must be gated by kind, or replaced with the common
view. Cross-kind node recovery must never ignore a profiled writer. The initial
checkpoint refuses incarnation replacement while ANY profiled claim is held;
a typed receipt reference alone is not owner verification. This uses bounded
EXISTS queries rather than loading all held launch manifests.

| Existing location | Required bridge |
| --- | --- |
| execution.rs load_attempt (138), save_attempt (179) | strict legacy kind gate; revision-aware writer; profile decoder separate |
| execution.rs replacement_safe (301) | common conservative writer/stop projection over BOTH kinds, not JSON assumption |
| execution.rs retire_unaccepted (307) | dispatch Offered/Preparing revocation per kind; no start after retirement |
| execution_register (350), replace_incarnation (1172) | inspect both kinds under node lock; any unverified writer blocks replacement |
| execution_offer (413), existing-row parse (439) | legacy kind explicit; shared unsettled claim conflicts with real attempts |
| execution_poll (479), offered-row parse (489) | legacy kind predicate; actual profiled poll separate and bounded resource-fit first |
| accept/renew/observe/result (530/605/675/744) | strict kind before legacy load; old wire/digests unchanged |
| cancel/report/get (875/928/942), decline (947) | generic owner cancellation dispatch; legacy inspection/decline never parse profiled JSON |
| directive ack (1011), reconcile per-attempt parse (1112), incarnation loop (1180) | kind-aware routing; omitted/mismatched inventory never hides a writer |
| execution/api.rs inspect/cancel (244/258) | return kind-specific versioned inspection or explicit unsupported-kind; no fake legacy envelope |
| agent_access.rs lock_attempt (357/367/373) | common view with graph-specific current authorization policy |
| agent_access.rs provision (following lock_attempt), revoke/issue/principal | trusted profiled binding installed atomically; generic exact identity/status/digest lookup, graph revision stays separate |
| agent_access.rs revoke node/attempt locks (470) | same canonical order; graph delegation revoke does not become process stop |
| agent_access/mutations and messaging | currently consume parent authority helper; requalify all dispatch/replay/snapshot/effect fences after helper adaptation |
| source_retention.rs authority (203/214), binding (226), check_manifest (234) | common view; allow late owned source retention and compare exact frozen source/spec/attempt |
| source_retention.rs artifact owner lookup (820) | common identity only; never cast state implicitly |
| intelligence/feedback.rs (59/67) | real ProfiledV1 + current grant + stored exact sealed selection; no legacy fixture feedback |
| intelligence/resolver.rs task counts (31/329) | intentionally count BOTH kinds; SERIALIZABLE predicate prevents mixed-kind retry reset |
| migrations 0003/0004/0005/0007/0009 FKs | remain common execution_attempts IDs; no fabricated legacy state required |
| execution/testing.rs recovery instrumentation and tests | feature-gated fixture helpers remain legacy; add mixed-kind fixtures through genuine typed profiled builder |

## Lock order and terminal registry consumption

Admission order: node/current boot -> project -> active catalog -> intelligence
registry/current node capability -> source policy/observation -> material leases
(sorted lease ID) -> immutable decision/artifact rows (sorted ID) -> shared task
claim/attempt insertion -> scoped graph binding. SERIALIZABLE predicate reads
cover task attempt/budget claims. Preparation performs external Git/Graph/file
I/O before this transaction; final freshness checks occur after all waits.
No path may acquire node/project/catalog locks after holding the intelligence
registry/material/terminal rows. Source policy writers take project before policy;
material configuration installation takes registry only and never node.

Existing-attempt operations: node (or future independent central-authority row)
-> exact attempt/session authority -> current scoped authorization -> terminal
ledger -> controller/command/subscription/outbox rows in deterministic IDs.
Source transfer/effect child rows follow their owning attempt and current scoped
authorization. No handler holds a terminal ledger then waits for attempt/node.
An operation touching admission facts and an existing attempt must take all
admission-prefix locks first, then the attempt; the final grant path uses this
expanded order. Cancellation can lock node->attempt directly because it never
later waits for project/catalog/registry. Review transaction cycles before merge.

AQ21 uses `Access.session_revision = AuthorityView.authority_revision`, with exact
SessionIdentity, status, current grant ID/expiry and immutable hard deadline.
Separate terminal registration binds TerminalIdentity (terminal ID+epoch) to that
session. Peer registration checks exact node/boot/attempt/session/generation; any
host guardian/writer-set reference is separately verified host evidence, not a
PID-derived central identity or proof inferred from output.

Acquire/renew clamps the controller to freshly locked grant/hard expiry; an
existing issued lease never extends automatically on execution renewal. Cancel,
revoke or unknown authority denies input/resize/dispatch until explicit registry
reconciliation. Every injection/resize dispatch rechecks current authority and
independent control lease/generation/epoch. Watch and historical receipts/output
may remain available under current scoped human authorization after execution
ends; neither watch nor graph API scope grants control. No terminal operation
launches or renews execution. Central sessions require their real central
identity/authority adapter; there is no fake worker row.

## Merge gates

Before schema installation: root approves exact state fields, constraint/trigger,
query inventory, receipt semantics and lock graph. Before first real grant:
qualified source/outcome/context artifacts, conservative qualified token
accounting, provider material ownership/verification and public profiled runtime
transport are wired end to end. First attempts only and explicit unsupported
hard-accounting/replacement policies are permitted initial refusal gates.
Tests must prove mixed-kind claim exclusion/recovery, same-byte legacy outputs,
no state decoder crossovers, current graph/source/auth scoping, material expiry/
rotation/mismatch and terminal-current-authority reads under actual lock races.
