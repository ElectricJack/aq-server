# Supplemental PNG evidence for an exact retained report

Status: bounded API-11/UI-18 design delta for review, November 1, 2026. No storage
implementation or migration is authorized by this document alone. Migration 35 is
reserved for conversations; the separate proposed DDL has no migration number.

## Existing owners and the selected boundary

`aq-admission::transport::ResultReport` has no media reference field. Its source
receipt/effect IDs are claims, not stored image bytes. The historical
`profiled_execution::results::load_locked` owner validates the exact report,
receipt, original attempt/source and expected-outcome requirements; the latter
are pre-execution requirements. `admission_artifacts` contains closed kinds of
execution inputs and has worker-assignment fetch authority. `source_retention`
owns Git manifests/bundles. Neither is an output-image store or a human media
grant. Frozen admission/result transport and these owners remain unchanged.

Introduce one canonical AQ result-media owner in the existing PostgreSQL
database, with bytes in one private managed local directory. An immutable
attachment receipt links a new artifact to an exact report ID/digest, project,
task, attempt and original source identity obtained from the actual result owner.
This is **supplemental evidence uploaded by a human Owner after the report**.
It does not claim the worker submitted the image, rewrite report content, certify
what the pixels depict, satisfy a capture requirement, or establish acceptance.
Multiple attachments append independent immutable receipts; no attachment is
retroactively inserted into the frozen worker report digest.

The first dashboard increment reads and expands retained images only. It adds no
uploader, worker capture grant, clip playback, comments, download, report dump,
arbitrary path/URL retrieval, review waiver or completion-policy integration.
Full API-11/UI-18 acceptance remains partial until their remaining requirements
are delivered. Managed storage is part of AQ, not a new external service.

## Authority and immutable upload intent

Human routes require current `RequireOwner`. Reading requires
`results.media.read@1`; upload admission/content/finalize additionally require
`results.media.upload@1`. Capabilities are published only after the configured
store is initialized and the paired implementation is available. Machine tokens
cannot use this API. A later machine port must bind actual attempt/generation
and capture policy; it will not borrow a browser identity.

Before reserving capacity, resolve `results::load_locked` and compare the exact
URL report digest. Derive all project/task/attempt/source fields server-side.
Upload operation UUID is also upload/artifact identity. Persist the original
bounded JSON bytes, their SHA256, original human actor, expected input digest and
size, capture assertions, storage identity and policy before accepting bytes.
Same actor/operation with different raw intent bytes or target conflicts without
revealing another actor's intent. Exact replay returns the existing intent;
it cannot renew expiry, change storage, replace the image or extend retention.

GET upload status is pure lookup.404 never proves an earlier in-flight admission
had no effect. PUT retries use the same original PNG bytes; finalize replays
only the retained intent. No response loss allocates another artifact or changes
the payload. Terminal rejection/expiry permits a deliberately new operation, not
an altered replay. The owner of an upload intent alone can inspect/replay that
intent; other current Owners may browse a finalized attachment through its report.

Recheck current human role and exact result linkage after relevant waits and
before returning a successful admission/finalization/content response. A retired
worker or expired execution grant does not erase historical report/media access.
A superseded report stays addressable at its original exact digest.

## PNG and canonical rendering policy

The first policy accepts at most 1,048,576 input bytes, dimensions 1..4096 on each
axis, and at most 2,000,000 pixels. Accept only a static, noninterlaced, 8-bit RGB
or RGBA PNG. Structural preflight permits IHDR, consecutive IDAT, optional sRGB,
and final IEND only; validates checked lengths/chunk count/order/CRC; rejects any
trailing bytes, palette, APNG acTL/fcTL/fdAT, text, profiles or other ancillary
chunks. The strict subset is declared as a limit, not general PNG compatibility.

Fully decode under the PNG decoder's resource limits and separately bounded
caller-owned buffers; IHDR/signature checks alone cannot finalize an image.
The full-raster RGBA buffer is at most 8,000,000 bytes; preview RGBA is at most
589,824 bytes. Configure a 16 MiB decoder scratch budget and bound encoding
output separately. Only two processing jobs may be active; acquire the permit
before reading a request body and hold it until all work finishes, even if an
HTTP client disconnects. No detached timed-out decoder is counted as completed.
Catch decoder panics at the job boundary and retain a typed rejection, not a
daemon crash or finalized image. The implementation must qualify actual peak
buffers and malformed/compressed-bomb cases before enabling upload capability.

`aq.png.rgba8-srgb-v1` interprets unprofiled accepted samples as sRGB, adds opaque
alpha to RGB, and preserves RGBA samples; it performs no ICC color conversion.
Write a fresh static RGBA8 PNG with sRGB marking and no copied metadata. Retain
the input privately for digest/provenance, but expose only canonical variants.
Encoder final settings are RGBA8, noninterlaced, perceptual sRGB marking,
`Compression::Fast` followed by `Filter::NoFilter`, and no optional metadata.
Pin exact dependency bytes and golden vectors; changing those settings requires
a new rendering-policy identity. These are supported by the upstream
[encoder](https://docs.rs/png/0.18.1/png/struct.Encoder.html),
[compression](https://docs.rs/png/latest/png/enum.Compression.html) and
[filter](https://docs.rs/png/latest/png/enum.Filter.html) APIs. The render
variant keeps dimensions and has an 8 MiB encoded bound. A deterministic
nearest-sample preview has a maximum 384px long edge, preserves aspect ratio,
and has a 768KiB bound. Decode/encode overflow rejects rather than truncates.

Input, render and preview have separately recorded SHA256/byte counts. Equality
of digests is allowed if bytes happen to match; one identity never silently
substitutes for another. Capture scenario/time/viewport/source claims remain
explicit human assertions beside the server-derived report/source binding.

Dependency gate: the Rust `png` crate provides decoder/encoder APIs, but its
[documented limits](https://docs.rs/png/latest/png/struct.Decoder.html) are
best-effort and exclude caller-allocated output. Do not equate them with a hard
process memory limit. The published 0.18.1 has a reported malformed-palette panic;
the [upstream fix](https://github.com/image-rs/image-png/pull/682) and
[release issue](https://github.com/image-rs/image-png/issues/721) must be checked
when pinning a patched release or narrowly vendored fix. Palette preflight and a
panic regression remain required. No dependency is added by this design commit.
The RGBA/output bounds are hard checked allocation sizes; the decoder's scratch
setting is an additional best-effort defense, not a claimed hard RSS ceiling.
The strict input/chunk subset removes arbitrary metadata expansion, and actual
peak decoder behavior remains a dependency qualification gate.

## Storage grant, quotas, commit and cleanup

Configure one absolute private root and stable storage UUID. It must not be
inside dashboard assets or an agent workspace. Initialize a 0700 owned directory
with an identity marker; use 0600 regular files, no-follow descriptor-relative
operations and server-generated operation/lease names only. Hold an exclusive
filesystem lock for the store lifetime. A different root/marker cannot silently
rebind retained intents. The local-files design does not support multiple writers
on different machines; worker nodes upload through a future authenticated port.

The durable intent is a **bounded storage grant**, never execution authority:
fixed actor/report/storage/policy/input, 30-minute upload expiry, immutable
seven-day byte retention from admission, and a 10 MiB worst-case physical
reservation (input + render + preview + bounded per-job staging). Original input
staging becomes the retained private input; rendering buffers are not extra
unaccounted disk copies. Each intent has one active physical writer.

Hard initial budgets are 256 MiB installation/64 MiB project bytes, 4096 historical
intents installation/512 project, eight retained or pending attachments per exact
report, and two active jobs. Admission locks installation budget, then project
budget, then actual result authority and intent; it reserves bytes/counts before
writing. Current reservations plus retained bytes must fit. Finalization converts
reservation into exact physical retained bytes and releases only the difference.
Config may lower limits; it cannot expand this contract's hard bounds.

No transaction spans decoding or filesystem IO. Claim a short fenced write lease,
perform bounded file work in its private generation directory, fsync files and
directory, then revalidate role/report/lease/expiry in a short transaction.
Commit the canonical immutable receipt and variant references only after durable
files exist. A crash after files but before commit leaves an accounted orphan,
not an available artifact. Repeated finalize reconciles only that same intent's
owned generation; no filename, remote URL or new bytes are accepted in finalize.
It must reuse verified owned files or confirm removal of the previous uncommitted
generation before generating replacement files. It cannot retain two physical
generations against one 10 MiB reservation. Publish the completed directory by
same-filesystem rename; this does not make a second copy of its bytes.

Persist intent states reserved, received, finalizing, retained, rejected or
expired. GET never advances them. Stale lease or response loss may require an
explicit original PUT/finalize replay. Never release a reservation while a live
writer/decoder may still write: track active jobs under the store lock. On process
restart the old writer is gone before acquiring that same lock. Metadata fencing
prevents a late worker from committing after cleanup or a newer lease.

Cleanup is bounded owner maintenance: select a small indexed batch of expired or
rejected intents/expired retained bytes, verify no active physical job, claim a
cleanup generation, unlink only that intent's known files, fsync, then record
confirmed absence and release quota. Failure retains budget and a retryable
cleanup disposition. Keep intent identity, receipt, digests, dimensions, original
association and an expired/missing tombstone after byte deletion; no physical
path is returned. External deletion/corruption produces missing/failed, not an
available image. Owner access failure never becomes missing or expiry.

There are no media holds or capture-policy consumers in this first slice. Media
must not be accepted as completion/review evidence until explicit holds/waivers
exist. Future policy integration cannot silently reinterpret this seven-day
unheld supplemental-media contract.

## Closed HTTP contract and delivery

The separate schema/example/semantics are in
`protocol/human-result-media/v1/`. Each JSON object rejects unknown properties.
JSON admission is at most 16KiB. The raw PUT is image/png, has the retained exact
length/digest, and is at most 1MiB; no multipart/range-upload surface is introduced.

| Route | First-slice behavior |
| --- | --- |
| POST `/api/human/results/{report}/revisions/{revision}/media-uploads` | Reserve immutable intent; 201 new/200 exact replay; generic 409 altered operation |
| GET `/api/human/result-media-uploads/{operation}` | Pure original-actor lookup; 200 typed state or 404 |
| PUT `/api/human/result-media-uploads/{operation}/content` | Exact whole input; 200 received/retained, 202 busy, 410 expired/422 rejected typed state |
| POST `/api/human/result-media-uploads/{operation}/finalize` | Closed `{}`; 200 retained/202 pending/409 input needed/410 expired/422 rejected typed state |
| GET `/api/human/results/{report}/revisions/{revision}/media` | Up to eight finalized attachments/tombstones with exact binding |
| GET `/api/human/results/{report}/revisions/{revision}/media/{artifact}/content?variant=preview\|render` | Current Owner, exact report/link, current retention and exact variant bytes; 200 only |

The API returns no storage paths or arbitrary content URLs. Content reads first
resolve the authoritative immutable receipt, read at most the declared variant
bound and verify digest/length, then recheck role/link/expiry before replying.
Use image/png, no-store/private, nosniff, a restrictive content security policy
and inline disposition. Original input bytes have no public content route.
Range and conditional-cache requests are outside the first slice; reject them
after authorization. No permanent signed URL or unauthenticated static serving.
Media uses its own bounded request class, separate from renewal/cancellation:
two physical processing jobs, four content reads, and no unbounded wait queue.
Content response permits remain associated with bounded body delivery/drop;
returning from a handler does not release a budget while private bytes remain
queued indefinitely for a slow client.

Availability is a current observation: available means the owned variant files
are present at expected lengths under a valid retained receipt; content retrieval
revalidates their actual digests. Expired/missing/withheld/failed descriptors keep
exact provenance and no usable content. Unknown/denied IDs do not reveal another
actor's intent or unauthorized project's metadata. First-slice withholding has
no dashboard mutation; the owner maintenance port must preserve a typed reason.

## Dashboard composition and acceptance

References: aq-dashboard `10-results-desktop-v2.png` and
`13-results-mobile-v2.png`, plus product design 0002. Preserve centered evidence
cards, prominent real media, compact image selectors and full-width phone cards.
Keep exact source/receipt identities in disclosures beneath media. Do not add
sample clips, comment counts, required-capture success, review acceptance or a
fake discussion pane. Use preview-first lazy reads and reserved aspect ratios so
new bytes do not move the reading anchor; explicitly expand a selected rendering.

Protected query keys include principal/authorization/project/report/revision/
artifact/variant/digest. Bound image fetches, validate returned bytes, revoke Blob
URLs on identity/scope/revision change or denial, and fence late replies. A missing
read capability leaves the existing metadata increment useful. Actual paired
upload/finalize/content proof and independent desktop/390px/320px screenshot
review are required before promoting this delivered subset.
