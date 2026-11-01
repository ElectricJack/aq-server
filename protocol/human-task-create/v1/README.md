# Human task creation, version 1

Capability `tasks.create@1`, contract `human-task-create-1`. All routes require the
current human Owner. Operations belong to their original AQ user UUID; another
Owner receives 404. Graph service credentials remain server-side. No agent token,
execution attempt, worker assignment or launch is manufactured.

- POST `/api/human/projects/{project_id}/tasks`: closed `Create` object from
  schema.json, JSON body at most 65536 bytes. Title at most 512 Unicode scalars,
  nonblank and no controls; body 32768 UTF-8 bytes; acceptance 8192 UTF-8 bytes;
  body and acceptance cannot contain NUL;
  unique labels at most 16, each nonblank/no controls/at most 128 UTF-8 bytes.
  Expected project revision is a positive signed 64-bit decimal string.
- GET `/api/human/operations/{operation_id}`: pure retained status lookup. It
  never calls Graph or initiates dispatch. 404 is not proof an earlier POST has
  finished; preserve ambiguous submissions.
- POST `/api/human/operations/{operation_id}/recover`: exact closed empty object
  `{}`. Explicitly replays immutable retained Graph bytes using the same
  idempotency key, if current authority/binding/service and recovery budget match.

The caller freezes one operation UUID and literal UTF-8 request body. Task ID is
that same UUID. `request_digest` is `sha256:` plus lowercase hexadecimal SHA256
of those literal bytes, not normalized JSON. Retrying the original POST uses
identical bytes. Same operation with different bytes or project is 409. Duplicate
JSON keys, unknown fields and invalid bounded values are refused before admission.

POST/create and recover return typed status bodies: 200 confirmed, 202 pending or
unknown, 409 conflict, 422 rejected. GET returns 200 for any retained state. Generic
AQ admission/auth/transport errors are not receipts. Only a validated confirmed
receipt proves creation. Then read the exact task in its project; never infer
creation from title matches or optimistic state.

A stale project revision is durably retained as conflict/not_dispatched and
`project_binding_changed_before_dispatch`; it is safe to prepare a new operation
from a fresh project read. A first-dispatch validated Graph conflict/rejection is
not_applied. Any transport, timeout, malformed success or ambiguous prior attempt
remains unknown/possible. A later conflict cannot erase possible prior effects.

Immutable intent commits before network IO: exact caller bytes/digest, fixed
Graph payload/digest, actor, project snapshot/revision, original workspace and
credential/service fingerprint. Dispatch claims a six-second lease in a short
transaction with current Owner and project row locks; HTTP has a three-second
budget and no transaction crosses the network. Recovery is limited to eight
attempts and five minutes; changed project binding, Graph origin/actor/credential,
clock regression and exhausted budget block replay without declaring no effect.
Terminal receipts remain historical evidence for the original workspace even
if current binding changes. Authorization is checked again before returning.

Migration 0034 follows 0033 without editing prior migrations. Upgrade while the
previous daemon is stopped, then start a state 34-aware binary. Older binaries
must refuse newer state; rollback requires restoring the pre-upgrade database
backup with its corresponding binary. Do not delete unknown intents or attempt
an in-place downgrade. The append-only identity journal admits at most 4096
operations and 64 MiB reserved retained bytes globally; exhaustion refuses new
admission while lookup/recovery remain available. Retention/GC is future work.
Graph remains the sole task-record authority; this journal stores operation
intent and proof only. Draft/active labels confer no execution authority.
