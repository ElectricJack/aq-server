# Scoped agent graph access v1

API-01 publishes this contract for AQ-06/AQ-07 and WRK-05. The daemon implements the scoped server routes; owning implementation evidence qualifies their behavior separately. JSON Schema 2020-12 (`schema.json`, identifier `urn:agent-q:agent-access:1`) is the public type boundary; `openapi.json` describes twelve machine HTTP routes, including the messaging extension below. `client_types.py` contains generated Python type declarations for developer/client tooling. The validator and fixture oracle are developer-only, with no production SDK, authentication implementation or runtime dependency implied. Rust and other clients can generate types from the public schemas.

| Method/path | Authority and purpose |
| --- | --- |
| GET `/v1/agent/context` | Active agent session: immutable resolved profile and launch context, current credential metadata. |
| POST `/v1/agent/graph/read` | Active agent session: one finite tagged `get`, `revision`, `history`, `search`, `context` or `relationships` query. |
| POST `/v1/agent/graph/commands` | Active agent session: one bounded typed graph command batch admitted under current authority. |
| GET `/v1/agent/operations/{operation_id}` | Agent's own existing operation disposition; no new action. Expired/revoked credentials use node/operator recovery. |
| POST `/v2/worker/agent-operations/query` | Enrolled node: bounded lookup/list of its own existing attempt/session/generation operations; no graph query, command, arbitrary payload or new intent. |
| POST `/v2/worker/agent-operations/retry` | Enrolled node: reconcile exactly one associated retained intent under its original immutable payload/key and budget. |

The node recovery routes are execution-protocol-2 extensions carrying this independently versioned agent-access-1 body. It does not reinterpret foundation connectivity protocol 1. AQ-06/AQ-07 must negotiate support before worker/client use. Requests/answers identify agent API version 1; responses carry `X-AQ-Agent-API-Version: 1` and `Cache-Control: no-store`. An unsupported explicit version is refused with HTTP 426. Strict bodies reject unknown fields and commands. Standard schema validation describes shape; `x-utf8-maxBytes`, `x-int64-token`, digest equality, cursor/receipt bindings and authorization rules are additional normative semantic checks. Independently generated clients must implement those checks and run the published vectors; JSON Schema alone is not authorization. There is no forward-any-route, URL, SQL, shell, aq-git or graph-admin operation. The twelve playbook Host commands are separate and are not agent graph commands.

## Authority, profile and context

`Binding` freezes task, attempt, session, generation, enrolled node/boot, project/workspace and resolved-spec/profile/context digests. These are equality assertions against server-owned credential/attempt records, never caller-granted authority. AQ must authenticate the credential class before admission and check current generation, grant, cancellation, expiry and revocation. An owner, worker/node or aq-graph service credential cannot substitute for a per-attempt agent credential. Model instructions, provenance and possession of a readable token file cannot widen permissions.

The resolved profile is canonical immutable JSON with role, model/configuration identity, instruction/tool-policy digests, explicit read/write scopes, capabilities, response limits and retry budget. The context digest covers the exact launch snapshot, revision labels, observation time and coverage. SHA-256 uses sorted JSON keys, preserved array order, UTF-8, no whitespace, no fractional/nonfinite numbers and no duplicate keys. Credential version/expiry can rotate separately; profile and launch-context digests cannot silently change during an attempt. Changes requiring different authority use an explicitly authorized new attempt/session. Existing execution fixture specifications do not yet provision these richer bindings; AQ-06 and WRK-05 must add and verify that linkage.

Scopes are a workspace-wide or explicit-record read set, allowed relationship types, explicit writable record IDs/field allowlists and bounded creation/proposal grants. Creation-derived access is associated centrally with the admitting attempt and cannot be asserted by a client; it does not mutate the launch profile/context. The fixture oracle accepts an already resolved synthetic principal/profile; it does not verify real tokens, clocks, policy storage or author delegations.

Every structured output must satisfy the authorized projection: roots, search results, history, relationship endpoints and pinned revisions. `ScopedNode` omits raw provenance, arbitrary metadata, creator fields and successor references; `HistoryItem` omits arbitrary graph `data`. Search returns authorized record projections rather than unfiltered snippets. Both relationship endpoints must be readable; every requested/pinned revision remains in scope. Missing and inaccessible records use the same generic not-found result. Do not disclose hidden IDs, titles, snippets, counts, cursor contents or graph errors. Validate upstream workspace bindings before dropping that field during projection.

Filter/authorize before paging. An opaque cursor binds node/session/generation, profile and current authority revision, exact filters/direction and paging budget. Reject changed/tampered cursors; never use an unbound offset. `coverage.complete_for` is only `authorized_projection`; an empty partial page is not global absence. Scan/node/edge/byte limits yield explicit partial coverage without unauthorized omission counts. Unavailable historical links remain `relationships_available:false, links:null`, without current-link substitution. `observed_at` labels observations; version/revision tokens are decimal strings preserving positive signed-64-bit values for generated clients, not timestamps or commit order.

## Finite commands and conditional delivery

| Command | Admission and qualified graph mapping |
| --- | --- |
| `create_finding` | Explicit new UUID and bounded knowledge content. AQ supplies task/attempt/session provenance; caller cannot supply actor, verification or credentials. Maps to knowledge `create_node`. |
| `correct_record` | Writable/readable record, expected version, only `title`, `body`, `labels`. Task content additionally needs `write.task_content` and central impact policy. Graph author/protection rules remain mandatory. |
| `add_relationship` | Informational edge, writable source, readable target, permitted type and exact optional revision pins. Source expected version is mandatory. |
| `remove_relationship` | Server resolves stored edge and verifies both endpoints against bindings and scope; edge/source expected versions are mandatory. Caller endpoint claims are not authority. Live structural removal is excluded. |
| `propose_decomposition` | One exclusive unapplied draft, bounded new draft tasks and structural links. Every existing endpoint is readable/writable and has a version guard. Cannot mix with live commands or apply a draft. |

Complete/cancel/reopen, approval/verification, retirement, publication, assignment control, arbitrary metadata and permission changes are absent. They require separate AQ policy services. All command/endpoint checks precede whole-batch admission; a failing second command cannot apply the first. Order and explicit IDs are frozen. Structural links occur only in proposals; informational links cannot bypass node-kind, cycle, author or protected-record rules. A draft receipt confirms proposal retention, never live application or task completion.

Graph v1 lacks an AddEdge source-CAS parameter. The representative mapping adds a no-op `update_node` source-version guard in the **same graph transaction**, before add/remove. Other existing proposal endpoints receive equivalent guards. Qualified `store.rs` loads for update, checks expected version and `authorize_edit` before no-op detection; a no-op does not advance version/history. `knowledge_rules.rs` refuses protected/retired nodes and another author's knowledge for an ordinary writer. AddEdge uses the same source authorization. The selected-image probe verified success/replay, stale-guard atomic refusal and foreign-author/retired-source HTTP 403 without changing principal.

AQ-07 must reject a command if its guard is unavailable under the selected ordinary service principal, even if narrower edge permission might otherwise be allowed. Never acquire an admin identity, fall back to an unguarded write or pre-read/new-version overwrite. A future graph compare primitive needs separate qualification. Expanded batches remain within graph's 100-operation cap. Public receipts omit no-op guard results. Explicit command-result versions describe returned results, not the final current version of every affected record; `affected_record_ids` identifies records to observe deliberately. Do not infer unchanged source versions after edge checkpoints.

## Receipts, uncertainty and recovery

An operation ID binds the entire canonical `CommandRequest`: immutable bindings, reason and ordered commands. Persist intent, digest, profile/policy references, fixed service-principal/workspace/privilege mapping, graph operation class and derived key **before dispatch**. Fixture derivation is `aq-agent-v1:<operation UUID>`, scoped upstream by the recorded actor, workspace and mutate/draft-create class. Caller-supplied keys/principals are impossible. Preserve that identity through credential rotation and response loss. Current graph pilot receipts are indefinite; future retention uncertainty or unavailable old principal must preserve uncertainty rather than generate another key.

| State | Meaning and client action |
| --- | --- |
| `pending` | Durable admission; not dispatched or possibly in flight. Poll existing ID. HTTP 202 is not success. |
| `confirmed` | Exact graph receipt retained and confirmed-event outbox identity committed locally. Return scope-filtered receipt/version references. HTTP 200; `task_completion:false`. |
| `conflict` | Terminal conditional-write/identity conflict, no application by this intent. HTTP 409. Review inputs, never automatically fetch a version and overwrite. `current` can be null: graph errors do not provide a current-version value. |
| `rejected` | Recorded definite refusal with proof of no application, such as an undispatched queued operation closed by policy or permanent graph rejection. HTTP 422. Pre-admission/authentication errors instead use `ErrorResponse`. |
| `unknown` | Effect may have occurred. Admission may itself be unknown after transport loss. Keep original ID/bytes; query, then replay only the same admitted request under permitted recovery rules. |

No operation row yields generic `operation_unknown`, not proof of no graph effect. `UnknownOperation.admission:unknown` can represent a CLI transport result, not a fictitious durable receipt. Lookup never creates intent. Already admitted exact delivery may reconcile under recorded authority after session expiry/cancellation; this neither renews agent credentials nor authorizes new mutation. Node recovery exposes only own persisted attempt operations and receipt references, without original graph bodies or unrelated reads. Unknown/unassociated IDs are refused identically.

Retry counters, jittered delays and elapsed budget are bounded by the immutable profile and must be persisted by the eventual client. Bound requests by remaining authority/budget; exhaustion requires manual reconciliation, not a fresh ID. Cancellation cannot prove a potentially dispatched operation unsent. Commit graph receipt and deduplicated confirmed-event outbox fact in one local transaction; pending/unknown/conflict cannot emit a confirmed completion event. External effects, source retention and graph task completion remain separate facts.

`cli-contract.json` reserves commands, private token-file input, JSON output and exit codes. It requires journaling original bytes/ID before sending, explicit unknown outcomes and bounded retries. API-01 ships no `aq-agent` executable or production journal/cache. WRK-05 context caching must be private, bounded, read-only and scoped; it never supplies cached authority or offline committed edits.

## Reproduce and limits

Use Python 3.11+ in a disposable developer environment:

```sh
python3 -m venv /tmp/aq-agent-access-venv
/tmp/aq-agent-access-venv/bin/pip install -r protocol/agent-access/v1/requirements-dev.txt
/tmp/aq-agent-access-venv/bin/python protocol/agent-access/v1/generate.py --check
/tmp/aq-agent-access-venv/bin/python -m unittest discover -s protocol/agent-access/v1 -p test_conformance.py -v
```

If the host lacks `ensurepip`, create the environment with `python3 -m venv --without-pip /tmp/aq-agent-access-venv` and use an installed base pip with `python3 -m pip --python /tmp/aq-agent-access-venv/bin/python install -r protocol/agent-access/v1/requirements-dev.txt`. The retained fresh-environment run used this isolated fallback.

`validate.py TYPE FILE` (or `-` for stdin) checks public types, UTF-8/integer/digest/budget invariants and prints a digest without echoing invalid private input. `build_fixtures.py` deterministically derives examples from retained synthetic graph responses. Ordinary checks need no sibling checkout, daemon, graph, worker, Docker, database or credentials. Developer dependencies are pinned; date-time validation needs no unpinned optional format package.

Optional `capture_graph_fixture.py` reproduces the narrow probe with already-installed qualified images. It refuses a mismatched graph image ID, creates unique disposable Docker resources with a dynamic loopback HTTP port and synthetic credentials, uses only public graph APIs, and removes owned resources. It neither pulls/publishes nor contacts production resources. Re-capture changes fixture timestamps/receipt UUIDs and must be reviewed before regeneration.

Tests establish schemas, client decisions, scope/receipt fixture invariants and actual guard behavior on graph revision `225329cd6acf5a775fd2a0b016dd1c45fb16c79b`, image `sha256:807beba98dcd20b0ae335f09cae8c06ecb182a352ee78de278cb47fccb2ee3be`. In-memory fixture ledger/service are not durable AQ authority or a graph engine. API-01 fixture evidence alone does not establish authentication, author delegation, SQL/outbox atomicity, exhaustive graph invariants or real scoped traversal/search/history/cursors. AQ-06/AQ-07 runtime evidence qualifies the daemon boundaries separately; production CLI persistence remains WRK-05 work. No production authorization, exactly-once guarantee, task completion, scale or deployment gate is accepted here.

## Additive AQ-11 messaging extension

`message.send`, `message.read` and `message.ack` are explicit, independent profile
capabilities. Older profiles retain their original bytes and digests and gain no
permission. New request/response schemas and machine routes are included in the
same v1 generated contract; examples are in `fixtures/messaging.json`. An older
peer may reject the new capability or route. Refusal never permits raw-keystroke
fallback or borrowing a graph capability. The server checks current authority at
each admission, including body chunks and node delivery acknowledgements.

`SupervisorMailbox` (`global_supervisor` plus the current `conversation_id`) and
`SupervisorMessageSender` (`central_supervisor`) are additive strict branches of
`MailboxRecipient` and `MessageSender`; existing documents remain valid. An agent
session credential may send to the current supervisor conversation only, with
`thread_id` equal to that conversation. Agent and owner inbox, ack and body reads
of the supervisor mailbox are refused; owners use the conversation API.

See [messaging operations](../../../docs/operations/messaging.md) for owner routes,
limits, cursor semantics, immutable task/session identity, separate agent versus
node/harness receipts, and explicit oversized-body recovery. Worker polling is
outbound and returns durable per-attempt IDs. `delivery_unknown` is terminal for
ordinary delivery and does not trigger automatic injection retry, including on a
successor. These interfaces supply persistence and authentication; AQ-11 does not
implement provider message delivery or human notifications.


## AQ-07 durable mutation implementation

The daemon now implements finite mutation admission, status lookup and scoped
node/owner recovery; see [graph mutation operations](../../../docs/operations/graph-mutations.md)
for exact routes, configuration, quotas, dispatch/cancellation semantics and
qualification limits. `mutation_actor_id` must name the actual ordinary service
actor. The new `RecoveryRetry` route permits one existing associated intent and
never accepts replacement graph bytes or renews an agent credential.

Guard author/protection observations now require the caller's original CAS token
to equal the fetched public version before admission. Future tokens are refused;
concurrent changes remain fenced by the same-batch graph CAS. The bounded fixture
oracle mirrors this rule. `write_scope.relationship_types` accepts structural
EdgeType values as an additive extension; drafts require explicit structural
write scope plus read edge scope, while ordinary relationship commands remain
informational-only. Original profile/base and wire-message fixture bytes remain
unchanged. The legacy informational-only proposal scope vector now expects a
refusal; new `fixtures/mutation-delivery.json` explicitly covers both profiles.
There is no agent draft-apply or task-completion route.
