# Current catalog inspection: bounded component-owned metadata

New source after `46db40d`: runner/PostgreSQL/CLI **0.1.3**, with
model/compiler remaining 0.1.0. Source qualification is recorded in the adjacent
evidence directory. This does not change a consumer pin or claim remote package
publication. Existing 0.1.2 archives are immutable.

## Read boundary

Add a separate versioned `CatalogQuery`/`CatalogPage` inspection profile rather
than overloading run IDs in `InspectionQuery`. Query definitions or activation
bindings, with either one exact key or a keyset page of at most 25 entries.
Responses are at most 256 KiB; byte-limited pages may return fewer entries than
requested while retaining a cursor for the last returned key. Keys and namespace
identities use explicit bounds. Decimal revisions/incarnations remain lossless.

Definitions represent artifacts currently retained in the namespace's
authoritative `State.artifacts`, not every artifact in the global catalog and
not a history of namespace installation timestamps. Expose only artifact and
executable/source fingerprints, name, scope, compiler, bounded literal purpose,
entry names and declared dependency references/selection modes. Purpose
truncation is explicit and UTF-8 aligned. Do not return captured definition
values, registry/input/output schemas, expressions, variables, source maps,
credentials or full artifact bodies. Declared dependency references are distinct
from the transitive dependency pins captured for a particular run.

Activations represent `State.bindings`: exact binding, artifact, revision and
stored controls. They are not inferred readiness, current grant validity,
worker eligibility, an activation time or proof of a human decision. Existing
commands, events, activation/admission semantics and run pins do not change.

`Storage::inspect_catalog(namespace, owner, query)` defaults to explicit
unavailable support. It must never fall back to `load()` or a full snapshot.
The new `CatalogError` is separate from the existing storage/runner error and
request/response enums; exhaustive matches on those existing enums remain valid.
Both existing actual backends implement it: MemoryStorage and PostgreSQL. There
is no SQLite backend in this repository. Existing third-party wrappers remain
explicitly unavailable until they delegate/implement the new bounded method.

## Freshness and pagination

A read requires the currently held namespace owner and lease. PostgreSQL checks
its connection-local lease, live session advisory lock, durable owner,
incarnation and backend PID under the namespace row's shared lock. No first-page
success is returned for absent ownership, lost connection, stale incarnation or
unmaterialized/mismatched projection.

Each CAS updates the catalog's **projected state revision** to the same namespace
revision as its authoritative snapshot. Reads require equality. CAS also publishes
expected definition/activation counts; INSERT/DELETE triggers separately maintain
actual counts. Reads and the next CAS compare these fixed headers without COUNT
or a row/snapshot scan. A missing row therefore refuses rather than becoming an
empty/partial success, and an unchanged CAS refuses rather than repairing it.
Row/count rollback is atomic; TRUNCATE and direct actual-counter writes are
refused. Expected counts cannot change while retaining the same State head.
The final State publication requires actual counts to equal the new expectation.
This catches missing-row faults with the qualified triggers enabled, not hostile
schema-owner disabling of guards or replay of arbitrary previously valid encrypted
rows. It is not a cryptographic integrity claim against a malicious DBA. A separate
**metadata revision** changes only when visible catalog metadata changes or the
projection is first materialized. This avoids invalidating catalog pagination
on every unrelated run transition.

Pages return the fresh state revision and a catalog stamp bound to namespace,
owner incarnation and metadata revision. Continuations require their original
stamp. A new owner/restart or any visible catalog change refuses the old stamp;
unrelated run progress does not. Reads are atomic per page under the row lock;
there is no lease renewal or hidden current-state cache. An unknown mutation
outcome never becomes an operation receipt merely because current metadata was
subsequently inspected.

## PostgreSQL representation and protection

Migration 4 adds component-owned catalog rows and namespace projection/revision
markers. Native keyset indexes bound requested metadata reads. Projection rows
are derived from State and updated/deleted inside the existing owner-validated
State CAS transaction. The database refuses row-identity changes (namespace,
kind or address), including moving a foreign namespace row into an owned one.
The first owned CAS backfills a namespace once; read
paths never backfill or scan/decrypt the snapshot. Retention/GC removes metadata
only when its authoritative artifact/binding is removed in that same CAS.

Protected artifact purpose itself is covered by existing confidentiality tests.
Therefore protected catalog rows require per-row authenticated encryption, not
a new plaintext public projection. AAD binds namespace, kind, key, row revision
and key ID. Reads decrypt only the selected bounded metadata rows with the
configured owner's keyring. The envelope must match namespace protection mode.
Typed allowlists, identity/context checks and byte bounds apply after opening;
authentication, missing keys and malformed rows refuse rather than returning
partial success. Key rotation re-encrypts rows in the same CAS without pretending
that unchanged metadata changed semantically. Unprotected namespaces may store
the same bounded typed projection as plaintext. Envelopes use byte columns, so
valid literal control characters do not create a PostgreSQL text encoding hole.
An activation item address is a domain-separated hash of its binding; protected
binding names do not become plaintext index keys. This address is not a secret
or an authorization token.

Existing transaction rollback keeps snapshot and projection invisible together.
Unknown commit/connection outcomes return errors; no process-local record is
promoted to current truth. A stale projected revision or incomplete envelope is
unavailable/corrupt, never a successful empty catalog. An ambiguous CAS commit
latches current inspection unavailable in that owning session until a later
verified successful CAS; this is a refusal marker, not cached current metadata.

A State accepted by the previous API can exceed this new profile
(e.g. an oversized binding name or total serialized metadata row). Such a CAS
continues to commit State atomically with `catalog_supported=false`; catalog
queries explicitly refuse rather than changing established mutation semantics
or returning a partial inventory. A later supported State can materialize again.

Migration 4 also fences both fresh constructors and already-open 0.1.2 writers.
Use the existing writer-format mechanism with a new qualified format marker;
do not edit migrations 1–3 or frozen archives. Old constructors refuse the newer
migration. Old in-process CAS must not overwrite current State while leaving its
new catalog projection stale. Upgrade requires retiring the previous owning
process; migration is not an ownership-stealing or replacement-owner mechanism.

## Memory and CLI

MemoryStorage keeps State and its derived catalog projection under its existing
shared mutex. Lease incarnation advances on reacquisition; CAS validates owner
and state revision before atomically replacing both. Catalog reads inspect only
the bounded projection while holding that mutex, including the same stamp and
identity checks. No full-State clone is required by a read.

The embedding Engine exposes the typed query through its actual owner context.
The standalone `inspect-catalog --acquire-idle-namespace` command explicitly acquires
an existing idle namespace and releases it; it never steals another process's live ownership.
CLI output retains one session/stamp for at most four pages;
a later process cannot silently reuse a previous incarnation's continuation.
Acquisition/one-time materialization is distinct from the bounded read itself.

## Qualification before release/pinning

Pure tests cover field/privacy allowlists, byte/cursor/query bounds, exact keys,
memory ownership/reacquisition, stable metadata stamps across unrelated State
changes and refusal after catalog changes. Protected storage tests retain the
existing sentinel guarantees, including purpose text.

Actual PostgreSQL cases cover atomic catalog/State changes and rollback, exact
definition/control updates, independent connections, ownership loss/takeover,
restart and stale continuations, missing keys/tampered metadata, rotated keys,
unmaterialized/mismatched revision refusal and finite indexed pages over large
private history. A restricted-role/no-snapshot-access case should demonstrate
that a leased catalog read needs neither snapshot nor full-artifact columns.
Actual frozen 0.1.2 constructor/already-open writer behavior is qualified
separately against the new schema. No external provider/service call is needed.

Heavy builds use a root-assigned existing cache, two jobs/debug0 and an explicit
handoff. Package candidates and extracted local qualification use new identities;
there is no remote publication, agent-q vendor edit or consumer pin before the
new component source and evidence are frozen.
