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
