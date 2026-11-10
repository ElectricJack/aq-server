# dashboard-actions-1

This server-owned contract adds `POST /api/v1/actions` and
`GET /api/v1/operations/{operation_id}` to the authenticated human router.
All mutations require `X-AQ-Request: 1` and the existing same-origin policy.
Production registers `task.create`, `decision.resolve`,
`supervisor.message.send` and the typed task and knowledge kinds listed below.
The owner-only `dashboard.actions@1` capability advertises this boundary. Check
each mutation capability as well: task creation requires the configured Graph
mutation identity, decision resolution requires a mounted review owner, and
supervisor send requires the mounted global mailbox. Each typed kind has its own
bootstrap capability named after the kind (for example `task.retry@1`).

## Admission and recovery

Requests carry `operation_id` (non-nil UUID), `kind` (dotted domain.verb),
`subject` (`{type,id,revision?}`), `expected_versions` (positive decimal revision
strings), optional bounded `reason`, and a kind-specific strict `payload`.
The entire request is limited to 65,536 bytes; kind names to 128 bytes,
entity types to 64, entity IDs to 128, reason to 1,024, and expected-version
entries to 32 (keys up to 128 bytes; revisions up to 40 decimal digits).
References/reason reject control characters. Operation receipts have at most
100 receipt and 100 affected-entity refs and a 65,536-byte storage ceiling.
Unknown kinds and fields return 422. A registry is assembled from trusted Rust
adapters at startup, never from a client-supplied command, URL or shell string.
An adapter's `Payload` must deny unknown fields and its Schemars schema must
agree with deserialization. Each adapter must check current subject/scope
permission and expected versions; hints and old receipts confer no authority.

Canonicalization recursively sorts JSON object keys lexicographically and uses
Serde JSON compact UTF-8 serialization (array order preserved, no whitespace).
The SHA-256 digest includes the entire validated request and the server-derived
scope. Object-key order and insignificant JSON whitespace do not change identity;
changed values, fields, array order or scope do. Numbers use Serde JSON's finite
number representation; future kinds should use decimal strings for revisions
and lossless large integers. JSON duplicate fields are invalid.

Admission commits the original principal, scope, canonical digest and pending
operation before any adapter runs. Only the request that inserts the operation
may dispatch. Concurrent and later retries return the retained disposition;
changed identity returns 409. Authentication and adapter authorization run on
every retry. No request retry redispatches an admitted operation. Adapters must
implement durable domain-specific reconciliation before offering a recovery
that might cause effects. A crash leaves pending evidence; a dispatch timeout
or ambiguous effect is unknown. HTTP 202 means pending/unknown, never completion.

Operation lookup first filters by principal, then invokes the kind's mandatory
`authorize_lookup` policy on the current scope and every returned reference.
The policy must separately authorize a retired scope-only tombstone. Keep lookup
policies installed for retained kinds; missing policy or revoked access fails
closed with neutral 404. No request payload is retained just for read authorization.

Operation lookup is principal-private; missing IDs and another principal's IDs
both return neutral 404. A missing row means **admission unknown**. Preserve the
original ID and request bytes. Operation responses identify the digest, scope,
created/updated timestamps, subject refs, receipt refs, affected entities and
permitted recovery. Confirmed means only the effects named in that receipt.

Ordinary terminal receipts survive at least 90 days after their last update.
Pending and unknown operations never expire. Retirement erases response details
but preserves immutable ID/principal/scope/digest tombstones indefinitely; 410
for the original principal means retired, and the ID can never execute again.
Retention is published on the action capability. Cleanup is explicit;
keeping receipts longer than the minimum is safe.

## Enqueue work

`task.create` also accepts optional `priority` (integer 1–300), `profile_hint`,
`intelligence_hint` and `task_type`. Hints are nonblank, without control characters,
and at most 128 UTF-8 bytes. They become `aq:priority`, `aq:profile_hint`,
`aq:intelligence_hint` and `aq:task_type` metadata on the new Graph task. Omitted
fields preserve native task-create serialization and defaults. These hints confer
no execution or credential authority.

`task.graph.create` has a `project` subject, matching `project_id` and
`expected_version`. Its `tasks` array contains explicit nonnil task `id`s and
`title`s, plus optional body, acceptance criteria, labels, task_state (default
`active`), parent_id and the same enqueue metadata. Its optional `edges` array
contains `{id,source,target}` blocks edges; at least one endpoint must be a new
task. Existing endpoints and parents must be live tasks in the selected project
and workspace. IDs must be unique across nodes and edges; self and duplicate
blocks edges are invalid. Parent links produce deterministic `parent-child` edges.
Graph owns cycle and other domain validation.

All nodes, blocks edges and parent links are submitted as **one atomic Graph
mutation batch of at most 100 total operations**, without chunking. The exact
Graph body and `aq-dashboard-action:{operation_id}` key are retained before the
POST. Reconciliation on lookup or repeated submit replays those bytes only,
checking the original binding and Graph service identity. A first Graph conflict
is terminal; an ambiguous response remains unknown until the original receipt
settles it. Owner bootstrap advertises `tasks.graph.create@1`.

The separate worker listener exposes the same `/api/v1/actions` and
`/api/v1/operations/{operation_id}` paths for supervisors, using F1b's active,
generation-bound `Authorization: Bearer ...` credential with `enqueue` scope.
Only `task.create` and `task.graph.create` are registered there. Credential and
lifecycle locks remain held through admission, dispatch and response; lookup
rechecks scope, expiry and revocation. No human Owner identity is manufactured.
Receipts are private to the supervisor session UUID, preserved across credential
rotation within that session. Owner and session principal references are disjoint
in the operation journal (state format 58). Owners continue to use the human
listener and its authentication and CSRF policy.

## Errors and cursors

All `/api/v1/*` errors use `request_id`, `code`, `message`, `retryable` and
optional `operation_id`, `field_errors`, `current_versions`, `retry_after_ms`.
Error messages are static safe text; request values, SQL errors and credentials
never enter them. Required terminal clock and retry headers survive normalization.
The common error envelope covers all `/api/v1/*` families; specialized terminal
success/effect semantics remain unchanged. Cursor expiry uses 410 `cursor_restart_required`: restart the
list while preserving the reading anchor. Authentication is 401, authorization
403, neutral absence 404, conflicts 409, size 413, validation 422, unsupported
required contract 426 and capacity/unavailability 429/503.

The shared cursor codec uses random process-local keys, authenticated encryption
and a versioned domain separator. It binds principal, authorization partition,
scope, normalized filters, sort, page size, resource and snapshot, plus position
and validity interval. A restart invalidates existing tokens. Every consumer must
still recheck current authorization and snapshot membership; tokens grant no access.

## Publication and verification

Generate the strict schema with
`cargo run --locked -p aq-server --example dashboard_actions_contract`;
append `-- --check` to verify drift. Run `python3
protocol/dashboard-actions/v1/check.py` for schema, byte bounds, fixture replay
relationships and read/action version compatibility. Test-only adapters never
enter the production schema. Dashboard consumer pinning belongs to G3/G8.

The action payloads retain the native write fields, including `operation_id`,
which must equal the outer operation ID. `expected_versions` contains exactly
one key (`project`, `decision` or `conversation`), matching both the subject's
revision string and the payload's expected revision. The subject type uses that
same key and its ID is the actual project, decision or conversation UUID.

- `task.create` uses `human-task-create-1`: title at most 512 UTF-8 bytes, body
  32768 UTF-8 bytes, acceptance criteria 8192 bytes, at most 16 distinct labels,
  and `draft` or `active`. The Graph task ID equals the operation ID. A confirmed
  operation references the validated Graph change set and exact task revision.
  Native human requests retain their original byte-sensitive request identity.
- `decision.resolve` includes the native resolve fields and `expected_subject`,
  the exact review subject artifact (`id`, `version`, `digest`). Both the decision
  revision and immutable subject are bound. The existing protected review owner
  checks these pins under its authority locks and retains the final receipt.
- `supervisor.message.send` keeps the caller's separate `message_id`, an expected
  conversation revision, a nonempty body of at most 16384 UTF-8 bytes, and empty
  `context` and `attachments` arrays. Confirmed means durable mailbox enqueue;
  native receipts still say host delivery `not_attempted`, with no agent receipt.

- `session.message.send` durably enqueues an owner message into one session's
  scoped mailbox. The subject is `{type:"session",id:<session UUID>}`; the
  payload carries `operation_id`, `project_id`, a separate `message_id`, optional
  `thread_id` (defaults to `message_id`), `expected_generation` (the decimal
  `session_identity.assignment_generation`, also the subject revision and the
  single `session` expected version) and a nonempty body of at most 16384 UTF-8
  bytes. The recipient is derived from the session's unrevoked launch binding,
  never from client scope; a missing, revoked or other-project binding is a
  neutral 404. A stale generation is a `conflict` with `current_versions`
  `{session:<live>}`. Confirmed means queued only: the receipt names the
  `mailbox_operation` and the affected `message` at its mailbox cursor. Runner
  delivery and harness input are not attempted here. Owner bootstrap advertises
  `session.message.enqueue@1`; session detail lists `session.message.send` in
  `permissions` when the slot is usable.

## Typed task and knowledge kinds

Each typed payload carries `operation_id` (equal to the outer ID), `project_id`
and `expected_version`, the decimal Graph node version that must also equal the
subject revision and the single `expected_versions` entry (`task` or
`knowledge`). `knowledge.author` instead uses `expected_project_revision` with a
`project` subject. A stale version returns a `conflict` whose operation carries
`current_versions` with the live Graph version; nothing is written.

- `task.edit` patches title, body, acceptance criteria or labels. Every task
  and knowledge title (create, edit, author, correct) is 1 to 512 UTF-8 bytes,
  Graph's cap; schema `maxLength` values count characters and are upper bounds.
- `task.reprioritize` writes Graph metadata `aq:priority` (integer in
  -1,000,000..1,000,000, higher first) and preserves other metadata keys.
- `task.dependency.add` adds a `blocks` edge from `blocker_id`;
  `task.dependency.remove` removes `edge_id` at `edge_version`.
- `task.cancel` records Graph task lifecycle `canceled`, including draft tasks
  without attempts. `reason` is optional. A central cleanup cursor requests B3
  cancellation for any holding attempt, retries after restart, and records the
  existing stopped disposition only after verified stop proof.
- `task.delete` tombstones the selected task and atomically removes its incident
  relationships/pointers. Dependencies, hierarchy, lifecycle and workflow gates
  do not prevent deletion. Other tasks and retained history survive. Native
  attempt cleanup follows the same automatic path as cancellation.
- `task.retry` asks B4 to assess the task; only an admissible replacement
  records the predecessor's replacement release (receipt `replacement_release`),
  after which the scheduler starts the replacement attempt. An unsettled or
  exhausted predecessor is `rejected`. Retry does not change Graph task lifecycle.
- `knowledge.author` creates an unverified note (default content type `note`)
  whose Graph node ID equals the operation ID.
- `knowledge.correct` patches title, body, labels or content type.
- `knowledge.verify` requires a `reason` and 1 to 64 Graph `Source` evidence
  objects with verdict `verified` or `disputed`. Graph may answer `forbidden`,
  which is `rejected`.
- `knowledge.retire` requires a retirement reason and an optional successor;
  retiring an already retired record is `rejected` and writes nothing.
  `knowledge.restore` restores an earlier `revision`.

Graph writes use the idempotency key `aq-dashboard-action:{operation_id}` and a
confirmed operation references the Graph change set and the exact new revision.

## Playbook kinds

Offered only when the playbook runtime is mounted and the principal is its
namespace owner. Each kind has a bootstrap capability: `playbooks.runs.start`,
`playbooks.runs.cancel`, `playbooks.activations.activate` and
`playbooks.activations.rollback`. Payloads carry `operation_id` and the
namespace's `project_id`; any other project returns 404.

- `playbook.run.start`: subject `playbook_binding` at `expected_version`, the
  binding's activation revision, which is also the single `expected_versions`
  entry. The payload names `binding`, the active `artifact`, `entry` and up to
  32 typed `inputs` (boolean, number, or text up to 4096 bytes). No inputs means
  a `null` input. The run is admitted with the binding's grant and the engine
  refuses it if the activation moved. The receipt is a `playbook_admission`, and
  the affected entity is the `playbook_run`. If admission is not yet proven, the
  operation reports `pending` or `unknown`, and a lookup reconciles the
  retained admission.
- `playbook.run.cancel`: subject `playbook_run` with no revision, empty
  `expected_versions` and payload `run_id`. An optional envelope `reason` is
  accepted. The engine's cancel is idempotent per run, and confirmation means
  cancellation was requested, not that the run stopped.
- `playbook.activation.activate` binds `artifact` to `binding` for new runs.
- `playbook.activation.rollback` restores the binding's previous activation;
  `artifact` must be the currently active one. With nothing to roll back to,
  the operation is `rejected`.

Both activation kinds use the same `playbook_binding@expected_version` subject.
If the revision or artifact is stale, the result is `conflict` with
`current_versions.playbook_binding`. Changes are idempotent per operation ID,
and the receipt is `playbook_activation_operation`. The version check before
activate or rollback is not atomic with the change.

Legacy task-create, decision-resolve and supervisor-send HTTP handlers enter the
same action admission path and retain their native response envelopes. A legacy
message/decision handler racing an admitted operation before its native commit
can return a retryable unavailable response. Preserve the original ID and look
it up. Operation lookup reconciles existing native receipts without dispatch.
Explicit native task recovery retains its original fixed Graph key and bytes;
a later union lookup reflects its receipt. Native supervisor requests keep their
128 KiB JSON envelope limit, including escaping overhead; union HTTP requests
retain their 64 KiB envelope limit. Both enforce the native UTF-8 body bound.

Bootstrap remains `dashboard-read-1`. Its separate `contracts` entries advertise
both schema digests and versions. For the current owner, `pending_operations`
contains at most 100 own pending/unknown IDs, ordered by admission time then ID.
This includes unresolved pre-upgrade human Graph journal rows, deduplicated
against union history. Looking up a native-only task validates native evidence and
current access, imports admission metadata, and reconciles without dispatch.
Terminal, retired and other principals' rows are excluded. Truncation reports
partial coverage with `pending_operations_truncated`; discovery never grants
permission to inspect a reference. Bootstrap and lookup recheck current access.

Named acceptance tests:

- `cargo test --locked --workspace unknown_effect_operation_is_recoverable_by_lookup`
- `cargo test --locked --workspace action_union_rejects_unknown_kind_with_envelope`
- `cargo test --locked --workspace bootstrap_lists_pending_operations`
- `cargo test --locked -p aq-server --test it_dashboard dashboard_task_knowledge::knowledge_correct_with_stale_version_conflicts`
- `cargo test --locked -p aq-server --test it_dashboard dashboard_task_knowledge::task_retry_creates_replacement_attempt`

- `cargo test --locked --workspace supervisor_creates_graph_with_dependencies_idempotently`
- `cargo test --locked -p aq-server --test it_dashboard dashboard_actions::session_message_send_enqueues_to_bound_session_mailbox`

## Task comment kinds

Task comments (aq-dashboard spec 0004 §4.1) are an aq-server model keyed by
`(project_id, task_id)`, not Graph content. The thread is read with
`GET /api/human/projects/{p}/tasks/{t}/comments` (human-graph-1). Owner
bootstrap advertises `tasks.comments.write@1`, which also requires
`dashboard.actions`.

- `task.comment.add`: subject `{type:"task",id:<task UUID>}` with **no**
  revision and empty `expected_versions`. Payload `{operation_id, project_id,
  body}`. The task must be a live Graph task in the project. The new comment's
  ID equals the operation ID.
- `task.comment.edit`: subject `{type:"task_comment",id:<comment UUID>,
  revision:R}`, `expected_versions {task_comment:R}`. Payload `{operation_id,
  project_id, expected_revision:R, body}`.
- `task.comment.delete`: same subject and versions. Payload `{operation_id,
  project_id, expected_revision:R}`. Delete is a tombstone: the body is cleared
  and the comment keeps its place in the thread.

`body` is Markdown: nonblank, at most 16384 UTF-8 bytes, no control characters
other than tab, line feed and carriage return (schema `maxLength` counts
characters and is an upper bound). Revisions are positive decimal strings.

- A confirmed operation's receipt is `[{type:"task_comment",id,revision}]`, the
  revision that operation produced; affected entities are that comment and
  `{type:"task",id}`.
- A stale `expected_revision` is `conflict` with `current_versions
  {task_comment:<live>}`; nothing is written.
- An owner may edit or delete only comments they wrote. Anything else is refused
  before admission with 403 `comment_not_author` ("Only the owner who wrote a
  comment can edit or delete it"). Editing or deleting an already deleted comment
  is refused with 409 `comment_deleted`, except that a replay of the operation
  that deleted it still returns its confirmed receipt. A comment outside the
  project is a neutral 404.
- Replaying an operation ID returns its retained disposition (journal), and the
  comment store keeps one record per operation so lookup can reconcile an
  interrupted dispatch. A different request under the same ID is 409.

## Worker control

Owner-only kinds on a service machine (spec 0007 phase 1). The subject is
`{type:"machine",id:<SwarmMachine.id>}` with **no** revision and empty
`expected_versions`; the compare-and-swap value travels in the payload. Owner
bootstrap advertises `swarm.worker.control@1` (these kinds) and
`swarm.worker.enroll@1` (adding a machine), which also require
`dashboard.actions`.

- `worker.start`, `worker.stop`, `worker.drain`: payload `{operation_id,
  machine, expected_generation}`. `machine` is a machine key (a service
  machine's `SwarmMachine.id`); `expected_generation` is the `control.generation`
  the page last read, a positive decimal string. `worker.stop` also requests a
  handoff of every attempt running on the machine. Each is `confirmed` with
  receipt `[{type:"machine_control",id:<operation id>,revision:<new generation>}]`
  and affected entity `{type:"machine",id,revision:<new generation>}`. A stale
  generation is `conflict` with `current_versions {machine:<live generation>}`;
  nothing is written.
- `worker.revoke`: payload `{operation_id, machine}`, where `machine` is a key or
  `node:<uuid>`. It revokes every node of the machine and is idempotent: revoking
  a machine whose nodes are all revoked is `confirmed` again. Its receipt is
  `[{type:"machine_revocation",id:<operation id>}]` with no revision.

An unknown machine key is a neutral 404. `reason` is optional for all four kinds.

## Owner availability

`swarm.availability.set` (spec 0007 Q2) is the owner's sticky availability
switch, owner-only. Owner bootstrap advertises `swarm.availability.control@1`,
enabled only when an intelligence registry is configured (otherwise the kind is
a neutral 409). The subject is `{type:"swarm",id:"owner-availability"}` with no
revision and empty `expected_versions`; the payload is `{operation_id,
available, expected_generation}`, where `expected_generation` is the
`server.owner_availability.generation` the page last read (a positive decimal
string; `"1"` while the switch is unset).

- `available: true` turns the switch on. It writes no observation: while the
  switch is on, selection treats every authorized credential as attested
  available by the owner (credential-wide: no model, no node), so availability
  stays on until the owner switches it off. A credential with unexpired
  negative feedback in the node's observations is not attested. An offer made
  under the switch is refused at accept or grant only if the switch changed
  (a new generation) since its selection.
- `available: false` turns it off: polls make no new offer and run no selection.
  Manual observations are still accepted, but the switch holds.

It is `confirmed` with receipt `[{type:"owner_availability",id:<operation
id>,revision:<new generation>}]` and affected entity
`{type:"swarm",id:"owner-availability",revision:<new generation>}`. A stale
generation is `conflict` with `current_versions {owner_availability:<live
generation>}`; nothing is written. `reason` is optional.
