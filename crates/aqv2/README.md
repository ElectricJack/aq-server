# aqv2

Human terminal access to the same AQ domain operations as the dashboard. Build
with `cargo build --locked -p aqv2 --release`. The executable is always `aqv2`;
v1 continues to own `aq` and `agent-queue`.

Supply `--origin https://aq.example` and
`--cf-access-token-file /absolute/private/access.jwt`. The token file must be a
regular owner-only file owned by the current user, with no links. Authentication,
origin checks, CSRF headers, byte budgets, timeouts and redirect/proxy/retry
refusals come from `aq-human-http`. The server decides authorization.

Examples below omit those two global flags:

```text
aqv2 tasks list
aqv2 tasks list --project PROJECT_UUID --limit 25 --cursor CURSOR
aqv2 tasks show TASK_UUID --project PROJECT_UUID
aqv2 tasks create --project PROJECT_UUID --expected-project-revision 1 --title "Write launch notes" --body "Document accepted behavior" --acceptance-criteria "Notes reviewed" --label docs --task-state draft
aqv2 decisions list --project PROJECT_UUID --state open
aqv2 decisions show DECISION_UUID
aqv2 decisions respond DECISION_UUID --request answer.json
aqv2 messages inbox --after 0 --limit 16
aqv2 messages send "Please inspect the pending decision."
aqv2 chat
aqv2 chat --once "Please summarize the queue" --timeout 120
aqv2 workers list
aqv2 projects list --limit 25
aqv2 projects show PROJECT_UUID
aqv2 operations show OPERATION_UUID
```

`tasks list` without a project reads the bounded task overview; `--limit` sets
its per-project task limit (1–32), and `--cursor` supplies its `next_after`
project UUID. Project task lists use the owner's ordinary `limit` and opaque
`cursor`. Pagination and coverage metadata are preserved in output; lists do
not automatically fetch more pages.

`answer.json` is the `decision.resolve` payload published in
`protocol/dashboard-actions/v1/fixtures/request-decision-resolve.json` (the
`payload` object). It includes `schema_version: 1`, `decision_id`, a numeric
`expected_decision_revision`, `choice` (`approve` or `reject`),
`comment`, and the immutable `expected_subject` artifact (`id`, `version`,
`digest`) from inspection. The CLI replaces any `operation_id` with a newly
minted UUID, binds it to both the envelope and payload, and preserves the
expected subject. Payload files must be regular, nonsymlink files of at most
64 KiB. The server validates domain fields and expected revisions.

Each mutation makes exactly one `POST /api/v1/actions`. A fresh client-minted
operation UUID appears both outside and inside the domain payload; messages
also get a distinct fresh message UUID. A confirmed supervisor send means
mailbox admission only. Pending, unknown, conflict, rejection, HTTP failure,
invalid response and transport loss return a nonzero status and the original
operation ID. Inspect that ID with `operations show`; this only calls
`GET /api/v1/operations/{id}`. An absent operation does not prove non-admission.
The CLI never resends a mutation or switches to a legacy write route.

The current action union offers task creation, decision response and supervisor
message enqueue. `tasks cancel`, `tasks retry`, `workers revoke ID`,
`workers enroll`, and `projects onboard` return `unsupported` without making
HTTP requests. Adding their domain implementations belongs to their owning
tasks; this CLI does not route around their absence.

The published `human-supervisor-conversation-1` foundation has human-only
history and explicitly unavailable runtime. Consequently `chat --once` currently
returns `unsupported` after discovery, before sending, even with `--timeout`.
Live send/wait/reply and timeout behavior require F1b's published reply and
correlation contract. `chat`, `messages inbox`, and `messages send` work with the
published foundation. No live reply or timeout qualification is claimed.

`--json` is accepted before or after command groups. Success is
`{"schema_version":1,"data":<server response>}`; failure is
`{"schema_version":1,"error":{"code":"...","message":"...",...}}`.
Mutation failures also carry `operation_id` and a read-only recovery command.
Unconfirmed operation errors include the returned operation under `operation`.
JSON errors go to stdout and exit nonzero; human errors go to stderr. Raw
HTTP error bodies and credential contents are never printed. Human read output
is formatted JSON, which escapes terminal control characters.
