# dashboard-read-1 bounded human read integration

This subset adds authenticated bootstrap, overview, sessions/list/detail and
projects to the existing human listener. It reuses current human identity,
exact-session inspection ACL and common authority readers. It never forwards a
browser credential to a worker/provider or grants terminal/execution authority.
Projects retain existing Owner-only access. Status/workers keep their existing
independent routes and ACL. Tasks/search/decisions/provider quota/supervisor reads
are explicitly unavailable; no unknown aggregate is reported as zero.

The public machine-readable authority is protocol/dashboard-read/v1. Rust DTOs
produce its schema and synthetic response fixtures. New wire version is
`dashboard-read-1`. Bundle digest is nullable without verified loaded assets;
when available it is tagged SHA256 of exact loaded manifest bytes. Revisions
remain positive decimal strings. authorization_revision is a digest-derived
cache partition over current user role/binding and bounded active exact ACL
revisions/expiry, not an invented database or cross-service revision.

Router-local Extension owns a separate two-connection lazy read pool, bounded
request admission, ephemeral AEAD cursor key and <=64 membership snapshots,
<=4/principal, <=256 IDs each, TTL30s. Pages default25/max100. Cursor authenticates
principal/scope/auth partition/resource/snapshot/offset/expiry and grants no
access. Every returned session rechecks current common authority and exact ACL;
role/visibility drift refuses old pages. Each authorization transaction locks the
current user and ordered ACL rows for share, commits before any attempt read,
and rechecks expiry after awaited SQL. Each item releases its common node/attempt
locks before another item; a final authorization transaction fences response delivery. No migrations or AppState fields.
Snapshots freeze bounded membership; current row observations are independently
sampled/rechecked, not a fictional cross-owner atomic snapshot. Truncation and
unknown totals are explicit. Old/evicted cursors require restart.

Session identity includes exact node/boot/attempt/session/generation, decimal
record revisions when owned, safe sealed provider/model/harness labels and
inspection permission only. Missing task titles use their immutable ID as a
clearly documented display fallback. No raw state/spec, credential refs, paths,
provider keys or browser authority tokens are returned. A grant is not a process
observation. Missing qualified process/aggregate evidence remains Unknown and
coverage Partial. Existing revoked execution can be inspected historically under
its current human ACL; it supplies no control authority.

Tests must cover current owner/viewer ACL, revocation/expiry after waits, scope
mismatch, principal/filter cursor replay, tampering/expiry/bounds, secret canaries,
unknown versus zero and normal authenticated router behavior. The SQL route fixtures are explicitly synthetic and cannot masquerade as a
qualified native/provider run. Actual daemon/browser smoke is separate evidence.

Bounds limit returned members, memory, requests, SQL time and lock waits; LIMIT
is not a proof of constant database work as retained history grows. Membership
and ACL scans have not received throughput/capacity qualification. Process,
terminal-control, supervisor, task and accounting adapters remain separate work.
