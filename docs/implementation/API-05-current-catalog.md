# Current Playbooks catalog: consumer contract increment

This source increment adopts the exact qualified component source `0173673`,
archive SHA-256 `6f8c373abacc9360c7be9618bdef3d608ac2f12d605738e711466a22fe6a1b4a`,
in new immutable `vendor/aq-playbooks-0173673`. Old vendor closures and archives
remain unchanged. Runner/PostgreSQL/CLI are exactly 0.1.3; model/compiler remain
0.1.0. The AQ embedding host becomes 0.1.3 without changing its legacy public
Request or Error enums. No canonical database/configuration, runtime namespace or
remote publication is changed by this source work.

## Protected read routes

- `GET /api/playbooks/inspection/catalog/definitions`
- `GET /api/playbooks/inspection/catalog/activations`

Both use closed query parameters: optional `project_id`, `limit` (1..25), exact
opaque SHA-256 `exact`, or `after` plus paired positive decimal `incarnation` and
`metadata_revision`. Exact and after are mutually exclusive; continuation
requires both stamp fields. Exact lookup can optionally carry that same pair.
The server reconstructs the component stamp from its actual engine namespace and
fixed route kind; callers cannot select another engine namespace. A supplied
project must match the namespace project. Current human Owner and exact namespace
owner authorization run before and after the awaited owner query. The original
trusted User ID/issuer/subject/Owner role are retained; empty identity bindings
are refused, with email and user row-version pinned across the wait as well.
Same-principal revocation/rebind returns 403 for the dashboard denial latch,
while an unrelated namespace Owner retains the existing hidden-scope 404.
Reauthorization takes precedence even if the engine query itself failed.

The closed `playbooks-current-catalog-1` response has `source` equal to
`current_namespace_state`, `current_catalog=true`, `component_api_version` equal
to `playbooks-catalog-1`, namespace identity/generation/project, observation time,
lossless decimal state revision/incarnation/metadata revision, fixed kind, typed
items and an opaque last-key cursor. Definition items expose exact artifact,
executable/source fingerprints, name/scope/compiler, bounded literal purpose and
truncation flag, entry names and declared dependency binding/reference/selection.
Activation items expose opaque key, actual binding/artifact/revision, stored
calls/events/execution controls and the actual artifact execution-denied flag.
No authority, current grant validity, readiness, activation timestamps, full
artifacts, captured values, expressions, registry schemas or run history appear.

The underlying component checks live advisory-lock ownership/incarnation/backend,
State/projection revision equality, expected/actual row counts and row AEAD. The
AQ adapter delegates to the owning Engine through a new private read envelope on
the existing dedicated owner thread. There is no full-State read, direct component
SQL, process-local current catalog or new mutation authority in this path.
The response packs typed items within 256 KiB and sets a cursor to the last
returned key if wrapper framing requires a shorter page. At most 25 entries are
read per response plus the component's one bounded lookahead. The existing two
concurrent reads, three-second route deadline, two-second owner deadline and
`Cache-Control: no-store` remain in force.

Catalog metadata changes or a successor incarnation refuse an old stamp with
409. Missing/corrupt projection, lost ownership and unsupported metadata profiles
return explicit unavailable errors, never successful empty pages. A successful
empty page means the actual current represented set is empty. Backend-specific
private diagnostics are not returned. Repeated authorization runs after await
and stamp joins check the exact response namespace/kind/continuation context.

## Capability and historical fallback

`playbooks.catalog.read@1` requires mounted catalog routes, an actually attached
owner whose initial bounded component query established support, and the current
human Owner matching the configured namespace owner. This is interface support,
not cached current metadata or a guarantee that every later profile is supported.
Bootstrap does not query the engine. Mounted state retains only a weak live
interface-support handle; atomic support/closing state is rechecked at each
bootstrap without extending owner lifetime. Acknowledged stop disables the
capability on the same existing router. Later Unsupported/corruption remains an
explicit read failure and never falls back to stale startup configuration.

The existing `playbooks.loaded-definitions.read@1` API and startup capture remain
unchanged. When current-catalog support is not negotiated, Definitions can retain
its explicitly labelled startup-configuration view. It must never relabel that
history as installed/current. Activations remains unavailable without current
catalog support. No create/install/edit/activate or run-action control is added.

## Dashboard subset and qualification gates

Keep the approved `06-playbooks.png` roster/detail composition and phone exit.
Definitions and Activations are separate views; use current metadata only when
its capability is negotiated. Definition selection, exact detail/reload, immutable
pins, declared dependencies and current activation controls have strict local
adapters. Bound loaded pages to 100 entries and retain namespace/stamp joins.
Read errors cannot display cached successful catalog data under changed context.
A feature-local JSON client bounds request/body time to five seconds, streams at
most 256 KiB before parse, and refuses duplicate keys, lossy numeric syntax and
invalid UTF-8. Fetch is same-origin, no-store and redirect:error. HTTP 401/403
headers are classified immediately; body cancellation is never awaited, so a
held error body and unavailable bootstrap cannot retain old catalog metadata.
Run pins/timeline and historical startup definitions remain separate sources.

Source tests must cover legacy-wire compatibility, actual owner delegation with
no snapshot fallback, current mutation visibility after startup, namespace/role
and malformed-query refusal, stamp changes/takeover, byte/cursor bounds and
profile-unavailable semantics. Actual protected owner/HTTP tests run in disposable
namespaces; component schema migration applies only to disposable databases.
Strict DTO/schema generation and frontend tests precede a root-built bundle and
independent actual desktop/390/320px browser review. Until those gates pass, this
is a source contract increment, not a canonical preview or full Playbooks design
acceptance. Editing, source compilation and mutation actions remain deferred.

### Actual default37 owner over real Unix PostgreSQL

The [retained real-UDS composition proof](evidence/private-uds-current-catalog-37/README.md)
passed with composed source5d445a5/default features and immutable dashboard624707…e663.
The same isolated physical cluster/database advanced AQ35→37 and component3→4,
served one unchanged inert current definition/activation, and released ownership
across graceful stop/restart. The retained expired grant was neither replaced nor
extended; current metadata and stored controls do not claim execution authority.
Actual desktop/390/320 catalog/navigation/empty-work browser checks passed3503ms
with zero browser mutations or foreign requests. All17 work/effect counts remained
zero. This is separate from the earlier synthetic-forwarding protected HTTP
fixture, and does not enable or qualify a canonical optional owner.
