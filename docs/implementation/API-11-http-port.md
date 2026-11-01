# API-11 optional human media port

This increment connects the qualified internal media owner to the actual human
router. It adds no worker upload grant, provider call, uploader UI, external
service, arbitrary URL fetch, original-input download, or report mutation.
Existing report ownership and the PNG codec remain the sources for identity and
bytes. The schema and six paths are frozen in
[human-result-media-1](../../protocol/human-result-media/v1/README.md).

## Explicit configuration and lifetime

Absent `[result_media]`, the default daemon/router constructors mount no media
interface. A configured owner requires an existing absolute private 0700 root
and its stable nonnil `storage_id`. `uploads_enabled` defaults to false. The
configuration is closed and deliberately has no URL, provider, tunable codec,
or expanded quota surface.

`Runtime::start(AppState, &MediaConfig)` separately opens the file owner, verifies
the immutable root/marker/database binding and completes one bounded initial
cleanup pass before returning a mountable `Arc<Runtime>`. A failed or blocked
cleanup refuses startup and leaves charges retained. No owner is stored in
AppState. Maintenance uses a weak runtime reference, a 60-second skipped-tick
interval, batches of at most sixteen, and no unbounded work queue. Shutdown stops
new scheduling and awaits a cleanup already in progress; it does not abort its
physical work.

The compatible public `build_admin_router_with_optional_owners` accepts the
optional runtime after dashboard assets. Existing constructors forward `None`.
The configured main path starts once and awaits shutdown. Bootstrap read/upload
capabilities require the actual constructed and mounted owner plus current
Owner role; upload additionally requires the explicit mutation opt-in. Private
original-actor GET status remains readable when upload mutations are disabled.

## Human authority and transport

The adapter is merged beneath the existing identity resolver, Owner extractor,
CSRF Origin/custom-header guard, maintenance, admission and response-hardening
layers. It is absent from the machine listener. The owner receives the trusted
full human snapshot and rechecks id/issuer/subject/current Owner across waits and
IO. A foreign upload intent is generic 404; actual access loss remains 401/403.
Known content failures receive typed media errors only after the actual
report/revision/artifact association has been resolved. Failed IO also crosses
the final authorization fence before an error is exposed.

Admission preserves literal JSON bytes and returns transaction-derived 201 new
or 200 exact replay. Lookup never performs a reconciliation. PUT checks exact
original length/SHA before a lease and permits read-only Received/Retained replay.
Finalize requires a closed empty object and returns success only with the actual
immutable receipt. Busy, rejected and expired states preserve original intent
and deadlines. Request bodies have fixed byte bounds and a ten-second read
deadline; compressed bodies and unsupported MIME/query fields are rejected.

Media responses carry the selected exact PNG length/SHA, immutable receipt SHA,
private no-store, inline disposition, nosniff and a restrictive CSP. The shared
hardener preserves only the exact `private, no-store` policy; other responses
retain the existing forced no-store behavior. Unsupported Range and conditional
requests do not introduce a partial-content/cache surface.

Each response holds a nonqueued content admission permit. It emits at most
16 KiB per frame without copying the whole image into a queued frame. Each
`Bytes::from_owner` chunk independently owns an Arc containing verified Content
and its permit. Both survive EOF/body cancellation while a queued frame remains;
cleanup cannot release the physical charge and another request cannot consume
that permit prematurely. The transport test reconstructs the full multi-frame
PNG and checks its length/SHA, then deliberately drops the body while retaining
one frame and proves cleanup and capacity remain fenced.

## Actual qualification and browser fixture

Tests use the actual assembled admin router, current identity/CSRF/bootstrap
owners, actual AQ28 legacy report authority, PostgreSQL, private managed files
and the pinned PNG codec. They do not replace endpoints or manufacture a
successful receipt. They cover the default-disabled and explicit read/upload
mounts, current-role capability loss, original-actor opacity, header/body/query
bounds, replay/status mapping, typed known failures, failed-IO role loss, frame
lifetime and startup cleanup refusal. Earlier internal owner cases remain a
backstop; source-only review and actual browser acceptance are separate gates.

`AQ_TEST_RETAIN_MEDIA_HTTP_DEMO=1` on the dedicated fixture test retains a fresh
disposable AQ36 database and private root after actual API06/media binding
equality checks. Its original report conclusion is `AQ disposable media fixture`;
it starts with zero media intents and makes no capture or provider claim. The
0600 `/tmp/aq-media-http-demo.json` contains only nonsecret fixture identity,
root/storage binding and exact Results route. The browser probe captures a
labelled actual AQ page and performs one explicitly authorized human-supplemental
PNG upload against the isolated candidate. Live upload deadlines come from the
candidate daemon's real clock. No canonical database or configuration is changed
by this fixture.
