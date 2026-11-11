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
partial coverage. `needs_you` counts the returned attention items (decisions,
failed results, offline workers, uncertain operations); it is unknown while the
decision owner is unavailable and estimated while any source is partial. The
decision count alone is the `reviews` source total. In the all-projects view
every enrolled offline worker is attention; one without recorded work in a
listed project has `project: null`. Session activity remains unknown until its owning projection
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

`swarm.read@1` advertises owner-only GET `/api/v1/swarm` (`swarm.json`); `limit`
and `cursor` are refused. The read groups worker nodes into machines. Nodes that
report the same `machine.key` (worker-machine/v1) form one machine; a node with
no report or no key is its own machine with id `node:<node uuid>`. A machine is
`cloud` when its report says so, or when it has no report and its backend is
`boat`; otherwise `local`. Revoked nodes are left out. A machine is `offline`
when none of its nodes is online, `stopped` when every online node reports
`stopped`, `holding` when any online node holds, and `online` otherwise. A
machine's `load` and `hardware` come from the newest report of its online nodes
and are absent when it is offline or has no report; `load.fresh` means the
report was received within 60 s, and the 15-minute figures are means of the
samples received in the last 15 minutes, and are `null` when no sample
fell in that window (`samples_15m` is 0). Samples are stored at most once per
5 s per node. `agent_max` is the sum of the node slot
totals, and absent when any total is unknown.

Phase 1 service machines: a node enrolled with `aq-worker service` holds a server-side machine key (claimed by its slot 1). That key groups the machine's nodes ahead of any reported key, and a reported key that a service machine holds no longer groups other nodes. A service machine carries `control` (desired state, compare-and-swap generation, its time, pending handoffs). `control` is `null` for legacy and `node:` machines, which the worker actions refuse. Each service node's `service.state` is its machine's desired state.

Owner availability (spec 0007 Q2): `server.owner_availability` is the owner's sticky availability switch: `state` (`unset`, `on` or `off`), its decimal compare-and-swap `generation` for `swarm.availability.set`, and `since` (when it last changed; `null` while unset, at generation `"1"`). `on` makes selection treat the owner as available (no observation row is needed) until the owner switches it off; `off` stops new offers. `unset` leaves manual observations as they were. A switch set by a user who is no longer an owner reads `unset`, with `note` saying so, at its real `generation`, so an owner can switch it again in one step; `note` is absent otherwise.

A node's `holding` lists why placement would refuse it now: capability
unreported or expired, node spend ceiling, unaccounted spend, disk pressure, a
missing or expired context measurement for a natively registered node, and
`worker_stopped` or `worker_draining` from the worker service state. Offline and
revoked workers show as state, and a full concurrency ceiling shows as no free
slots, so neither appears as a hold.
`history_rate_limited` and `history_cap_reached` (spec 0008) say that the offer rate
(30 an hour per node, 240 per project, 960 in all), the inventory report rate, or a
live sealed-history cap holds new offers. On a node they apply to that node; in
`waiting.reasons`, a message starting `Project <id>:` applies to one project and one
starting `Server:` to all.
`tasks_held` (R6, `waiting.reasons` only) says a project has tasks that a server-side hold
keeps from being offered: an exhausted retry, time or cost budget, a lost attempt whose
retry was not admitted, a quarantine, or a successor refused on every poll for 10 minutes.
Its message is `Project <id>: <n> task(s) held (<reason code> <count>, ...)`. It counts
exactly the tasks whose work view shows a `task_held` reason (human-graph-1): both read the
same current holds. A hold clears when the task gets a new attempt, its claim is released,
or AQ observes it closed or retired in Graph (its work view, or a sweep every 10 minutes).
Each hold is named in one system comment.
A node's `slots.total` is absent when the
worker has not reported one; the read never turns that into zero free slots.
`cpu_above_threshold`, `memory_above_threshold` and `agent_maximum_reached` are
reserved for placement and are not produced yet. Each `agents` entry is a running
session of the node; its optional `routing_profile` identifies the authoritative
pinned intelligence profile (`id` and `display_name`). Historical attempts without
that identity omit it. Its `intelligence_class` is a legacy palette hint derived from the model name
(`haiku`, `mini`, `spark` or `flash` fast, `sonnet` standard, `opus` deep) and is
absent otherwise. `waiting.reasons` says why new work cannot start now:
`no_machine_online` when no machine is online, holding or stopped,
`no_eligible_machine` (naming each machine's first reason) when no online node
is free of holds, and `no_free_slot` when every eligible node has no idle slot.
`waiting.agents` is empty until placement reports per-agent waits. Coverage is
partial when the capacity read is partial or more than 64 agents run. Nothing
here grants placement.

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
