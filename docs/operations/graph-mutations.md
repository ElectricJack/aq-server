# Durable agent graph mutation delivery

2026-11-13: aligned with the rolling-records principle
([architecture 0006](../architecture/0006-records-are-rolling.md));
implementation pending. The intent journal and receipt paragraphs changed.

AQ-07 admits finite scoped graph operations into AQ PostgreSQL before any graph
write. It uses aq-graph's public API and ordinary workspace service credential;
workers receive neither that credential nor graph database access.

Configure the existing service binding with an explicit mutation actor:

```toml
[agent_access]
graph_url = "http://graph:8741"
graph_token_file = "/run/secrets/graph-ordinary-writer"
mutation_actor_id = "aq-service-writer"
credential_ttl_ms = 120000
```

The private token file retains AQ-06's ownership, size and content checks. The
operator must provision an ordinary graph writer whose actual actor matches
`mutation_actor_id`; there is no admin fallback, discovery or privilege upgrade.
Without the actor setting, mutation admission refuses `unsupported_capability`.
This version supports one configured service binding, scoped by the upstream
credential's workspace. AQ records origin, actor, credential verifier, workspace,
operation class and the selected receipt-retention contract (aq-graph keeps
receipts for a window, below) per intent. No secret token is stored in an operation or returned to an agent.
Changing origin/actor/credential mapping holds existing uncertain intent for
manual reconciliation. Retain the original protected service binding through
recovery; never recover under a fresh actor/key just because a credential rotated.

| Route | Authority and behavior |
| --- | --- |
| `POST /v1/agent/graph/commands` | Current agent credential; API-01 `CommandRequest`; durable admission followed by bounded dispatch |
| `GET /v1/agent/operations/{operation_id}` | Current agent credential; its own attempt's bounded `OperationStatus` |
| `POST /v2/worker/agent-operations/query` | Current enrolled-node credential; lookup/list existing operations associated with that exact attempt/session/generation, even after agent expiry |
| `POST /v2/worker/agent-operations/retry` | Same node recovery authority; `RecoveryRetry` names exactly one existing operation and no replacement graph payload |
| `GET /api/agent-operations/{operation_id}` | Installation owner; retained disposition |
| `POST /api/agent-operations/{operation_id}/retry` | Owner; retry that exact retained intent within its original budget/mapping |
| `POST /api/agent-operations/{operation_id}/cancel` | Owner; reject only an intent proven never dispatched |

Admin routes use the existing owner/CSRF path; machine routes are absent from the
admin listener. Enrolled node, owner and graph credentials do not substitute for
an agent credential on ordinary mutation routes. No generic graph route, caller
service actor, caller idempotency key, draft-apply or task-completion operation is
exposed. Central task completion remains AQ-10 policy.

The fixed command mapping is:

| Command | Required permission and graph effect |
| --- | --- |
| `create_finding` | `write.finding`; create attributed unverified knowledge in the frozen workspace |
| `correct_record` | `write.correct`, authorized record/patch fields, unverified/unretired state and knowledge authorship; tasks additionally require `write.task_content` |
| `add_relationship`, `remove_relationship` | `write.relationships`; informational types only; writable protected source, readable target, exact source-version guard in the same atomic graph batch |
| `propose_decomposition` | `write.proposal`; writable task parent and existing endpoints with exact guards; persist a draft containing draft tasks, never apply it |

Every proposal edge requires both `write_scope.relationship_types` and
`read_scope.edge_types`. The write allowlist now accepts structural types as an
additive schema extension, but ordinary relationship commands remain
informational-only. Existing informational-only profiles gain no structural
permission and their original profile bytes/digests remain unchanged. A new
resolved profile can explicitly grant `blocks`/`parent-child` for proposals.

Scope checks precede bounded public guard reads. Fetched knowledge authorship,
verification and retirement facts authorize only the exact fetched version.
Correction/source/parent/guard CAS tokens must equal that version before
admission; caller-supplied future versions cannot bless a later protected state.
The original tokens are retained unchanged. Concurrent changes after those reads
are fenced by aq-graph's atomic expected-version checks, including no-op source
updates in the same batch as edge changes. Targets of ordinary informational
relationships have read scope, not a target-content write grant or implicit target
CAS; explicit revision pins and upstream availability checks still apply.

AQ reauthenticates and locks node → attempt → launch authority before committing
intent. Expired, cancelled, revoked or stale sessions cannot admit new writes.
All commands are validated before any effect; mixed live writes and proposals are
refused. Public request limits are 16 commands, at most 100 translated operations
and 64 receipt subjects, with the existing 1 MiB input/response ceilings. Complete
status/receipt envelopes must fit the resolved profile budget before admission.
Created-record reservations are cumulative per attempt. Only confirmed findings
receive the profile's optional `include_created_by_attempt` read grant; a draft
or unknown receipt cannot create that grant.

Each attempt keeps its unresolved operation intents, and its settled intents as
a rolling set: the most recent 4096 and 32 MiB of conservative logical
reservation **(proposed default: the former cap)**. The reservation counts
canonical request, mapped request and policy bytes plus 32 KiB for bounded
status/outbox growth. PostgreSQL/index physical overhead belongs to installation
storage policy. The count/byte aggregate is indexed and serialized by the
existing attempt authority lock. New admission deletes the attempt's oldest
settled intents when the set is full; it returns `overloaded` only when
unresolved intents alone fill the reservation, which is work happening now.
Uncertain intent is never deleted while it is uncertain. Rejected intent is
settled and rolls with the rest.

An operation ID binds the complete canonical integer-only request and original
binding. Changed bytes conflict; identical replay reuses the immutable mapped
payload and `aq-agent-v1:<operation UUID>` key. The graph operation class is fixed
to `/mutations` or `/changesets`, scoped by service actor/workspace. The selected
graph contract keeps receipts for a window (7 days, aq-graph spec 0001; pending)
and checks identical-key replay before reapplying CAS guards. Qualification
verifies a lost receipt after a real commit can replay successfully despite the
now-stale original CAS, without a second history entry, inside that window. AQ
retries an uncertain intent with its original key only inside the window. After
it, the graph would accept the old key as new, so AQ resolves the intent by
reading current graph state instead of replaying. Draft responses can materialize omitted optional fields
as declared defaults. The gateway validates the ordered plan after removing only
those no-effect defaults; the original stored request remains unchanged. A
[captured native draft fixture](../../protocol/agent-access/v1/fixtures/native-draft-normalization.json)
checks this selected public behavior and rejects added non-default meaning.

`Prefer: respond-async` on command admission persists the intent and returns its
current disposition without starting dispatch. A later identical command without
that preference, or an authorized node/owner retry, drives the same intent. Merely
GETting an operation never dispatches it. This supplies a reachable durable queued
state and an explicit unsent cancellation boundary without holding a request or
provider process alive.

Dispatch records a lease and increments the immutable policy's attempt/elapsed
budget before HTTP, then releases every database transaction and authority lock.
Only one lease drives an intent at a time; a crashed lease expires after 15 seconds
and recovery may replay identical bytes. HTTP timeout, malformed receipt or storage
failure after reservation preserves possible effect/unknown status. Rare post-admission size failures retain the existing operation ID and report
unknown/recovery, never `not_applied`. Only a valid,
scoped graph receipt can commit `confirmed` together with one stable outbox event.
Version conflicts never trigger a read-new-version/overwrite loop. Definite initial
permanent refusal is `rejected`; ambiguity cannot become a false no-effect claim
merely because a later credential fails. Exhausted budgets or changed service
mapping require manual reconciliation, without a new key or unbounded retry.

Owner cancellation succeeds only while the recorded count is zero and phase is
queued. Once dispatch may have occurred, it returns the existing possible-effect
state. Execution/session cancellation does not erase an already dispatched intent;
recovery of the same recorded effect remains independent of fresh agent mutation
authority. A queued initial dispatch rechecks live authority before it can send.
A revoked enrolled node cannot use recovery; the installation owner can inspect
and reconcile under the recorded service binding.

Node recovery returns statuses and receipts, never original graph bodies or service
credentials. Lists contain at most 64 operations. Opaque five-minute `aqc_` cursors bind
node, complete attempt identity, limit and monotonic operation sequence. Their
membership upper bound is captured while holding the same node fence used by
admission; later operations, including lower UUIDs, belong to a new listing. Status
values remain current per page rather than historical snapshots. Cursor creation
is serialized per node and capped at 128 live cursors per node/attempt; exact-ID
lookup remains available at the cursor ceiling. Missing/foreign operations have
the same nondisclosing refusal.

State format 7 adds intent, confirmed-event outbox, confirmed created-record grants
and recovery cursors. There is no automatic deletion of uncertainty, background
provider process, graph replica, direct task lifecycle mutation, publication or
human notification. The outbox is durable producer evidence; downstream playbook
consumption and final task policy are separate AQ-15/AQ-10 work.

Run the ordinary tests with a disposable `DATABASE_URL` from `scripts/test-db`:
`cargo test -p aq-server --test graph_mutations`. The explicitly ignored real-graph
test runs only through `python3 scripts/test-agent-mutations.py`, which checks the
pinned graph image identity, creates isolated synthetic Docker resources and uses
an ordinary writer. It makes no image pulls, fixture rewrites, production database
changes or provider calls.
