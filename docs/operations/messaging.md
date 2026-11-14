# Durable task and session mail

2026-11-13: aligned with the rolling-records principle
([architecture 0006](../architecture/0006-records-are-rolling.md));
implementation pending.

AQ-11 provides scoped logical mail and outbound worker delivery records. It does
not type into terminals, send Discord notifications or assert that an agent
understood a message. Those mechanisms consume these durable identities later.

Owner requests use the normal admin identity and CSRF guard:

| Route | Request |
| --- | --- |
| `POST /api/messages/send` | `MessageSend` |
| `POST /api/messages/inbox` | `MessageInbox` |

Agent requests use a live per-attempt credential on the worker listener:

| Route | Required profile capability | Contract |
| --- | --- | --- |
| `POST /v1/agent/messages/send` | `message.send` | `MessageSend` → `MessageSendReceipt` |
| `POST /v1/agent/messages/inbox` | `message.read` | `MessageInbox` → `MessageInboxResponse` |
| `POST /v1/agent/messages/body` | `message.read` | `MessageBodyRequest` → `MessageBodyResponse` |
| `POST /v1/agent/messages/ack` | `message.ack` | `MessageAck` → `MessageAckReceipt` |

New node polls require the frozen profile's explicit `message.read` capability.
Acknowledging an already admitted delivery does not grant a new read; its exact
disposition may still be recorded without that capability while the attempt
authority remains current. Node requests use the enrolled node credential and exact node/boot plus
attempt/session/generation on the worker listener:

| Route | Contract |
| --- | --- |
| `POST /v2/worker/messages/poll` | `MessageNodePoll` → `MessageNodeBatch` |
| `POST /v2/worker/messages/ack` | `MessageNodeAck` → `MessageNodeAckReceipt` |

Public schemas, generated types and examples live in
[the agent-access contract](../../protocol/agent-access/v1/README.md) and
[messaging fixtures](../../protocol/agent-access/v1/fixtures/messaging.json).
These are additive v1 API shapes and capability identifiers. Existing fixture
profiles and their digests are unchanged. A profile without the requested
capability gets `unsupported_capability`; an older peer can refuse these new
capabilities/routes. The caller must negotiate support and cannot downgrade to
raw terminal input or borrow graph-read permission. `X-AQ-Agent-API-Version: 1`
is supported; another explicit version is refused.

A recipient is either a task mailbox with workspace/project/task UUIDs or an
immutable session mailbox with those UUIDs plus attempt/session/generation.
Initially, recipient task identities must match a trusted provisioned launch
binding. Arbitrary unknown tasks, cross-project claims and graph knowledge UUIDs
are refused. This supports admitted and dormant task identities; sending before
a first trusted binding awaits the AQ-19 project/execution binding integration.
AQ-17 will supply a central supervisor authority adapter. Owner routes are the
current central sender and do not impersonate a worker/session.

Agents may send only within their frozen workspace/project and readable task
scope, with explicit send permission. Inbox, body chunks and acknowledgements
are restricted to their own task or exact current session. The server derives
sender identity from the authenticated owner/session and rejects supplied sender
fields. Admission retains node/attempt/credential authority locks through commit;
cancellation, revocation, credential expiry and stale boot/generation refuse new
writes. Replaying an old ID requires current authority. Already accepted mail
remains durable after authority expires.

Persist the original `operation_id`, `message_id`, `thread_id`, recipient and body
before send. Canonical integer-only JSON and authenticated sender form the digest;
a changed operation payload or reused message identity conflicts. An identical
operation returns its immutable receipt. Each mailbox owns a serialized positive
cursor; rolled-back sends consume no cursor. A thread UUID filters the same
mailbox cursor namespace and grants no additional visibility. `after` is an
exclusive decimal cursor, suitable for a future wait-source reobservation seam;
AQ-11 does not register waits or wake sleeping agents.

Bodies are 1–16,384 UTF-8 bytes. Inbox pages contain at most 32 messages and obey
the agent's complete response budget. Pages trim to fit; an oversized single
message returns an explicit omitted-body descriptor and leaves `next_cursor`
unchanged. The descriptor names `message_body` recovery and includes the exact initial
`body_request`. Retrieve that immutable message with `/body`, following `next_offset`
until `complete`; offsets and chunks use UTF-8 boundaries and every chunk checks
current authority and scope. Advance to its message cursor deliberately afterward.
A descriptor is neither delivery nor acknowledgment. Owners can inspect the full
body with a one-message admin inbox page. Single chunks are at most 16,384 bytes.

Worker polling returns at most 16 envelopes (each bounded by the same body limit)
and a persisted delivery ID per message/attempt. Repeated polling may replay
`pending` or `received_by_node` envelopes; the worker must journal/deduplicate the
exact ID before any harness effect. Poll response creation is not proof of receipt.
A node acknowledgement records `received_by_node`, `handed_to_harness`,
`delivery_unknown` or `stale`. Terminal dispositions cannot be changed by ordinary
acknowledgements, and changed operation bytes conflict. Unknown, handed-off or unresolved prior-attempt
messages are excluded from automatic polling across attempts; uncertainty requires
explicit reconciliation rather than duplicate injection. Task mail with no prior
unresolved delivery/handoff/unknown/agent receipt can reach an eligible successor; old session mail
cannot. Node/harness delivery does not create an agent `received`/`read` receipt.
Those per-attempt receipts are separate, monotonic and cannot authorize task
completion or a human decision. Admin inbox shows latest receipt/delivery evidence;
agent inbox shows its own attempt's evidence.

State format 5 adds mailbox heads/messages, operation receipts, agent receipts
and node deliveries. Migration is additive and runs through normal Store startup;
older state formats remain readable by this binary. No production database was
used for qualification. Read and expired mail is deleted by DEP6 ([row retention](row-retention.md)),
following architecture 0006. Administrative reconciliation of unknown deliveries
requires explicit future policy; this version does not reset cursors, retry
unknown injection or send human notifications.

Node acknowledgements currently require live attempt authority. If revocation or
expiry wins before a handed-off acknowledgement, the persisted pending delivery
remains held and cannot be automatically injected on a successor. A future
known-intent recovery path must authenticate retained node identity and reconcile
that exact delivery without admitting a new message or restoring agent authority.
Inbox and body reads recheck credential expiry after database work and before
returning content; the credential expiry is bounded by the admitted grant.
