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

Owners upload a file first, then reference it from a Create. All owner routes
below require the current Owner (as above) and the normal CSRF header on PUT
and DELETE. `{id}` is the conversation and `{attachment}` a client-minted
non-nil UUID; a nil or malformed UUID is 400 `bad_request` and an unknown
conversation is 404 `not_found`.

### Capability discovery

Discovery carries `attachments` (`$defs/AttachmentCapability`, example
`discovery.example.json`): the accepted `media_types` and the effective limits
the server enforces: `max_file_bytes` (default 1048576), `max_files_per_message`
(default 4, always equal to `limits.attachments`), `max_staged_files` (16) and
`max_staged_bytes` (16777216) per conversation, `staged_ttl_ms` (86400000) and
`max_name_bytes` (255). The first two are configurable by the server operator
(`[supervisor_attachments]`, see Release prerequisites); an operator can only
lower them, never above 1 MiB or 4 files, so a client must read them from
discovery on every session and never assume the defaults. The advertised values
are the very values the routes enforce: the PUT body limit is
`max_file_bytes` (413 `attachment_too_large` above it, the limit itself is
accepted) and a Create listing more than `max_files_per_message` refs is 400
`bad_request`. Lowering a limit never hides a file or message stored earlier.
Clients size and filter uploads from this object, not from hard-coded values.
When the object is absent or `limits.attachments` is 0, the server has no
owner attachments: hide the upload control (see Release prerequisites).

### Media types

`Content-Type` must be exactly one of these (no parameters; anything else is
415 `attachment_media_type_unsupported`), and the bytes must be a readable file
of that type (else 400 `attachment_unreadable`):

| Content-Type | Typical extension | Content check |
| --- | --- | --- |
| `text/plain` | `.txt` | UTF-8, no NUL |
| `text/markdown` | `.md`, `.markdown` | UTF-8, no NUL |
| `text/csv` | `.csv` | UTF-8, no NUL |
| `application/json` | `.json` | UTF-8 JSON that parses |
| `application/pdf` | `.pdf` | starts `%PDF-`, `%%EOF` in the last 1024 bytes |
| `application/vnd.openxmlformats-officedocument.wordprocessingml.document` | `.docx` | a ZIP whose central directory lists `[Content_Types].xml` and `word/document.xml` |
| `image/png`, `image/jpeg`, `image/gif`, `image/webp` | | format signature |

Legacy `.doc` (`application/msword`), HTML and other types are refused.
Browsers often report an empty type for `.md`; clients map the extension to
the table above before uploading. Content is not converted, rendered or
inflated by the server.

### Routes

| Method and path | Success | Refusals |
| --- | --- | --- |
| PUT `/api/human/conversations/{id}/attachments/{attachment}?name=<urlencoded>`, raw bytes as body | 201 `AttachmentUpload` (staged); 200 with the current state for an identical retry (same owner, conversation, name, type and bytes, also after it was attached) | 415 `attachment_media_type_unsupported`, 413 `attachment_too_large` (body above the advertised `max_file_bytes`), 400 `attachment_name_invalid`, 400 `attachment_unreadable` (empty or failing its content check), 409 `attachment_id_conflict` (the id holds another upload), 503 `attachment_capacity_exhausted` (staged quota) |
| GET `/api/human/conversations/{id}/attachments/{attachment}/metadata` | 200 `AttachmentUpload` | 404 `attachment_not_found` |
| GET `/api/human/conversations/{id}/attachments/{attachment}` | 200 the exact bytes | 404 `attachment_not_found` |
| DELETE `/api/human/conversations/{id}/attachments/{attachment}` | 204, empty body: the staged upload is gone | 404 `attachment_not_found`, 409 `attachment_bound` |

Refusals listed here have the body `$defs/AttachmentError`
(`{"error": code, "message": text}`, example `attachment-error.example.json`),
the same shape as other AQ errors. Authentication, CSRF and availability
failures keep their generic codes (401, 403, 503 `identity_unavailable`).

- `name` is one path segment: 1 to 255 UTF-8 bytes, no control characters,
  `/` or `\`, no leading or trailing whitespace, not `.` or `..`. A missing
  `name` is `attachment_name_invalid`.
- Read visibility (metadata and content): a staged file only for its uploader,
  in its conversation, until it expires; an attached file for any current
  owner, for as long as its message is retained (also across server
  restarts). Everything else, including another owner's staged file, another
  conversation's file and an expired or deleted file, is the same 404
  `attachment_not_found`, so ids do not leak.
- Content downloads are `Content-Disposition: attachment` (RFC 5987 file
  name) with the stored `Content-Type`, `X-Content-Type-Options: nosniff`, CSP
  `sandbox; default-src 'none'`, `Cache-Control: no-store` and
  `x-aq-content-digest` (the `digest`). Clients verify the digest.
- DELETE (unstage) removes only the caller's own unexpired staged upload. A
  bound file is never deleted by an owner route (409 `attachment_bound`); it
  is removed with its message. A retried DELETE after a lost 204 is 404; both
  mean the file is gone. The id may then be reused for a new upload.
- Staged uploads expire 24h after `created_at_ms` and are then purged. At most
  16 staged files (16 MiB) per conversation; more is 503
  `attachment_capacity_exhausted` until a staged file is attached, deleted or
  expires.

### Binding from a Create

- A Create lists `{id, digest}` for up to `max_files_per_message` (default 4) staged uploads of the same owner
  and conversation. If any ref is missing, expired, deleted, already attached,
  someone else's, or pinned to a different digest (a stale ref), the operation
  is a retained `not_queued` receipt with reason `attachment_unavailable` and
  nothing is bound. A queued message binds every file in order; bound files
  count against the 64 MiB retention reservation and live exactly as long as
  the owner message.
- The central supervisor downloads a bound file with
  GET `/v1/supervisor/conversation/attachments/{attachment}` and its
  `messaging`-scoped credential, only after the message was dispatched to it.
  Its turn text carries one line per file:
  `[attachment {id} name {JSON string} {media_type} {size} bytes {digest}]`.

### Release prerequisites

Servers that advertise `limits.attachments: 0` (and no `attachments` object)
run a release from before agile-vault-27 and have none of these routes. Before
a dashboard enables uploads against a server:

1. The server runs a release that contains migrations 76
   (`global_supervisor_attachments`) and 163 (DOCX). Both apply automatically at
   startup and need no owner configuration. Migration 163 only widens a CHECK
   constraint, so rows written by older releases stay valid; a server without
   163 refuses DOCX uploads and does not list DOCX in `media_types`.
2. Discovery returns the `attachments` object. Gate the upload control on
   that object and on `supervisor.message.enqueue@1`, never on the release
   number, and take types and limits from it.
3. Attachment bytes are stored in PostgreSQL, so database backups grow by at
   most 16 MiB of staged files per conversation plus bound files within the
   64 MiB retention reservation. No new secret or volume is needed.
   The only configuration is optional: the vault `config.toml` table
   `[supervisor_attachments]` with `max_file_bytes` (1 to 1048576, default
   1048576) and `max_files_per_message` (1 to 4, default 4). An omitted table
   keeps the defaults; a value outside the range, or an unknown key, makes the
   server refuse to start. A change takes effect at the next server restart
   and is then advertised by discovery. This is a deployment (owner) setting
   that this change does not edit; the defaults need no action.
4. Pin the schema digest below in the dashboard and re-check it on upgrade.

This document does not deploy anything; releasing follows the normal release
and owner review process.

## Supervisor History attachments (additive, for G8)

Published schema digest: `sha256:eda588e205779a11b74a319628f603a55e311358b3de345200d87fe6031f3ed3`
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

Project conversations use the same contract with a scope-specific immutable id and bounded label. A fresh enabled controller may report runtime `idle` / `central_service_ready` between physical turns; admission receipts keep their original unavailable snapshot.
