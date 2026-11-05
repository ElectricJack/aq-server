# human-supervisor-conversation-1

Current authenticated Owner only; session inspection ACLs do not grant global
conversation access. Protected admin routes retain normal CSRF rules.
Capabilities: `supervisor.history.read@1`, `supervisor.message.enqueue@1`. The
`supervisor.conversation` is true only while an active central session serves
the conversation. Discovery and History `runtime` (`$defs/Runtime`) is
`unavailable` with null binding until then, and `available` with
`binding: {actor_id, generation, session_id}` and reason
`central_session_active` while that session is active. Receipts keep the
admission-time unavailable runtime and never change after they are written.

- GET `/api/human/conversations/global`: Discovery, stable conversation UUID and
  definition revision across restarts. It creates no message or session.
- GET `/api/human/conversations/{id}/messages`: History. Default newest 16,
  chronological output. Optional `limit` 1..16; `before` positive decimal cursor
  for older records OR `after` nonnegative decimal cursor for newer records.
  Both are exclusive. `next_cursor` is the next before/after value when has_more;
  `through_cursor` records the observed latest admitted cursor. Maximum 256 KiB
  encoded response; a page may be shorter to fit. No reply is invented.
  Owner items carry cursor `N` (their mailbox cursor). A supervisor reply carries
  its own cursor `A.R`: `A` is the max cursor of the batch it answers and `R`
  (positive) its reply-mailbox cursor. Items are ordered by `(N, 0)` / `(A, R)`,
  so each reply follows the owner message it answers. `limit`, `has_more` and
  `next_cursor` count replies like any other item, and `before`/`after` accept
  either cursor form. `through_cursor` stays an owner-mailbox cursor.
- POST the messages path: closed Create, all fields required. Body nonblank,
  at most 16384 UTF-8 bytes, no NUL. Whole literal JSON at most 131072 bytes.
  `context` holds at most 16 pinned refs (`$defs/ContextRef`:
  `{type, id, revision}`), unique by `(type, id)`, with non-nil ids;
  `attachments` holds at most 4 staged uploads (`$defs/AttachmentRef`:
  `{id, digest}`), unique by id (see Owner attachments below). Owner History
  items echo non-empty context as `context` and bound files as `attachments`
  (`$defs/AttachmentDescriptor`); each field is absent when empty. The
  supervisor turn text lists each ref, then each attachment, after the body. Positive revision is a
  signed 64-bit decimal string. Operation and message UUIDs are client minted,
  stable, nonnil and different; preserve them with the original request.
- GET/PUT `/api/human/conversations/{id}/read-marker`: the caller's read cursor
  (`$defs/ReadMarker`). PUT takes `{"cursor": "N" | "A.R"}`
  (`$defs/ReadMarkerUpdate`). The marker only moves forward: an earlier cursor
  is a no-op and a cursor past `latest_cursor` is 400. `unread.count` counts
  items after the marker that the caller did not author, capped at 100 with
  `capped: true`. Markers are per principal, so owners do not share them.
- GET `/api/human/conversations/{id}/operations/{op}`: pure original-actor receipt
  lookup, 200 Receipt or 404. A 404 is not proof an earlier request has finished.

POST 200 means queued in the durable AQ mailbox, never delivered or reasoned.
POST 409 with a typed not_queued Receipt is a retained revision, message-ID or
`attachment_unavailable` refusal.
Its original operation cannot later enqueue. Generic admission/auth/error bodies
are not receipts; preserve ambiguous requests and look up their operation.
An identical request returns its immutable receipt; changed bytes that change the
canonical request conflict. Reformatting equivalent JSON does not change identity.

`request_digest` is sha256:lowercase hexadecimal over canonical JSON of the closed
Create object alone: recursively sorted object keys, compact UTF-8 JSON, integer
values only. It is not a hash of literal transport bytes. `body_digest` hashes the
literal decoded text UTF-8. The owning mailbox additionally binds authenticated
sender and exact conversation into its private operation digest. UI validates
conversation/operation/message/actor/digests before accepting a receipt. A queued
receipt's cursor points to the actual history message; never infer from body text.

History authors are server-derived human UUIDs. Every message has the stable
conversation UUID as thread_id. Delivery remains not_attempted and agent receipt
null; no runtime/worker fallback, external notification or provider call occurs.
Retention is bounded to 4096 operations and 64 MiB reserved bytes; capacity refuses
new operations while history, lookup and exact replay remain available.

## Owner attachments (agile-vault-27)

Owners upload a file first, then reference it from a Create.

- PUT `/api/human/conversations/{id}/attachments/{attachment}?name=<urlencoded>`
  with the raw bytes as the body. `attachment` is a client-minted non-nil UUID.
  `Content-Type` must be exactly one of `text/plain`, `text/markdown`,
  `text/csv`, `application/json`, `application/pdf`, `image/png`,
  `image/jpeg`, `image/gif` or `image/webp` (no parameters). Bodies are 1 to
  1048576 bytes (413 above). Text types must be UTF-8 without NUL, JSON must
  parse, and binary types must start with their format's signature. `name` is
  one path segment of at most 255 UTF-8 bytes. 201 returns a staged
  `$defs/AttachmentUpload`; an identical retry (same owner, conversation,
  name, type and bytes) returns 200 with the current state; any other reuse
  of the id is 409.
- Staged uploads expire 24h after `created_at_ms` and are then purged. At most
  16 staged files (16 MiB) per conversation; more is refused as maintenance
  until a staged file is attached or expires.
- A Create lists `{id, digest}` for up to 4 staged uploads of the same owner
  and conversation. If any ref is missing, expired, already attached, someone
  else's, or pinned to a different digest, the operation is a retained
  `not_queued` receipt with reason `attachment_unavailable` and nothing is
  bound. A queued message binds every file in order; bound files count
  against the 64 MiB retention reservation and live exactly as long as the
  owner message.
- GET the same path downloads the bytes (staged: uploader only; attached: any
  current owner) as `Content-Disposition: attachment` with `nosniff`, a
  sandbox CSP and `x-aq-content-digest`.
- The central supervisor downloads a bound file with
  GET `/v1/supervisor/conversation/attachments/{attachment}` and its
  `messaging`-scoped credential, only after the message was dispatched to it.
  Its turn text carries one line per file:
  `[attachment {id} name {JSON string} {media_type} {size} bytes {digest}]`.

## Supervisor History attachments (additive, for G8)

Published schema digest: `sha256:2056d6fe1cdff14eca53784a93981c8a943fcdbc3033c7dee8cb80d79220ef3b`
(also in `schema.sha256`; `supervisor_turns` fails if the two diverge). Pin it.

A supervisor `SupervisorItem` may carry `attachments` (0–4 items, schema
`$defs/SupervisorAttachment`). Clients must tolerate its absence; pre-attachment
items are unchanged. Every item carries the `message_id` of the reply that
produced it. Items are snapshots taken when the reply is recorded and never
change, so they remain readable after the operation is retired.

- `proposed_task_draft`: `draft_id`, `project_id`, `revision` (the payload's
  expected project version), `payload_digest` (`sha256:` over canonical JSON of
  `payload`, the same canonical form as `request_digest`) and `payload`, a
  dashboard-actions v1 `task.graph.create` payload in normalized form (omitted
  defaults are materialized; the digest covers the served payload). It is a proposal only:
  nothing is created until an Owner or supervisor submits it as an action.
- `action_receipt`: `operation_id`, `kind` (`task.create` or
  `task.graph.create`), `project_id`, `project_revision` (nullable),
  `created_task_ids`, `created_edge_ids`, `receipt_refs` (for example the Graph
  change set) and the operation's `request_digest`.

The supervisor adds them on `POST /v1/supervisor/conversation/replies` with
`attachments: [{type:"proposed_task_draft",draft_id,payload}|{type:"action_receipt",operation_id}]`
(`$defs/ReplyAttachmentRequest`, example `reply-attachments.example.json`). The
server derives every other field. Limits: at most 4 attachments, 64 KiB
serialized, unique non-nil draft/operation IDs, and drafts must pass
`task.graph.create` validation (400 otherwise). A receipt may name only a
confirmed, unretired creation operation journaled by the replying supervisor
session; anything else is 409 and nothing is recorded. Replies without
attachments keep their previous replay digest. History cursoring and page byte
accounting are unchanged and include attachments.
`history-attachments.example.json` is a complete page.
