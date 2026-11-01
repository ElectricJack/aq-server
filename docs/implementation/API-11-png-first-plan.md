# API-11/UI-18 PNG-first implementation delta

Status: proposed source plan for review. No migration allocation, dependency
addition, filesystem write API or dashboard uploader is implemented here.
The [bounded design](../specs/result-media-png.md) and
[closed contract](../../protocol/human-result-media/v1/README.md) govern this slice.

Pure-core step 2 is now implemented and qualified in source `a3b2ead`; see
[the core-only qualification](API-11-core-qualification.md). Storage, HTTP,
authorization, migration registration and UI remain unimplemented. No pure test
is evidence that PNG bytes have already been retained by the daemon.

## Source ownership and sequence

1. Freeze schema/examples, storage/receipt identity, narrow PNG policy and quota/
   cleanup semantics. Independently review crash windows, late-writer fencing,
   result/source binding and human authorization. Root allocates migration 36 only
   after the conversation owner's 35 is frozen; do not edit existing migrations.
2. Add a small pure Rust result-media component for closed domain types and PNG
   preflight/decode/rendering. It has no SQL, HTTP, worker harness or Graph client.
   Pin and qualify one PNG decoder/encoder dependency with the known palette
   failure addressed. Unit/property cases run without the server/test database.
3. Add server `result_media` owner modules for immutable intent, file storage,
   current authorization, quota/lease transitions and bounded cleanup. Reuse the
   actual retained result read seam; do not copy result/task state into media
   tables. Proposed DDL is separate and unnumbered until root assigns it.
4. Add human routes and machine-readable schema export/tests. Root owns final
   config/bootstrap/router mounting and publishes capabilities only with the
   actual owner. No worker route, service credential propagation or producer
   simulation grants human authority.
5. Add only Results read/selector/expand UI, dedicated adapters/fixture/tests and
   a bounded binary client. Root owns shared route/bootstrap/mocks. Media and
   metadata remain separately gated; no upload button is introduced.
6. Qualify through disposable actual PostgreSQL + managed files + retained report
   owners, then build one root-composed production bundle. Upload an actual
   controlled application screenshot through the human API, finalize it and view
   exact images on desktop/phone. No provider or publication is needed.

No server Cargo or production UI build starts without the parent's shared target
and frontend slot. Source/tests freeze as bounded pairs; root performs integrated
qualification and promotion after required gates.

## Contract and transaction invariants to test

- Unknown fields, duplicate JSON keys, nil IDs, malformed report revisions,
  control/path-like filenames and oversized intent/content are refused.
- Admission resolves the actual original report/source owner. Wrong report
  digest/project/attempt, source/spec corruption and absent stopped/result owner
  cannot gain a media link. Superseded/revoked-worker historical reports remain
  readable to current authorized humans.
- Exactly one reserved operation; original raw JSON and expected input bytes
  remain immutable. Same operation with changed whitespace/target/input/capture
  conflicts. Another actor cannot read/replay the original actor's intent.
- Installation/project reservations and report cardinality are race-safe before
  any file body is accepted. Finalization changes reserved to actual physical
  bytes once. Concurrent finalize/cleanup cannot double release or overwrite.
- Loss after reservation, input fsync, render fsync or receipt commit leads to
  pure lookup or identical explicit replay. No lookup writes, no new operation
  after unknown, no fabricated retained receipt from metadata alone.
- Role loss during file/decode waits fails the final fence. Changed storage
  identity, expired intent/lease, clock regression or wrong generation blocks
  replay without renewing deadlines or reassigning old files.
- Strict RGB/RGBA8 static subset, CRC/order/trailing-byte rejection, APNG chunks,
  indexed/PLTE panic corpus, interlace, malformed deflate, output-size overflow,
  compressed bombs and pixel/dimension boundaries are covered. Verify exact
  render/preview golden bytes and separate input/variant digests.
- Reject symlink/foreign-root/path substitution. Missing or changed files do not
  serve bytes; deleted/corrupt preview does not relabel the source. No read of an
  arbitrary file or external URL is reachable. HTTP payloads are inert PNG only.
- A late processing job cannot publish after a newer lease or cleanup. Budget is
  not released until physical writers are done and owned files absent. Restart
  reconciliation keeps accounted orphans and tombstones; no directory-wide
  deletion or guessed path ownership.
- Every content read rechecks current human authorization and exact result link,
  expiry, byte length and digest. Revocation/missing/expiry/error never silently
  returns an empty successful image or a previous scope's Blob URL.

Use actual SQL/filesystem/router tests for ownership, crash recovery and quota
fences; unit mocks cannot establish these. Instrumented barriers may hold actual
IO boundaries, but must not manufacture a finalized receipt or fake image bytes.
The positive image is a real screenshot of the intended disposable AQ scenario;
its association is labelled human supplemental evidence, not worker capture proof.

## UI and visual acceptance

Use the existing Results metadata feed and exact report route. Read at most eight
media descriptors per report; lazy-load previews only for a bounded visible card
window and fetch the render on explicit expansion. No automatic feed reorder,
unbounded polling, native browser caching of private bytes or sound/video.

UI tests include exact metadata/content binding and digest, explicit capability,
available/expired/missing/withheld/failed states, identity/report/variant change
with a held old response, Object URL release, auth failure with stale bootstrap,
image selector keyboard labels, expand/close and reading position. The protected
failure fence follows the established session model; do not repeat imperative
mutation-only authorization gaps from earlier creation UI.

The actual production probe records served bundle digest, original report and
artifact/variant digests, one explicit finalized upload, content rejection after
expiry/revocation, bounded bytes/network requests, no provider calls, and separate
desktop/390px/320px screenshots. Inspect the screenshots against Results 10/13.
Report the delivered image-feed/expand subset and keep clips, comparison,
discussion, upload UI and capture-policy completion checks explicitly deferred.
