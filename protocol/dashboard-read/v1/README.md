# dashboard-read-1

This is the implemented read-only subset: authenticated bootstrap, overview,
grouped search, provider-account quota, session list/detail and owner-only
configured projects. Capability versions are
`1`; the envelope and contract profile version are `dashboard-read-1`.

Generate `schema.json` with `cargo run -p aq-server --example dashboard_contract
--offline`; add `-- --check` for drift verification. Fixtures are sanitized,
synthetic owned SQL projection responses, never evidence of a native process.

Session identities are historical immutable bindings. Overview counts come from
bounded owner observations; missing owners produce unknown counts and explicit
partial coverage. Session activity remains unknown until its owning projection
is connected. Authorization
revision is a decimal cache partition derived from current role and a bounded
visibility set, not a database revision. A changed partition refuses cursor
reuse. Snapshot membership expires after 30 seconds and never grants inspection:
each page reloads current rows and ACLs, then checks the current partition again.
The sessions `total` counts the snapshot's authorized members: `exact` when every
member projected, `estimated` when some were skipped as no longer visible, and
`unknown` when membership was truncated. Each summary carries optional
`last_output_at`, the last terminal output the relay recorded. Output is not
progress: repaints and heartbeats also advance it, and last-progress time is not
projected yet. Session detail `permissions` lists `session.inspect` and, for an
owner of a session with an unrevoked launch binding, `session.message.send` (see
`dashboard-actions-1`). At most 256 members, 100 items per
page, 64 cached snapshots, four per principal and four concurrent reads are
admitted. Each SQL statement and lock wait is bounded; common authority locks
are released per item. Responses are bounded to 1 MiB. The separate `dashboard-actions-1` contract
mounts admission and lookup, with no production kinds enabled. The disabled
`dashboard.actions` capability declares operation retention metadata.

All-authorized scope omits `scope`; project scope uses `scope=project:{uuid}`.
Opaque cursors are process-local authenticated ciphertext. A restart, expiry,
changed scope/limit or invalid token returns 410 `cursor_restart_required` and
requires a fresh list; authorization changes
return 403. No source paths, credentials, tokens or launch manifests are projected.

`python3 protocol/dashboard-read/v1/check.py` validates the five prior read
fixtures plus G4 aggregate/search/quota cases using the development `jsonschema`
package. The route integration test
can deliberately replace fixtures with `AQ_DASHBOARD_WRITE_FIXTURES=1`; normal
checks never rewrite them. Fixtures contain random synthetic UUIDs from their
retained test execution. They are shape examples, not stable resource handles.

Session rows optionally include `lifecycle: {started_at, stopped_at}` from the
immutable authenticated worker receipt pair. Both times are nullable. The lookup
is one indexed join inside the same exact-session inspection transaction; only
recorded timestamps leave the daemon, never proof bodies or machine credentials.
This historical evidence does not change `observation.state`, process freshness,
terminal capability or task completion. `execution_authority` is the separate
central state. Older responses may omit lifecycle; an object with both times null
means this implemented projection has received no worker reports. Native facts
queued only on worker disk appear only after their owning handlers admit them.

The lifecycle columns reuse the existing authorized offer lookup; they add no
query roundtrip. The existing bounded list still performs per-session authority
and ACL reads. A warmed one-session fixture sampled five full human detail reads
at 11.50–13.17 ms (median 12.50 ms). Its receipt/offer join used the three primary-key
indexes, one row, and 0.167 ms SQL execution. These local shared-host samples are
not a fleet-scale benchmark or a 500-agent operating envelope.

Session detail optionally includes `observation_binding`, the exact lifecycle
binding from the already verified and authorized retained profiled offer. Its
project, task, lineage, attempt, attempt generation, and full session identity
pin the independently fetched observation usage view. Protocol binding generation
fields remain JSON integers; the existing summary session identity retains its
decimal strings. Legacy/missing/null binding means usage cannot be pinned. No
extra query, usage-derived identity, active-process fact or execution permission
is introduced. Session list rows do not gain this field.

`session.usage.read@1` advertises the actual mounted human observation usage API
when current session visibility can be represented within the dashboard bound.
Each GET `/api/observations/sessions/{attempt_id}/usage` still enforces current
Owner or exact, unexpired Viewer inspection ACL. The unchanged version 1 body
and strict schema live in `crates/aq-observation-api/artifacts/usage.schema.json`.
Revision 0 and null last-recorded time mean no projection; counter unknown values
must not become zero. Known category subtotals may overlap and remain partial
when counters are unknown. Costs are unknown/not_reported; session usage does
not establish provider account quota, invoice cost, or fleet-wide totals.

G4 examples include `provider-usage-{empty,nonempty,partial,stale,unknown}.json`,
`overview-{empty,nonempty,partial,unknown}.json` and
`search-{empty,nonempty,partial,unknown}.json`. The latter synthetic variants
illustrate source coverage and null/zero distinctions, not separately captured
responses. Expired overview observations and search cursors are refused, so no
stale aggregate/search success fixture is asserted. The retained search cursor is
an opaque shape example, not reusable across processes; replay and restart are
verified by the owning integration test. Prior five read fixtures remain checked.

`capacity.read@1` advertises owner-only GET `/api/v1/capacity` (`capacity.json`).
`slots` totals and idle count online workers only and are `unknown`, with partial
coverage, when an online worker advertises no slot total (host-native) or the
inventory exceeds 256 workers; `occupied` is the exact count of task-holding
attempts. Each node's `eligibility.reasons` are the worker state, capability
report freshness and the placement owner's own ceiling refusal
(`node_concurrency_ceiling_reached`, `node_spend_ceiling_reached`,
`node_spend_unaccounted`, `node_disk_pressure`). Provider availability comes from
unexpired node credential bindings for the account's profiles: `unknown` with
`no_fresh_node_report` when no live node reports one. Nothing here grants placement.
`/api/workers` rows carry the latest storage report flattened (`disk_*_bytes`,
`preserved_*`, `storage_used_bps`, `storage_observed_at`), absent when unreported.

`questions.read@1` advertises the owner-only F5 question inbox (`question-inbox-1`,
also covered by this schema): GET `/api/questions/decisions?state=open|closed&limit=1..50[&project_id=uuid][&cursor=opaque]`
and GET `/api/questions/decisions/{decision_id}`. `state=open` lists pending
questions oldest asked first; `state=closed` lists answered ones most recently
answered first. The cursor is the shared opaque format, bound to the owner's
current identity, state, project and limit for five minutes; any change returns
410 `cursor_restart_required`. Items carry the decision ID and the decimal
`decision_revision` the answer must target, project/workspace/task IDs, the
graph `task_title` (null when the graph is unavailable to the read), the exact
question, `asked_at_ms` and `expires_at_ms`. `answer`, `answered_at_ms` and
`delivery` are null while pending and all set once answered; `delivery` is
`{kind: wait}` when the answer woke the waiting attempt, or
`{kind: task_mail, message_id}` when it was queued for the continuation.
`questions.answer@1` is enabled with `dashboard.actions` and names the
dashboard-actions-1 kind `question.answer`. Fixtures:
`question-inbox-{open,closed}.json` and `question-detail.json`.
