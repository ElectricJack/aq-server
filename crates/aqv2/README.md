# aqv2

The v2 command line: the owner's terminal and, inside a sandbox, the agent's
channel to AQ (spec [0006](../../docs/specs/0006-agent-channel.md) §4). Build
with `cargo build --locked -p aqv2 --release`.

## Names

One binary, installed under two names (owner decision 5):

| Where | Installed as | Why |
| --- | --- | --- |
| Sandboxes | `/opt/aq/bin/aq` | The agent's CLI. |
| Hosts | `aqv2` | v1 still owns `aq` and `agent-queue` on hosts. |

The binary names itself after the name it was run as: run as `aq`, its help,
usage and recovery hints say `aq`; under any other name they say `aqv2`.
Nothing else changes. Releases publish a static build,
`aqv2-x86_64-linux` (listed in `SHA256SUMS`), which runs in any image.

## Modes

The mode is chosen at startup:

- **Owner mode:** `--origin https://aq.example` and
  `--cf-access-token-file /absolute/private/access.jwt`, on the admin listener.
- **Channel mode:** `AQ_CHANNEL=unix:/aq/channel/aq.sock`, inside a sandbox.
  The CLI speaks HTTP/1.1 over that Unix socket and sends no credential: the
  channel proxy behind the socket injects the attempt's credential. With
  `AQ_CHANNEL` set, `--cf-access-token-file` and `--origin` are refused. A
  missing socket is `channel_closed` (exit 13) before anything is sent.

Both modes share the `aq-human-http` rules: bounded bodies, no redirects, no
environment proxies, no automatic resend of a mutation, and never echoing raw
error bodies or credential material. A channel request also sends no `Origin`.

## Commands

Every registered command (`protocol/commands/v1/commands.json`) runs through
the generic command surface, `POST /api/v1/commands/{name}`:

```text
aq commands [--all]                  # what you may run (with each grant's scope); --all adds the rest and why
aq whoami                            # attempt, task, project, session (and generation), supervisor conversation, grants, credential expiry
aq run NAME [--param KEY=VALUE | --param KEY:=JSON]... [--params-file FILE|-]
       [--subject TYPE:ID[:REVISION]] [--expect TYPE=REVISION]... [--reason TEXT]
       [--operation-id UUID] [--describe]
aq <cli tokens> [run flags]          # e.g. aq task graph show --param task_id=ID
```

- `run` sends `params`; a mutation gets a fresh `operation_id`, which is also
  put into `params` when the command's params carry one. `--json` is the
  output switch everywhere, so a params file is `--params-file` (spec §4.4
  wrote `--json FILE`).
- A command's CLI tokens (its `cli` field) also reach it. Tokens this binary
  does not know are resolved through `GET /api/v1/commands?all=true`, and
  `run` sends any name, so a command added to the server later needs no CLI
  change.
- `--describe` prints the built-in description (grant, target rule, params
  schema per principal) without a connection.

Typed subcommands are thin wrappers that build the same request. Params follow
the route that serves the caller: the agent's route in channel mode, the
owner's REST read or action-union payload in owner mode.

```text
aq task show [self|TASK_ID] [--project P]
aq task children [--param task_id=ID] | aq task history [--param task_id=ID]
aq task create --title T [--body B|-] [--acceptance-criteria A] [--label L]... [--parent self|TASK_ID]
               [--draft] [--priority N] [--task-type K] [--project P --expected-project-revision R]
aq knowledge search TEXT [--kind task|knowledge] [--limit N] [--cursor C]
aq knowledge show ID [--project P]
aq knowledge author --title T [--body B|-] [--label L]... [--content-type note|fact|…] [--reason R]
aq comment add BODY|- [--task T --project P]
aq comment list [--task T --project P] [--limit N] [--cursor C]
aq comment edit COMMENT --body B --revision R [--project P]
aq comment delete COMMENT --revision R [--project P]
aq question ask TEXT|-
aq question show ID
aq question wait ID [--timeout 300] [--interval 5]
aq wait register|inspect|ack --request FILE|-
aq wait list | aq wait show ID
aq message send BODY|- [--to task|task:ID|session|supervisor|supervisor:CONVERSATION | --to-json JSON] [--thread ID]
aq message inbox [--to …] [--after 0] [--limit 32] [--thread ID]
aq message body MESSAGE [--to …] [--offset 0] [--max-bytes 16384]
aq message ack MESSAGE [--to …] [--read]
aq operation show ID
```

- In a sandbox `self` is the attempt's task, `--parent self` makes a subtask,
  a comment goes on the attempt's own task, `comment list` lists the
  attempt's own task unless `--task` names another, `comment edit` and
  `comment delete` change only the agent's own comments, and `wait list` /
  `wait show` show the attempt's own waits. `task children` and
  `task history` (generic tokens) read the attempt's task unless
  `--param task_id=ID` names another. Owner mode needs the IDs the owner
  routes take (`--project`, `--task`, `--expected-project-revision`).
- `knowledge author` is a `create_finding` graph command for an agent and a
  `knowledge.author` action for the owner.
- `question wait` re-reads `question.show` until the state is `answered`,
  within `--timeout` seconds (at most 3600). Each read is its own bounded
  request. If the question is still open it exits 16 and the question stays
  open.
- Mailboxes in a sandbox, all from `whoami`: `--to task` is the attempt's own
  task mailbox, `--to session` its own session mailbox (`session_generation`)
  and `--to supervisor` the current supervisor conversation
  (`supervisor_conversation_id`). `supervisor:CONVERSATION_ID` names a
  conversation explicitly. A server whose `whoami` lacks those fields, or has
  no open supervisor conversation, is refused before sending (exit 2); any
  other mailbox takes `--to-json`.

### Result claim (worker-local)

```text
aq result submit --request-file "$AQ_AGENT_RESULT_DIR/submissions/request.json"
aq result status
```

`result` is not a server command and opens no connection (spec 0006 §9
step 4). It replaces `aq-agent result` with the same contract,
`aq-agent-result-local-1` (vendored from aq-worker as
`vendor/aq-agent-result-local-0.1.0`), and the same files and output:

- The outbox is the attempt's private directory named by
  `AQ_AGENT_RESULT_DIR`, installed by the worker (0700, owned by the agent's
  uid, `binding.json`, `presence.json` and `owner.lock` from the worker, only
  `submissions/` writable). An absent variable or an unsafe directory is
  refused; nothing falls back to another path.
- The request is `{"schema_version":1,"conclusion":"…"}` (one nonblank line,
  at most 512 bytes) in a regular, unlinked file of at most 4 KiB. The CLI adds
  the worker's binding; the request cannot name operations, stops or receipts.
- `submit` publishes one original claim (`submissions/submission.json`, create
  only, never replaced) and waits at most two seconds, capped by the original
  execution deadline, for the worker's acknowledgment. The same claim again is
  a replay; a different one is a conflict and the original is kept.
- Output is the local result document on stdout (`--json` wraps it as
  `{"schema_version":1,"data":…}`), with `aq-agent result`'s exit statuses:
  0 ready or recorded, 10 pending (inspect `phase`), 11 conflict, 13 needs
  reconciliation or outcome unknown, 15 unusable channel or request (a fixed
  `invalid_contract_or_private_state` document on stderr, never a path or a
  raw error).
- `tests/golden/result.json` pins every step. aq-worker checks `aq-agent
  result` against the same file, so the bundle's `aq-agent` wrapper
  (`exec aq "$@"`) keeps `aq-agent result submit|status` working unchanged.

The owner subcommands below (`tasks`, `decisions`, `messages`, `chat`,
`workers`, `projects`) call owner routes on the admin listener and are refused
in channel mode; `operations show` there is `operation show`.

## Output and exit status

`--json` prints one document: `{"schema_version":1,"data":…}` or
`{"schema_version":1,"error":{…}}`, on stdout. Human output prints reads as
formatted JSON, `commands` and `whoami` as text, and errors on stderr; server
text is escaped so it cannot drive the terminal.

A refusal (§6.8) keeps its published fields (`code`, `command`, `grant`,
`held`, `message`, `effect`, `request_id`). Human output prints the message,
the grant the command needs and what is held. A refusal is `not_applied`,
so it has no recovery command.

| Exit | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Any other error |
| 2 | Invalid arguments |
| 10 | Mutation pending or unknown: inspect the printed operation ID |
| 11 | Mutation conflict |
| 12 | Mutation rejected |
| 13 | Transport failure or closed channel (a mutation's outcome may be unknown) |
| 14 | Refused: `grant_missing`, `scope_denied`, `owner_only`, `quota_exceeded`, `session_revoked`, `session_expired`, `channel_refused`, `not_available`, `unauthorized` (kept from the aq-agent contract) |
| 15 | Invalid server response |
| 16 | A bounded wait ended before its condition |

## Sandbox template

What the sandbox runtime bundle ships (the aq-worker template owns the files):

- the release asset `aqv2-x86_64-linux`, pinned by its SHA-256 from the
  release's `SHA256SUMS`, installed as `/opt/aq/bin/aq` (mode 0755, owned by
  root). B7's "never ship `aq`" check is inverted: the bundle must contain
  `/opt/aq/bin/aq`.
- `manifest.json` lists it as an executable entry (`name: "aq"`, its
  version, `path: "/opt/aq/bin/aq"`, `sha256`, `bytes`).
- `aq-agent` stays for one template generation as a wrapper that execs `aq`
  (spec §9 step 3); `aq-agent result` is now `aq result submit|status`
  (above), which the wrapper reaches unchanged.
- No credential and no token file: the launch writes only
  `AQ_CHANNEL=unix:/aq/channel/aq.sock` into `/aq/in/env`, and
  `/opt/aq/bin` is on the agent's `PATH`. The runner note tells the agent
  that `aq` is available and that `aq commands` lists what it may run.
- The bundle checks: `aq --version` runs, and `aq --help` says `Usage: aq`.

## Owner mode

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

The owner groups send task creation, task cancel and retry, decision response,
worker revocation and supervisor message enqueue through the action union.

- `tasks cancel ID --project P [--revision R]` and `tasks retry ID --project P
  [--revision R]` send `task.cancel` and `task.retry`. `R` is the task version
  the change expects. Without it the CLI first reads the task
  (`GET /api/human/projects/{P}/tasks/{ID}`) and sends the version it saw; the
  server compares it again, so a task that changed in between is a conflict.
- `workers revoke MACHINE` sends `worker.revoke`. `MACHINE` is a machine key
  (the `id` of a machine in `run dashboard.swarm`; `workers list` shows node
  IDs, not keys), or the node ID of a legacy node that belongs to no service
  machine (sent as `node:<uuid>`). The server refuses a node of a service
  machine and asks for the machine.

`workers enroll` and `projects onboard` return `unsupported` without making
HTTP requests: no action kind serves them, and this CLI does not route around
their absence.

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
