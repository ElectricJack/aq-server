# dashboard-read-1

This is the implemented read-only subset: authenticated bootstrap, overview,
session list/detail and owner-only configured projects. Capability versions are
`1`; the envelope and contract profile version are `dashboard-read-1`.

Generate `schema.json` with `cargo run -p aq-server --example dashboard_contract
--offline`; add `-- --check` for drift verification. Fixtures are sanitized,
synthetic owned SQL projection responses, never evidence of a native process.

Session identities are historical immutable bindings. Activity and aggregate
counts remain unknown until their owning projection is connected. Authorization
revision is a decimal cache partition derived from current role and a bounded
visibility set, not a database revision. A changed partition refuses cursor
reuse. Snapshot membership expires after 30 seconds and never grants inspection:
each page reloads current rows and ACLs, then checks the current partition again.
Sessions totals remain unknown across pages. At most 256 members, 100 items per
page, 64 cached snapshots, four per principal and four concurrent reads are
admitted. Each SQL statement and lock wait is bounded; common authority locks
are released per item. Responses are bounded to 1 MiB; no action routes exist.

All-authorized scope omits `scope`; project scope uses `scope=project:{uuid}`.
Opaque cursors are process-local authenticated ciphertext. A restart, expiry,
changed scope/limit or invalid token requires a fresh list; authorization changes
return 403. No source paths, credentials, tokens or launch manifests are projected.

`python3 protocol/dashboard-read/v1/check.py` validates the five retained route
fixtures using the development `jsonschema` package. The route integration test
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
